"""Run native SDK opt-ins against an owned local Relay/SFU fixture; no real accounts."""
from __future__ import annotations
import argparse, importlib.util, json, os, pathlib, shutil, socket, subprocess, time, uuid

package = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--server', type=pathlib.Path, default=package/'dist/relay-server.exe')
parser.add_argument('--sfu', type=pathlib.Path, default=package/'dist/livekit-server.exe')
parser.add_argument('--test-binary', type=pathlib.Path, required=True, help='Explicit compiled relay Rust test executable from the native-RTC build')
parser.add_argument('--software-only', action='store_true', help='Skip the synthetic hardware-H264 RTC opt-in on unsupported GPUs')
parser.add_argument('--only', action='append', default=[], help='Run only an explicitly named fixture test from the opt-in inventory')
parser.add_argument('--work-root', type=pathlib.Path, help='Existing direct local C:/D: directory for owned QA data')
args = parser.parse_args()
server, sfu = args.server.resolve(), args.sfu.resolve()
test_binary = args.test_binary.resolve()
assert os.name == 'nt' and server.is_file() and sfu.is_file() and test_binary.is_file(), 'Windows server/SFU/compiled test binaries are required'
filters = ['actual_native_closed_peer_returns_error_without_abort',
           'actual_device_free_voice_recovery_preserves_nonce_counters_and_controls',
           'actual_device_free_voice_blackhole_deadline_and_cancel',
           'actual_device_free_voice_receiver_restart_rejects_inner_replay',
           'actual_sfu_synthetic_video_room_scope_stop_rejoin_and_revoke',
           'actual_controller_adaptive_video_telemetry_and_cleanup',
           'actual_controller_terminal_disconnect_releases_session_without_source_restart',
           'actual_controller_reconnect_updates_publications_and_keeps_stop_terminal',
           'actual_controller_signal_outage_deadline_and_retry_cancel',
           'actual_sfu_generated_application_audio_hidden_video_and_cleanup',
           'actual_sfu_protected_video_rejects_plaintext_wrong_key_and_unknown_sender']
if not args.software_only:
    filters += ['actual_controller_hardware_h264_generated_video_and_cancel']
if args.only:
    assert set(args.only).issubset(filters), 'Selected test is not in the owned RTC opt-in inventory'
    filters = list(dict.fromkeys(args.only))
inventory = subprocess.run([str(test_binary), '--list'], capture_output=True, text=True, timeout=10)
assert inventory.returncode == 0 and all(name+': test' in inventory.stdout for name in filters), 'Select the compiled native-RTC test executable containing the requested opt-ins'
workspace = package.parents[1] if package.parent.name == 'outputs' else package
if args.work_root is not None:
    from dev_profile_core import direct
    fixture_root = direct(args.work_root, False)
else:
    fixture_root = workspace/'work'
fixture = fixture_root/('rtc-acceptance-'+uuid.uuid4().hex)
fixture.mkdir(parents=True)
spec = importlib.util.spec_from_file_location('relay_integration', package/'scripts/test-integration.py')
helpers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helpers)
ports = []
while len(ports) < 5:
    value = helpers.unused_port(socket.SOCK_STREAM)
    if value not in ports:
        ports.append(value)
port, udp, sfu_port, sfu_tcp, sfu_udp = ports
data = fixture/'data'
shell = shutil.which('powershell.exe') or shutil.which('pwsh')
log = open(fixture/'commands.log', 'w', encoding='utf-8')
try:
    launch = [shell, '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', str(package/'scripts/launch-stack.ps1'),
              '-DataDirectory', str(data), '-ServerBinary', str(server), '-SfuBinary', str(sfu),
              '-Port', str(port), '-UdpPort', str(udp), '-SfuPort', str(sfu_port),
              '-SfuTcpPort', str(sfu_tcp), '-SfuUdpPort', str(sfu_udp)]
    assert subprocess.run(launch, stdout=log, stderr=log, timeout=35).returncode == 0, 'stack startup failed; inspect commands.log'
    api = helpers.API(port)
    users = {}
    for name in ('alice', 'bob', 'eve'):
        body = {'username': name, 'password': 'private-rtc-fixture-password'}
        if users:
            body['invite'] = api.call('POST', '/api/invites', users['alice']['session']['token'])['code']
        users[name] = {'session': api.call('POST', '/api/register', payload=body)}
    admin = users['alice']['session']['token']
    bootstrap = api.call('GET', '/api/bootstrap', admin)
    room = next(c['id'] for c in bootstrap['channels'] if c['kind'] == 'voice')
    other = api.call('POST', '/api/channels', admin, {'name': 'isolated', 'kind': 'voice'})['id']
    tickets = fixture/'private-tickets.json'
    tickets.write_text(json.dumps({'base': api.base, 'room_id': room, 'other_id': other, 'users': users}), encoding='utf-8')
    env = os.environ.copy()
    env['RELAY_RTC_TEST_TICKETS'] = str(tickets)
    env['RELAY_TEST_WORK_ROOT'] = str(fixture/'test-scratch')
    for test_filter in filters:
        command = [str(test_binary), test_filter, '--ignored', '--nocapture', '--test-threads=1']
        assert subprocess.run(command, cwd=package, env=env, stdout=log, stderr=log, timeout=90).returncode == 0, 'native opt-in failed; inspect commands.log'
    print('PASS: owned native RTC opt-ins; private test credentials remain outside the package. Evidence:', fixture)
finally:
    if (data/'server-runtime.json').exists():
        subprocess.run([str(server), '--data', str(data), '--stop'], stdout=log, stderr=log, timeout=10)
        deadline = time.monotonic()+15
        while (data/'stack-instance.json').exists() and time.monotonic() < deadline:
            time.sleep(.1)
    log.close()
