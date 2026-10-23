"""Bounded single-PC REST/event soak with real accounts, outage and durable catch-up."""
from __future__ import annotations
import argparse, collections, concurrent.futures, datetime, hashlib, importlib.util, http.client, json, pathlib, platform, socket, threading, time, urllib.parse, uuid

package = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--server', type=pathlib.Path, default=package/'dist/relay-server.exe')
parser.add_argument('--sessions', type=int, default=100)
parser.add_argument('--seconds', type=int, default=30)
parser.add_argument('--workers', type=int, default=16)
args = parser.parse_args()
assert 100 <= args.sessions <= 200 and 5 <= args.seconds <= 120 and 1 <= args.workers <= 24
server_binary = args.server.resolve()
assert server_binary.is_file()
server_sha256 = hashlib.sha256(server_binary.read_bytes()).hexdigest()
started_utc = datetime.datetime.now(datetime.timezone.utc).isoformat()
workspace = package.parents[1] if package.parent.name == 'outputs' else package
fixture = workspace/'work'/('relay-soak-'+uuid.uuid4().hex)
fixture.mkdir(parents=True)
spec = importlib.util.spec_from_file_location('relay_integration', package/'scripts/test-integration.py')
helpers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helpers)
server = helpers.Server(server_binary, fixture, helpers.unused_port(socket.SOCK_STREAM), helpers.unused_port(socket.SOCK_DGRAM))
results_lock = threading.Lock()
latencies, transient_retries = [], []

class Client:
    def __init__(self, index):
        self.index, self.token, self.user, self.ws = index, None, None, None
        self.source = (f'127.0.0.{index+2}', 0)
        self.connection = None
        self.sent_at = collections.deque(maxlen=5)
    def call(self, method, path, body=None, expected=200, measured=False):
        for attempt in range(4):
            if self.connection is None:
                self.connection = http.client.HTTPConnection('127.0.0.1', server.port, timeout=8, source_address=self.source)
            connection = self.connection
            payload = json.dumps(body).encode() if body is not None else None
            headers = {'Content-Type': 'application/json', **({'Authorization': 'Bearer '+self.token} if self.token else {})}
            start = time.perf_counter()
            try:
                connection.request(method, path, body=payload, headers=headers)
                response = connection.getresponse()
                data, status = response.read(16*1024*1024), response.status
            except (OSError, http.client.HTTPException):
                connection.close()
                self.connection = None
                raise
            duration = (time.perf_counter()-start)*1000
            if status == 503 and attempt < 3:
                with results_lock:
                    transient_retries.append(status)
                time.sleep(.025*(attempt+1))
                continue
            assert status == expected, f'{method} {path}: {status}, expected {expected}; {data[:180]!r}'
            if measured:
                with results_lock:
                    latencies.append(duration)
            return json.loads(data) if data else None
        raise AssertionError('unreachable')
    def new_message(self, body, measured=False):
        # Deliberate fixture pacing preserves the real account anti-spam rule.
        # Never consume/retry an unexpected 429 or obscure quota assertions.
        if len(self.sent_at) == 5:
            time.sleep(max(0, self.sent_at[0] + 5.1 - time.monotonic()))
        message = self.call('POST', '/api/messages', body, measured=measured)
        self.sent_at.append(time.monotonic())
        return message
    def connect(self):
        self.ws = helpers.WebSocket(server.port, self.token, bearer=True, source_address=self.source)
    def close(self):
        if self.ws:
            self.ws.close()
            self.ws = None
        if self.connection:
            self.connection.close()
            self.connection = None

clients = [Client(i) for i in range(args.sessions)]
password = 'private-synthetic-soak-password'
try:
    server.start()
    initial_health = clients[0].call('GET', '/api/health')
    for client in clients:
        body = {'username': f'Soak{client.index:03}', 'password': password}
        if client.index:
            body['invite'] = invitation
        session = client.call('POST', '/api/register', body)
        client.token, client.user = session['token'], session['user']
        if client.index == 0:
            invitation = client.call('POST', '/api/invites', {'max_uses': args.sessions, 'expires_in': 3600})['code']
        client.connect()
    admin = clients[0]
    bootstrap = admin.call('GET', '/api/bootstrap')
    channel = next(c['id'] for c in bootstrap['channels'] if c['kind'] == 'text')
    private = admin.call('POST', '/api/channels', {'name': 'private-soak', 'kind': 'text', 'visibility': 'private'})['id']
    for index in range(60):
        # Seed across real accounts; do not evade or silently retry the default
        # five-new-messages/five-seconds account limit (including administrators).
        clients[index % len(clients)].new_message({'channel_id': channel, 'content': f'needle seeded message {index}', 'client_id': f'seed-{index}'})
    cursor = admin.call('GET', '/api/bootstrap')['latest_change_sequence']
    history_path = '/api/history?'+urllib.parse.urlencode({'channel_id': channel, 'limit': 25})
    search_path = '/api/history?'+urllib.parse.urlencode({'channel_id': channel, 'search': 'needle', 'limit': 10})
    private_path = '/api/history?'+urllib.parse.urlencode({'channel_id': private})
    changes_path = '/api/changes?'+urllib.parse.urlencode({'after': cursor, 'limit': 50})
    rounds, sends = 0, 0
    soak_start = time.perf_counter()
    deadline = time.monotonic()+args.seconds
    def cycle(client):
        page = client.call('GET', history_path, measured=True)
        assert 0 < len(page['messages']) <= 25
        assert page['next_before'] is not None
        search = client.call('GET', search_path, measured=True)
        assert search['messages'] and all('needle' in message['content'] for message in search['messages'])
        changes = client.call('GET', changes_path, measured=True)
        assert all(change['channel_id'] != private for change in changes['changes']) if client.index else True
        if client.index:
            client.call('GET', private_path, expected=403, measured=True)
        # Keep every event consumer active without blocking on empty queues.
        client.ws.events(.001)
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        while time.monotonic() < deadline:
            list(pool.map(cycle, clients))
            author = clients[1+(rounds % (args.sessions-1))]
            author.new_message({'channel_id': channel, 'content': f'needle live round {rounds}', 'client_id': f'round-{rounds}'}, measured=True)
            sends += 1
            rounds += 1
            time.sleep(.025)
    actual_seconds = time.perf_counter()-soak_start
    # Close event sockets, persist unseen changes, force a server outage, then reopen
    # the same accounts without minting/replacing sessions or injecting DB rows.
    cursor = admin.call('GET', '/api/bootstrap')['latest_change_sequence']
    for client in clients:
        client.close()
    gap_body = {'channel_id': channel, 'content': 'needle unseen before outage', 'client_id': 'outage-durable-id'}
    gap = admin.new_message(gap_body)
    dm = admin.new_message({'recipient_id': clients[1].user['id'], 'content': 'private missed DM', 'client_id': 'outage-dm-id'})
    server.stop()
    try:
        admin.call('GET', history_path)
        raise AssertionError('owned stopped server unexpectedly served a request')
    except (OSError, http.client.HTTPException):
        pass
    server.start()
    restarted_health = admin.call('GET', '/api/health')
    assert restarted_health['server_id'] == initial_health['server_id']
    assert restarted_health['instance_id'] != initial_health['instance_id']
    retry = admin.call('POST', '/api/messages', gap_body)
    assert retry['id'] == gap['id'], 'idempotent retry after outage duplicated a message'
    for client in clients:
        changes = client.call('GET', '/api/changes?'+urllib.parse.urlencode({'after': cursor, 'limit': 200}))
        ids = {change['message_id'] for change in changes['changes']}
        assert gap['id'] in ids
        assert (dm['id'] in ids) == (client.index in (0, 1)), 'catch-up leaked private DM'
        page = client.call('GET', history_path)
        assert sum(message['id'] == gap['id'] for message in page['messages']) == 1
        client.connect()
    connected_message = admin.new_message({'channel_id': channel, 'content': 'reconnected delivery', 'client_id': 'reconnected-id'})
    for client in clients:
        notices = client.ws.events(.025)
        assert any(notice['kind'] == 'messages' and notice.get('channel_id') == channel for notice in notices), 'reconnected socket missed public delivery'
    second_dm = admin.new_message({'recipient_id': clients[1].user['id'], 'content': 'private reconnected delivery', 'client_id': 'reconnected-dm'})
    for client in clients:
        notices = client.ws.events(.025)
        assert any(notice['kind'] == 'messages' and notice.get('channel_id') is None for notice in notices) == (client.index in (0, 1)), 'reconnected DM event audience mismatch'
    ordered = sorted(latencies)
    def percentile(fraction):
        return round(ordered[min(len(ordered)-1, int((len(ordered)-1)*fraction))], 3)
    result = {'scope': 'single Windows PC, localhost, synthetic REST/chat/event traffic; no voice/video devices',
              'started_utc': started_utc, 'platform': platform.platform(), 'server_sha256': server_sha256,
              'sessions': args.sessions, 'distinct_accounts': args.sessions, 'simultaneous_bearer_websockets': args.sessions,
              'http_workers': args.workers, 'requested_soak_seconds': args.seconds, 'actual_soak_seconds': round(actual_seconds, 3),
              'rounds': rounds, 'measured_requests': len(latencies), 'live_messages': sends,
              'http_latency_ms': {'p50': percentile(.5), 'p95': percentile(.95), 'p99': percentile(.99), 'max': round(max(ordered), 3)},
              'http_latency_scope': 'successful HTTP attempts only; deliberate fixture pacing and transient503 retry attempts excluded',
              'fixture_new_sends_deliberately_paced': True,
              'transient_503_retries': len(transient_retries), 'permanent_request_failures': 0,
              'outage_reconnect_same_sessions': True, 'durable_catchup_dm_privacy': True,
              'idempotent_retry_after_restart': True, 'reconnected_ws_public_and_dm_delivery': True}
    (fixture/'result.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
    print(json.dumps(result, indent=2))
    print('Evidence:', fixture/'result.json')
finally:
    for client in clients:
        client.close()
    server.stop()
