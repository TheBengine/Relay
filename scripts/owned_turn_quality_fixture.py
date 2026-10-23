"""Explicit local-only TURN/TCP backend fixture, never a hosting/deploy option."""
import hashlib
import ipaddress
import json
from pathlib import Path
import re
import secrets
import socket
import subprocess

from dev_profile_core import direct, native_process, native_shell

PURPOSE = 'relay-owned-local-turn-tcp-candidate-v1'


def digest(path):
    with direct(path, False).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def candidate(path, sfu):
    raw = direct(path, False).read_bytes()
    if len(raw) > 32768:
        raise ValueError('Oversized owned TURN candidate manifest')
    record = json.loads(raw)
    if (record.get('schema') != 1 or record.get('purpose') != PURPOSE
            or record.get('version') != '1.13.10-relay-media.20261023.6-local'
            or record.get('windows_sha256') != digest(sfu)
            or record.get('go_module_files_unchanged') is not True):
        raise ValueError('Not the prepared owned TURN TCP candidate')
    helper = direct(Path(record['credential_helper']), False)
    if digest(helper) != record.get('credential_helper_sha256'):
        raise ValueError('Owned TURN fixture credential helper changed')
    version = subprocess.run([str(sfu), '--version'], capture_output=True, timeout=15,
        creationflags=subprocess.CREATE_NO_WINDOW)
    if version.returncode or record['version'].encode() not in version.stdout:
        raise ValueError('Owned TURN candidate runtime identity differs')
    return record, helper


def relay_range(address):
    # A fresh, bounded 128-number range, never a machine/network setting.
    for _ in range(50):
        start = 20000 + secrets.randbelow(8000)
        held = []
        try:
            for port in range(start, start + 128):
                item = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                held.append(item)
                item.bind((address, port))
            return start, start + 127
        except OSError:
            continue
        finally:
            for item in held:
                item.close()
    raise ValueError('No free owned loopback TURN range')


def owned_local_ipv4():
    code = ("$ErrorActionPreference='Stop'; @((Get-NetIPConfiguration | Where-Object {"
        "$_.IPv4DefaultGateway -and $_.NetAdapter.HardwareInterface -and "
        "$_.NetAdapter.Status -eq 'Up'}).IPv4Address.IPAddress) | ConvertTo-Json -Compress")
    result = subprocess.run([str(native_shell()), '-NoProfile', '-NonInteractive', '-Command', code],
        capture_output=True, timeout=15, creationflags=subprocess.CREATE_NO_WINDOW)
    if result.returncode or len(result.stdout) > 4096:
        raise ValueError('Owned hardware interface observation unavailable')
    values = json.loads(result.stdout.decode('utf-8-sig'))
    if isinstance(values, str):
        values = [values]
    if not isinstance(values, list) or len(set(values)) != 1:
        raise ValueError('One unambiguous owned hardware IPv4 interface required')
    address = ipaddress.IPv4Address(values[0])
    if not address.is_private or address.is_loopback or address.is_link_local or address.is_unspecified:
        raise ValueError('An owned private hardware IPv4 interface is required')
    return str(address)


def launcher(root, source, turn_port, address):
    original = direct(source / 'scripts/launch-stack.ps1', False)
    raw = original.read_bytes()
    text = raw.decode('utf-8-sig').replace('\r\n', '\n')
    old = '  node_ip: $nodeIp\n${mediaFilter}keys:'
    address = str(ipaddress.IPv4Address(address))
    new = (f'  node_ip: {address}\n'
           f'  ips:\n    includes: ["{address}/32"]\nkeys:')
    if text.count(old) != 1 or text.count('logging:\n  level: error') != 1:
        raise ValueError('Reviewed fixture launcher insertion point changed')
    text = text.replace(old, new)
    start, end = relay_range(address)
    turn = (f'turn:\n  enabled: true\n  bind_addresses: ["{address}"]\n'
            f'  domain: owned-turn.invalid\n  tls_port: {turn_port}\n'
            f'  external_tls: true\n  udp_port: 0\n'
            f'  relay_range_start: {start}\n  relay_range_end: {end}\n'
            f'  per_user_relay_allocation_limit: 12\n'
            f'  allow_restricted_peer_cidrs: ["{address}/32"]\n')
    text = text.replace('logging:\n  level: error', turn + 'logging:\n  level: error')
    # The copied QA launcher still uses the real reviewed runtime/supervisor
    # helpers. Only its fresh local configuration changes; production is intact.
    scripts = str(direct(source / 'scripts', False)).replace("'", "''")
    text = text.replace('$PSScriptRoot', "'" + scripts + "'")
    output = direct(root / 'launch-owned-turn.ps1')
    with output.open('xb') as stream:
        stream.write(text.encode())
    return output, {'original_launcher_sha256': hashlib.sha256(raw).hexdigest(),
        'owned_launcher_sha256': digest(output), 'same_host_interface_only': True,
        'external_tls_backend_without_tls': True, 'relay_range_start': start,
        'relay_range_end': end, 'per_user_allocation_limit': 12}


def credentials(helper, data, run_id):
    raw = direct(data / 'livekit.yaml', False).read_bytes()
    if len(raw) > 32768:
        raise ValueError('Unexpected owned SFU config size')
    match = re.search(rb'(?m)^keys:\r?\n  (relay[0-9a-f]{32}): ([0-9a-f]{64})\r?$', raw)
    if not match:
        raise ValueError('Not the freshly generated owned SFU key')
    result = subprocess.run([str(helper)], input=json.dumps({
        'key': match[1].decode(), 'secret': match[2].decode(), 'run_id': run_id}).encode(),
        capture_output=True, timeout=10, creationflags=subprocess.CREATE_NO_WINDOW)
    if result.returncode or len(result.stdout) > 16384 or result.stderr:
        raise ValueError('Owned TURN credential fixture failed')
    value = json.loads(result.stdout)
    if set(value) != {'alice', 'bob'}:
        raise ValueError('Wrong owned TURN credential peers')
    return value


def verify_listener(owner, port, address):
    address = str(ipaddress.IPv4Address(address))
    before = native_process(owner['pid'])
    if before != {key: owner[key] for key in ('pid', 'created', 'exe')}:
        raise ValueError('Owned TURN generation changed')
    code = ("$ErrorActionPreference='Stop'; "
        f"$t=@(Get-NetTCPConnection -State Listen -LocalPort {int(port)}); "
        f"if($t.Count -ne 1 -or $t[0].OwningProcess -ne {int(owner['pid'])} "
        f"-or $t[0].LocalAddress -ne '{address}'){{throw 'Wrong TURN listener owner'}}; "
        f"if(-not @(Get-NetIPAddress -IPAddress '{address}' -AddressState Preferred).Count)"
        "{throw 'TURN interface no longer assigned to this host'}")
    result = subprocess.run([str(native_shell()), '-NoProfile', '-NonInteractive', '-Command', code],
        capture_output=True, timeout=15, creationflags=subprocess.CREATE_NO_WINDOW)
    if result.returncode or native_process(owner['pid']) != before:
        raise ValueError('Owned same-host TURN TCP listener not proven')
    return True
