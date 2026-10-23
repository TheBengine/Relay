"""Explicit disposable local RTC quality/recovery check; no devices or public host.

The public Friends admission/TURN path remains a separate opt-in. This exercises
the same generated Peer/QualityProbe with isolated community admission and keys.
"""
import argparse
import datetime as dt
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid

sys.path.insert(0, str(Path(__file__).absolute().parent))
from dev_profile_core import direct, native_process, native_shell
import owned_turn_quality_fixture as turn_fixture
from owned_turn_tcp_cut import OwnedTurnCut

SOURCE = direct(Path(__file__).absolute().parent.parent, False)
TEST = 'rtc::people_tests::loopback_tests::actual_loopback_native_quality_observations_and_recovery'
PURPOSE = 'relay-owned-loopback-media-quality-v1'
SFU_SHA = 'f71a9403935689428aca13e4fa3c2cc0ecb930f75e579ea4e46e772444347b78'

def digest(path):
    with direct(path, False).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()

def json_read(path):
    return json.loads(direct(path, False).read_text(encoding='utf-8-sig'))

def json_write(path, record):
    with direct(path).open('xb') as stream:
        stream.write((json.dumps(record, indent=2) + '\n').encode())

def artifact(path, target, test):
    record = json_read(path)
    executable = direct(Path(record['artifact']['executable']), False)
    if (record['artifact']['target']['name'] != target
            or record['artifact']['profile']['test'] is not test or digest(executable) != record['sha256']):
        raise ValueError('Wrong or changed compiler artifact')
    current = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=SOURCE, text=True).strip()
    if record['source_revision'] != current:
        # A client/QA-only checkpoint does not change the already compiled
        # server. Retain its actual source revision rather than rewriting it.
        changed = subprocess.check_output(['git', 'diff', '--name-only', record['source_revision'], current],
                                          cwd=SOURCE, text=True).splitlines()
        harmless = lambda path: path.startswith('docs/') or path in ('scripts/test-loopback-media-quality.py', 'scripts/owned_turn_quality_fixture.py', 'scripts/owned_turn_tcp_cut.py') or (not test and path.startswith('crates/relay-client/'))
        if not changed or any(not harmless(path) for path in changed):
            raise ValueError('Runtime source changed after compilation')
    return record, executable

def preserve(executable, destination):
    with direct(executable, False).open('rb') as incoming, direct(destination).open('xb') as outgoing:
        while data := incoming.read(1024 * 1024):
            outgoing.write(data)
    if digest(executable) != digest(destination):
        raise ValueError('Owned artifact copy differs')
    return destination

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *unused):
        return None

class Api:
    def __init__(self, port):
        self.base = f'http://127.0.0.1:{port}'
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())

    def request(self, method, path, token=None, body=None):
        headers = {'Content-Type': 'application/json'} if body is not None else {}
        if token:
            headers['Authorization'] = 'Bearer ' + token
        request = urllib.request.Request(self.base + path, headers=headers, method=method,
            data=None if body is None else json.dumps(body).encode())
        try:
            response = self.opener.open(request, timeout=8)
        except urllib.error.HTTPError as error:
            raise ValueError('Owned loopback HTTP status ' + str(error.code)) from None
        with response:
            raw = response.read(65_537)
            if len(raw) > 65_536:
                raise ValueError('Owned response exceeds bound')
            return json.loads(raw) if raw else None

def ports(count=5):
    selected = []
    while len(selected) < count:
        # Reserve/check both protocols for each candidate; release only after all
        # ports are selected. The launcher also fails closed if another owner wins.
        tcp = socket.socket()
        tcp.bind(('127.0.0.1', 0))
        value = tcp.getsockname()[1]
        udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            udp.bind(('127.0.0.1', value))
        except OSError:
            tcp.close(); udp.close(); continue
        selected.append((value, tcp, udp))
    values = [item[0] for item in selected]
    for _, tcp, udp in selected:
        tcp.close(); udp.close()
    return values

def owned_media_addresses(owner, udp, tcp):
    """Read-only OS proof that the selected local SFU owns its media sockets."""
    before = native_process(owner['pid'])
    if before != {key:owner[key] for key in ('pid','created','exe')}:
        raise ValueError('Owned SFU generation changed before socket check')
    code = ("$ErrorActionPreference='Stop'; "
        f"$u=@(Get-NetUDPEndpoint -LocalPort {int(udp)}); "
        f"$t=@(Get-NetTCPConnection -State Listen -LocalPort {int(tcp)}); "
        f"if(-not $u.Count -or -not $t.Count -or @($u+$t | Where-Object {{$_.OwningProcess -ne {int(owner['pid'])}}}).Count){{throw 'Wrong socket owner'}}; "
        "@((Get-NetIPAddress -AddressState Preferred).IPAddress) | ConvertTo-Json -Compress")
    result = subprocess.run([str(native_shell()),'-NoProfile','-NonInteractive','-Command',code],
        capture_output=True, timeout=15, creationflags=subprocess.CREATE_NO_WINDOW)
    if result.returncode or len(result.stdout) > 16_384:
        raise ValueError('Owned local media socket/interface proof unavailable')
    values = json.loads(result.stdout.decode('utf-8-sig'))
    if not isinstance(values,list) or not 1 <= len(values) <= 128:
        raise ValueError('Unexpected owned interface snapshot')
    addresses = {str(ipaddress.ip_address(value)) for value in values}
    if any(ipaddress.ip_address(value).is_unspecified or ipaddress.ip_address(value).is_multicast for value in addresses):
        raise ValueError('Not a unicast local interface snapshot')
    if native_process(owner['pid']) != before:
        raise ValueError('Owned SFU generation changed during socket check')
    return sorted(addresses)

def run(args):
    server_record, original_server = artifact(args.server_artifact, 'relay-server', False)
    client_record, original_client = artifact(args.client_artifact, 'relay', True)
    sfu = direct(args.sfu, False)
    candidate = credential_helper = None
    if args.turn_candidate is not None:
        candidate, credential_helper = turn_fixture.candidate(args.turn_candidate, sfu)
    if args.transport_cut and candidate is None:
        raise ValueError('Transport cut requires the explicit owned TURN candidate')
    sfu_sha = digest(sfu)
    if candidate is None and sfu_sha != SFU_SHA:
        raise ValueError('Use the reviewed SFU .5 Windows build')
    root = direct(args.work_root)
    if root.exists():
        raise ValueError('Use a new owned work directory; retain previous attempts')
    root.mkdir()
    server = preserve(original_server, direct(root / 'relay-server.exe'))
    client = preserve(original_client, direct(root / 'relay-native-quality-tests.exe'))
    inventory = subprocess.check_output([str(client), '--list'], cwd=SOURCE, text=True, timeout=25)
    if TEST + ': test' not in inventory:
        raise ValueError('Native quality fixture is missing')
    env = os.environ.copy()
    for key, folder in [('TEMP', 'temp'), ('TMP', 'temp'), ('RELAY_CONFIG_DIR', 'owned-config'),
                        ('RELAY_TEST_WORK_ROOT', 'owned-fixtures'), ('RELAY_SERVER_DATA_DIR', 'data')]:
        directory = direct(root / folder)
        directory.mkdir(exist_ok=True)
        env[key] = str(directory)
    # No unrelated developer credentials/config enter these disposable services.
    for key in tuple(env):
        if key.startswith(('RELAY_RTC_', 'RELAY_DISCORD_', 'RELAY_ACCOUNT_')):
            env.pop(key)
    env['RUST_BACKTRACE'] = '0'
    selected_ports = ports(7 if args.transport_cut else 6 if candidate else 5)
    port, udp, sfu_port, sfu_tcp, sfu_udp = selected_ports[:5]
    turn_port = selected_ports[5] if candidate else None
    backend_port = selected_ports[6] if args.transport_cut else turn_port
    turn_address = turn_fixture.owned_local_ipv4() if candidate else None
    data = direct(root / 'data', False)
    shell = native_shell()
    command_log = direct(root / 'commands.log')
    record = {'schema': 1, 'purpose': PURPOSE, 'run_id': str(uuid.uuid4()),
        'source_revision': client_record['source_revision'], 'server_source_revision': server_record['source_revision'],
        'controller_source_revision': subprocess.check_output(['git','rev-parse','HEAD'],cwd=SOURCE,text=True).strip(),
        'server_runtime_sources_unchanged_since_compilation': True, 'client_runtime_sources_unchanged_since_compilation': True,
        'started_at': dt.datetime.now(dt.timezone.utc).isoformat(),
        'client_sha256': client_record['sha256'], 'server_sha256': server_record['sha256'], 'sfu_sha256': sfu_sha,
        'scope': 'One Windows PC, owned local community/SFU, generated fixture keys and media; not global Friends authentication, public TURN/TLS, physical devices, listening or WAN acceptance.',
        'actual_native_test_started': False, 'cleanup_confirmed': False, 'passed': False}
    owned = []
    heartbeat_stop = threading.Event()
    heartbeat = None
    heartbeat_state = {'successful_authenticated_updates': 0, 'error_type': None}
    turn_observations = []
    proxy = None
    api = Api(port)
    try:
        launcher = direct(SOURCE/'scripts/launch-stack.ps1',False)
        if candidate:
            launcher, setup = turn_fixture.launcher(root, SOURCE, backend_port, turn_address)
            record['owned_turn_tcp'] = {**setup, 'candidate_version': candidate['version'],
                'candidate_manifest_sha256': digest(args.turn_candidate)}
            record['scope'] = ('One Windows PC; generated E2EE sources through an explicit '
                'same-host TURN TCP backend, without TLS/proxy. Not public Friends admission, TLS/Funnel, '
                'downstream throttling, physical devices, listening or WAN acceptance.')
            if args.transport_cut:
                record['scope'] = ('One Windows PC; generated E2EE sources through an owned same-host '
                    'plain TCP relay and byte-forwarding cut fixture. Actual TCP streams are closed, '
                    'without SDK reconnect simulation, TLS/Funnel, physical devices or WAN acceptance.')
        with command_log.open('xb') as log:
            launch = [str(shell), '-NoLogo', '-NoProfile', '-NonInteractive', '-File', str(launcher),
                '-DataDirectory', str(data), '-ServerBinary', str(server), '-SfuBinary', str(sfu),
                '-Port', str(port), '-UdpPort', str(udp), '-SfuPort', str(sfu_port), '-SfuTcpPort', str(sfu_tcp), '-SfuUdpPort', str(sfu_udp)]
            result = subprocess.run(launch, cwd=SOURCE, env=env, stdout=log, stderr=subprocess.STDOUT,
                                    timeout=45, creationflags=subprocess.CREATE_NO_WINDOW)
        record['launcher_exit_code'] = result.returncode
        if result.returncode:
            raise ValueError('Owned stack startup failed; retain commands.log')
        stack = json_read(data/'stack-instance.json')
        for role, key, expected in [('server', 'relay_pid', server), ('sfu', 'sfu_pid', sfu)]:
            live = native_process(int(stack[key]))
            if not live or os.path.normcase(live['exe']) != os.path.normcase(str(expected)):
                raise ValueError('Owned process identity changed')
            owned.append({'role': role, **live})
        supervisor = json_read(data/'stack-supervisor.json')
        live = native_process(int(supervisor['pid']))
        if not live:
            raise ValueError('Owned supervisor is not live')
        owned.append({'role': 'supervisor', **live})
        record['owned_processes'] = owned
        media_owner = next(row for row in owned if row['role']=='sfu')
        media_addresses = owned_media_addresses(media_owner,sfu_udp,sfu_tcp)
        record['owned_media_socket_proof'] = {'confirmed_before':True,'local_interface_count':len(media_addresses)}
        if candidate:
            if turn_address not in media_addresses:
                raise ValueError('TURN listener interface not in owned socket snapshot')
            record['owned_turn_tcp']['listener_confirmed_before'] = turn_fixture.verify_listener(media_owner, backend_port, turn_address)
            if args.transport_cut:
                proxy = OwnedTurnCut(root, record['run_id'], turn_address, turn_port, backend_port)
                controller_owner = native_process(os.getpid())
                record['owned_tcp_cut_listener'] = {
                    'controller_process': controller_owner,
                    'listener_confirmed_before': turn_fixture.verify_listener(controller_owner, turn_port, turn_address)}
            turn_api = Api(sfu_port)
            turn_observations.append(turn_api.request('GET', '/relay/turn-status'))
        health = api.request('GET', '/api/health')
        if health.get('setup_required') is not True or health.get('server_id') != stack['server_id']:
            raise ValueError('Not the empty owned server')
        users = {}
        for name in ['alice', 'bob', 'eve']:
            body = {'username': name, 'password': secrets.token_urlsafe(24)}
            if users:
                body['invite'] = api.request('POST', '/api/invites', users['alice']['session']['token'], {})['code']
            users[name] = {'session': api.request('POST', '/api/register', body=body)}
        admin = users['alice']['session']['token']
        bootstrap = api.request('GET', '/api/bootstrap', admin)
        room = next(channel['id'] for channel in bootstrap['channels'] if channel['kind'] == 'voice')
        other = api.request('POST', '/api/channels', admin, {'name': 'isolated', 'kind': 'voice'})['id']
        fixture = direct(root/'private-quality-tickets.json')
        fixture_record = {'schema': 1, 'purpose': PURPOSE, 'run_id': record['run_id'],
            'server_id': health['server_id'], 'base': api.base, 'sfu_udp_port': sfu_udp, 'sfu_tcp_port':sfu_tcp,
            'owned_media_addresses':media_addresses, 'room_id': room, 'other_id': other, 'users': users}
        if candidate:
            fixture_record['turn_tcp'] = {'port': turn_port, 'owned_host_ipv4': turn_address,
                'credentials': turn_fixture.credentials(credential_helper, data, record['run_id'])}
        if args.transport_cut:
            fixture_record['recovery_mode'] = 'owned_turn_tcp_close'
        json_write(fixture, fixture_record)
        env['RELAY_LOCAL_QUALITY_FIXTURE'] = str(fixture)
        def keep_owned_admissions_live():
            # Native Room media does not itself send Relay's voice control
            # heartbeat. Use the normal authenticated endpoint, without
            # extending any server deadline or changing authorization checks.
            while not heartbeat_stop.wait(10):
                for name in ('alice', 'bob'):
                    if heartbeat_stop.is_set():
                        return
                    try:
                        api.request('POST','/api/voice/state',users[name]['session']['token'],
                            {'muted':False,'deafened':False,'sharing':True,'camera':True})
                        heartbeat_state['successful_authenticated_updates'] += 1
                    except Exception as error:
                        heartbeat_state['error_type'] = type(error).__name__
                        return
                if candidate:
                    try:
                        turn_observations.append(turn_api.request('GET', '/relay/turn-status'))
                    except Exception as error:
                        heartbeat_state['error_type'] = type(error).__name__
                        return
        heartbeat = threading.Thread(target=keep_owned_admissions_live, name='owned-voice-presence')
        heartbeat.start()
        record['actual_native_test_started'] = True
        seconds = 180 if args.transport_cut else 120
        print(f'OWNED_NATIVE_QUALITY_RUNNING: generated sources, {seconds} seconds, one owned recovery; no devices/public service.', flush=True)
        with direct(root/'native.log').open('xb') as log:
            result = subprocess.run([str(client), '--exact', TEST, '--ignored', '--nocapture', '--test-threads=1'],
                cwd=SOURCE, env=env, stdout=log, stderr=subprocess.STDOUT, timeout=260 if args.transport_cut else 200, creationflags=subprocess.CREATE_NO_WINDOW)
        record['native_exit_code'] = result.returncode
        record['owned_media_socket_proof']['confirmed_after'] = owned_media_addresses(media_owner,sfu_udp,sfu_tcp)==media_addresses
        if candidate:
            record['owned_turn_tcp']['listener_confirmed_after'] = turn_fixture.verify_listener(media_owner, backend_port, turn_address)
            turn_observations.append(turn_api.request('GET', '/relay/turn-status'))
            record['turn_forwarding_observed'] = any(
                item.get('schema') == 3 and item.get('relay_udp_reads',0) > 100
                and item.get('relay_udp_writes',0) > 100 and item.get('quota_tracking_enabled') is True
                for item in turn_observations)
        if proxy:
            record['owned_tcp_cut_listener']['listener_confirmed_after'] = turn_fixture.verify_listener(controller_owner, turn_port, turn_address)
            snapshot = proxy.snapshot()
            record['tcp_cut_before_cleanup'] = snapshot
            record['actual_cut_and_new_duplex_streams'] = (snapshot['error_type'] is None
                and snapshot['cut_applied'] and sum(row['created_after_cut'] and row['upstream_bytes'] > 0
                    and row['downstream_bytes'] > 0 for row in snapshot['connections']) >= 2)
        report = direct(root/'native-quality-loopback.json')
        if report.exists():
            if report.stat().st_size > 65_536:
                raise ValueError('Native quality report exceeds bound')
            record['native_report'] = json_read(report)
        record['local_generated_scenario_passed'] = (result.returncode == 0
            and (not args.transport_cut or record.get('actual_cut_and_new_duplex_streams') is True)
            and record['owned_media_socket_proof']['confirmed_after']
            and (not candidate or (record['owned_turn_tcp']['listener_confirmed_after']
                and record.get('turn_forwarding_observed') is True))
            and heartbeat_state['error_type'] is None and heartbeat_state['successful_authenticated_updates'] >= 2
            and record.get('native_report',{}).get('local_generated_scenario_passed') is True)
    except Exception as error:
        record['error_type'] = type(error).__name__
        print('OWNED_NATIVE_QUALITY_STOPPED: ' + type(error).__name__, flush=True)
    finally:
        heartbeat_stop.set()
        if heartbeat is not None:
            heartbeat.join(timeout=10)
        record['voice_presence'] = {**heartbeat_state, 'thread_stopped': heartbeat is None or not heartbeat.is_alive()}
        if candidate:
            record['turn_observations'] = turn_observations
        if proxy:
            try:
                record['tcp_cut_after_cleanup'] = proxy.close()
            except Exception as error:
                record['proxy_cleanup_error_type'] = type(error).__name__
                record['tcp_cut_after_cleanup'] = {'all_threads_stopped': False, 'active_connections': None}
        # Stop only this fresh stack through its checked runtime identity. Never
        # kill by process name or touch the installed app/personal server data.
        try:
            if direct(data/'server-runtime.json').exists():
                with direct(root/'shutdown.log').open('xb') as log:
                    stopped = subprocess.run([str(server), '--data', str(data), '--stop'], cwd=SOURCE, env=env,
                        stdout=log, stderr=subprocess.STDOUT, timeout=20, creationflags=subprocess.CREATE_NO_WINDOW)
                record['shutdown_exit_code'] = stopped.returncode
            deadline = time.monotonic() + 30
            remaining = list(owned)
            identity_retries = 0
            while remaining and time.monotonic() < deadline:
                pending = []
                for row in remaining:
                    try:
                        live = native_process(row['pid'])
                    except RuntimeError:
                        # Exiting processes may briefly retain a PID while their
                        # image cannot be queried. Keep it unknown and retry;
                        # never convert access denial into proven termination.
                        identity_retries += 1
                        pending.append(row)
                        continue
                    if live is not None and live['created'] == row['created'] and live['exe'] == row['exe']:
                        pending.append(row)
                remaining = pending
                if remaining:
                    time.sleep(0.1)
            record['cleanup_identity_retries'] = identity_retries
            record['cleanup_confirmed'] = bool(owned) and not remaining
            record['remaining_owned_roles'] = [row['role'] for row in remaining]
        except Exception as error:
            record['cleanup_error_type'] = type(error).__name__
        record['passed'] = (record.get('local_generated_scenario_passed') is True and record['cleanup_confirmed']
            and record['voice_presence']['thread_stopped'] and (proxy is None
                or record['tcp_cut_after_cleanup']['all_threads_stopped'] and record['tcp_cut_after_cleanup']['active_connections'] == 0))
        record['completed_at'] = dt.datetime.now(dt.timezone.utc).isoformat()
        record['artifact_hashes'] = {name: digest(root/name) for name in ['commands.log','native.log','native-quality-loopback.json','shutdown.log'] if direct(root/name).is_file()}
        json_write(root/'result.json', record)
        print(json.dumps({'passed': record['passed'], 'native_exit_code': record.get('native_exit_code'),
            'local_quality': record.get('native_report',{}).get('per_source_quality_observations',{}).get('scenario_assessment'),
            'cleanup_confirmed': record['cleanup_confirmed']}), flush=True)
    return 0 if record['passed'] else 1

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--server-artifact', type=Path, required=True)
    parser.add_argument('--client-artifact', type=Path, required=True)
    parser.add_argument('--sfu', type=Path, required=True)
    parser.add_argument('--work-root', type=Path, required=True)
    parser.add_argument('--turn-candidate', type=Path,
        help='Explicit owned candidate manifest: force same-host TURN TCP, with no TLS or public host')
    parser.add_argument('--transport-cut', action='store_true',
        help='Close only this fixture\'s actual owned TURN TCP streams after 70 seconds; no SDK simulation')
    sys.exit(run(parser.parse_args()))
