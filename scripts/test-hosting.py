"""Exercise the actual local stack, private RTC gateway, and owned failure cleanup."""
from __future__ import annotations
import argparse, ctypes, json, os, pathlib, shutil, socket, subprocess, time, urllib.error, urllib.request, uuid

package = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--server', type=pathlib.Path, default=package/'dist/relay-server.exe')
parser.add_argument('--sfu', type=pathlib.Path, default=package/'dist/livekit-server.exe')
args = parser.parse_args()
server, sfu = args.server.resolve(), args.sfu.resolve()
workspace = package.parents[1] if package.parent.name == 'outputs' else package
fixture = workspace/'work'/('host-stack-test-'+uuid.uuid4().hex)
fixture.mkdir(parents=True)
shell = shutil.which('powershell.exe') or shutil.which('pwsh')
assert os.name == 'nt' and shell and server.is_file() and sfu.is_file(), 'Windows server/SFU binaries are required'
log = open(fixture/'helper.log', 'w', encoding='utf-8')

def ps_quote(value):
    return "'"+str(value).replace("'", "''")+"'"

def run_ps(code, env=None):
    return subprocess.run([shell, '-NoProfile', '-ExecutionPolicy', 'Bypass', '-Command', code],
                          stdout=log, stderr=log, timeout=25, env=env).returncode

def alive(pid):
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.OpenProcess.restype = ctypes.c_void_p
    handle = kernel.OpenProcess(0x1000, False, pid)
    if not handle:
        return False
    try:
        code = ctypes.c_ulong()
        return bool(kernel.GetExitCodeProcess(ctypes.c_void_p(handle), ctypes.byref(code))) and code.value == 259
    finally:
        kernel.CloseHandle(ctypes.c_void_p(handle))

def paths_regression():
    env = os.environ.copy()
    for name in ('RELAY_CONFIG_DIR', 'RELAY_SERVER_DATA_DIR'):
        env.pop(name, None)
    env['LOCALAPPDATA'], env['APPDATA'] = str(fixture/'local'), str(fixture/'roaming')
    legacy = fixture/'roaming/Relay/Relay/config/server-data'
    legacy.mkdir(parents=True)
    sentinel = legacy/'relay.sqlite3'
    sentinel.write_bytes(b'earlier-community-must-stay-in-place')
    helper = '. '+ps_quote(package/'scripts/hosting-paths.ps1')+'; '
    invocation = 'Resolve-RelayDataDirectory -PackageRoot '+ps_quote(fixture/'fake-package')+' -CheckLegacy'
    assert run_ps(helper+invocation, env) != 0, 'default launcher must identify older community'
    assert not (fixture/'local/Relay/server-data').exists()
    assert sentinel.read_bytes() == b'earlier-community-must-stay-in-place'
    env['RELAY_SERVER_DATA_DIR'] = str(fixture/'explicit environment')
    assert run_ps(helper+'if(('+invocation+') -ne '+ps_quote(env['RELAY_SERVER_DATA_DIR'])+"){throw 'Environment override ignored'}", env) == 0
    explicit = fixture/'explicit argument'
    assert run_ps(helper+'if((Resolve-RelayDataDirectory -DataDirectory '+ps_quote(explicit)+
                  ' -CheckLegacy) -ne '+ps_quote(explicit)+"){throw 'Explicit directory ignored'}", env) == 0
    return {'default_legacy_guard': True, 'environment_and_argument_overrides': True, 'source_untouched': True}

def stack_case(case, fail_sfu=False, verbatim_data=False):
    data = fixture/(case+' data with spaces')
    ports = []
    while len(ports) < 5:
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
            if port not in ports:
                ports.append(port)
    port, udp, sfu_port, sfu_tcp, sfu_udp = ports
    command = [shell, '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', str(package/'scripts/launch-stack.ps1'),
               '-DataDirectory', ('\\\\?\\' + str(data) if verbatim_data else str(data)), '-ServerBinary', str(server), '-SfuBinary', str(sfu),
               '-Port', str(port), '-UdpPort', str(udp), '-SfuPort', str(sfu_port),
               '-SfuTcpPort', str(sfu_tcp), '-SfuUdpPort', str(sfu_udp)]
    assert subprocess.run(command, stdout=log, stderr=log, timeout=35).returncode == 0, 'stack startup failed; inspect helper.log'
    base = f'http://127.0.0.1:{port}'

    def api(path, method='GET', body=None, token=None):
        request = urllib.request.Request(base+path, data=json.dumps(body).encode() if body is not None else None,
                                         method=method, headers={'Content-Type': 'application/json',
                                         **({'Authorization': 'Bearer '+token} if token else {})})
        with urllib.request.urlopen(request, timeout=5) as response:
            raw = response.read()
            return json.loads(raw) if raw.startswith((b'{', b'[')) else raw

    record = json.loads((data/'stack-instance.json').read_text(encoding='utf-8-sig'))
    try:
        health = api('/api/health')
        assert health['rtc_enabled'] and health['pid'] == record['relay_pid']
        session = api('/api/register', 'POST', {'username': 'FixtureAdmin', 'password': 'long-fixture-password'})
        token = session['token']
        bootstrap = api('/api/bootstrap', token=token)
        assert 'native-rtc-v1' in bootstrap['capabilities']
        room = next(c['id'] for c in bootstrap['channels'] if c['kind'] == 'voice')
        api('/api/voice/join', 'POST', {'channel_id': room}, token)
        rtc = api('/api/rtc/token', 'POST', {'channel_id': room}, token)
        assert rtc['url'] == base.replace('http:', 'ws:')
        assert api('/rtc/validate', token=rtc['token']) == b'success'
        api('/api/logout', 'POST', token=token)
        try:
            api('/rtc/validate', token=rtc['token'])
            raise AssertionError('revoked RTC token accepted')
        except urllib.error.HTTPError as error:
            assert error.code == 401
        if fail_sfu:
            # Stop only the private fixture child, after matching executable and creation time.
            code = '$r=Get-Content -LiteralPath '+ps_quote(data/'sfu-instance.json')+' -Raw | ConvertFrom-Json; '+\
                   '$p=Get-Process -Id $r.pid; if($p.Path -ne $r.exe -or $p.StartTime.ToUniversalTime() -ne '+\
                   "([DateTime]::Parse($r.started_at).ToUniversalTime())){throw 'Owned process identity changed'}; Stop-Process -Id $p.Id"
            assert run_ps(code) == 0
        else:
            assert subprocess.run([str(server), '--data', str(data), '--stop'], stdout=log, stderr=log, timeout=10).returncode == 0
        deadline = time.monotonic()+15
        while time.monotonic() < deadline:
            if not (data/'server-runtime.json').exists() and not (data/'stack-instance.json').exists() and not alive(record['relay_pid']) and not alive(record['sfu_pid']):
                break
            time.sleep(.1)
        else:
            raise AssertionError('owned stack did not stop after SFU failure' if fail_sfu else 'graceful stack cleanup did not complete')
        return {'health_identity': True, 'rtc_capability': True, 'authenticated_proxy_validate': True,
                'revoked_token_denied': True, 'owned_stack_cleanup': True, 'sfu_failure_stops_relay': fail_sfu,
                'rust_canonical_data_path': verbatim_data}
    finally:
        if (data/'server-runtime.json').exists():
            subprocess.run([str(server), '--data', str(data), '--stop'], stdout=log, stderr=log, timeout=10)

try:
    result = {'paths': paths_regression(), 'graceful_shutdown': stack_case('normal'),
              'canonical_profile_hosting': stack_case('canonical', verbatim_data=True),
              'video_service_failure': stack_case('failure', True)}
    (fixture/'result.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
    print('PASS: legacy default guard, explicit overrides, real SFU/gateway auth, logout denial, graceful stop and SFU-failure cleanup. Evidence', fixture/'result.json')
finally:
    log.close()
