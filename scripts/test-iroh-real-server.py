"""Real relay-server (built-in network mode) + relay-iroh as host + relay-iroh as friend.

Usage: python scripts/test-iroh-real-server.py <cargo target dir> <relay-iroh.exe (a debug build)>
The target directory must hold debug/relay-server.exe.

Everything runs on this PC over iroh's loopback mode, in a scratch folder, on
ports 14580-14582. It checks the contract with the real server: the ingress
capability, the host-assigned peer address, login through the tunnel, and that
private paths stay private.
"""
import json, os, subprocess, sys, time, secrets, shutil, tempfile, urllib.request, urllib.error
from pathlib import Path

target = Path(sys.argv[1])
server_exe = target / 'debug' / 'relay-server.exe'
helper_exe = Path(sys.argv[2])
work = Path(tempfile.mkdtemp(prefix='relay-real-check-'))
data = work / 'server-data'
data.mkdir()
procs = []
results = []


def check(name, ok, detail=''):
    results.append((name, ok, detail))
    print(('PASS ' if ok else 'FAIL ') + name + (f'  [{detail}]' if detail and not ok else ''))


def http(method, url, body=None, headers=None, token=None, timeout=20):
    data_bytes = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(url, data=data_bytes, method=method)
    for k, v in (headers or {}).items():
        request.add_header(k, v)
    if token:
        request.add_header('Authorization', 'Bearer ' + token)
    if body is not None:
        request.add_header('Content-Type', 'application/json')
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(request, timeout=timeout) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw[:1] in (b'{', b'[') else raw)
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except Exception:
            return e.code, raw


def start_helper(name, secret):
    env = dict(os.environ, RELAY_IROH_CONTROL_TOKEN=secret, RELAY_IROH_NETWORK='loopback')
    state = work / name
    p = subprocess.Popen([str(helper_exe), '--state-dir', str(state)], env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    procs.append(p)
    line = p.stdout.readline()
    return json.loads(line)['control_url']


try:
    password = 'Check-' + secrets.token_hex(8)
    env = dict(os.environ, RELAY_ADMIN_PASSWORD=password)
    subprocess.run([str(server_exe), '--data', str(data), '--init-admin', 'checkadmin'], env=env, check=True, capture_output=True)
    server = subprocess.Popen([str(server_exe), '--bind', '127.0.0.1', '--port', '14580', '--udp-port', '14581',
                               '--network-peer-port', '14582', '--data', str(data), '--name', 'Real Check'],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    procs.append(server)
    for _ in range(100):
        try:
            status, body = http('GET', 'http://127.0.0.1:14580/api/health', timeout=2)
            if status == 200:
                break
        except Exception:
            time.sleep(0.2)
    check('the real server starts in built-in network mode', status == 200 and isinstance(body, dict) and body.get('ok'))
    server_id = body['server_id']
    runtime = json.loads((data / 'server-runtime.json').read_text())
    ingress = runtime['network_ingress_token']
    peer_port = runtime['network_peer_port']
    check('the server published its peer port and capability', peer_port == 14582 and len(ingress) >= 32)

    # The peer listener itself refuses anything without the capability.
    status, _ = http('GET', f'http://127.0.0.1:{peer_port}/api/health')
    check('the peer listener refuses a request without the capability', status in (401, 403), status)

    host_secret = secrets.token_hex(24)
    friend_secret = secrets.token_hex(24)
    host_control = start_helper('host-helper', host_secret)
    friend_control = start_helper('friend-helper', friend_secret)

    def host_call(path, body=None):
        return http('POST' if body is not None or path != '/status' else 'GET', host_control + path, body or {}, token=host_secret)

    def friend_call(path, body=None):
        return http('POST', friend_control + path, body or {}, token=friend_secret)

    status, started = host_call('/host', {'peer_port': peer_port, 'ingress_token': ingress, 'udp_port': 14581})
    check('the host helper starts against the real server', status == 200 and len(started.get('endpoint_id', '')) == 64, started)
    route = {'endpoint_id': started['endpoint_id'], 'token': started['token'], 'direct': started['direct']}
    status, opened = friend_call('/route', route)
    check('the friend helper opens a route with the invitation token', status == 200, opened)
    base = opened['http_url']

    status, health = http('GET', base + '/api/health')
    check('the real community answers through iroh with its own identity', status == 200 and health.get('server_id') == server_id, health)

    # A friend who forges the host's identity headers gets nothing extra.
    status, health2 = http('GET', base + '/api/health', headers={'X-Relay-Network-Peer': '1.2.3.4', 'X-Relay-Network-Ingress': 'forged', 'X-Forwarded-For': '127.0.0.1'})
    check('forged identity headers do not break or change the answer', status == 200 and health2.get('server_id') == server_id)

    # Real login through the tunnel (the server rate-limits by the host-assigned peer address).
    status, session = http('POST', base + '/api/login', {'username': 'checkadmin', 'password': password})
    check('a real login works through iroh', status == 200 and 'token' in session, session)
    token = session['token']
    status, bootstrap = http('GET', base + '/api/bootstrap', token=token)
    check('an authenticated request works through iroh', status == 200 and bootstrap.get('server_id') == server_id, str(bootstrap)[:200])
    status, _ = http('POST', base + '/api/login', {'username': 'checkadmin', 'password': 'wrong-password-123'})
    check('a wrong password is refused by the real server through iroh', status in (400, 401, 403), status)

    # Private paths never reach the server.
    for path in ('/host-secret', '/api/local-management/x', '/localapi/v0/status', '/index.html'):
        status, _ = http('GET', base + path)
        check(f'{path} is refused at the host', status in (400, 403, 404), status)
    status, _ = http('GET', base + '/api/health', headers={'Origin': 'https://evil.example'})
    check('a browser request is refused at the host', status == 403, status)

    status, path = friend_call('/path', {'endpoint_id': started['endpoint_id']})
    check('the path is reported as direct with a delay', status == 200 and path.get('kind') == 'direct', path)

    # A wrong token is refused before anything reaches the server.
    status, _ = friend_call('/route', dict(route, token='A' * 43))
    check('a wrong token never opens a route', status == 502, status)

    # Rotating the token locks out the old invitation.
    status, rotated = host_call('/rotate-token', {})
    check('the host can replace the token', status == 200)
    third_secret = secrets.token_hex(24)
    third = start_helper('third-helper', third_secret)
    status, _ = http('POST', third + '/route', route, token=third_secret)
    check('a friend with the old token is refused after rotation', status == 502, status)
    status, _ = http('POST', third + '/route', dict(route, token=rotated['token']), token=third_secret)
    check('a friend with the new token is admitted', status == 200, status)

    status, _ = host_call('/unhost', {})
    check('the host stops sharing cleanly', status == 200)
finally:
    for p in procs:
        try:
            p.terminate()
        except Exception:
            pass
    time.sleep(0.5)
    for p in procs:
        try:
            p.kill()
        except Exception:
            pass
    shutil.rmtree(work, ignore_errors=True)

failed = [r for r in results if not r[1]]
print(f'\n{len(results) - len(failed)} of {len(results)} checks passed')
sys.exit(1 if failed else 0)
