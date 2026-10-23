#!/usr/bin/env python3
"""Exercise the real packaging filter against an isolated synthetic source tree."""
from __future__ import annotations
import hashlib
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import zipfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--support-root', type=Path, help='Unchanged shell-runtime.ps1 location for staged script fixtures only.')
    parser.add_argument('--work-root', type=Path, help='Explicit local fixture root.')
    args = parser.parse_args()
    project = Path(__file__).resolve().parents[1]
    work = args.work_root or (project.parents[1] / 'work' if project.parent.name == 'outputs' else project / 'work')
    work.mkdir(parents=True, exist_ok=True)
    work = work.resolve()
    shell = shutil.which('pwsh') or shutil.which('powershell')
    if not shell:
        raise SystemExit('PowerShell is required to exercise package.ps1.')
    with tempfile.TemporaryDirectory(prefix='relay-package-test-', dir=work) as temporary:
        assert Path(temporary).resolve().parent == work
        assert Path(temporary).name.startswith('relay-package-test-')
        isolated = Path(temporary) / 'outputs' / 'relay'
        isolated.mkdir(parents=True)
        inputs = {
            '.gitignore': '/work/\n', '.cargo/config.toml':'[build]\n', 'Cargo.toml': '[workspace.package]\nversion = "0.6.0"\n',
            'Cargo.lock': '# fixture\n', 'LICENSE': 'fixture license\n',
            'README.md': 'fixture README\n', 'THIRD_PARTY_NOTICES.txt': 'fixture notices\n',
            'Relay.exe': 'fixture root launcher',
            'launcher/RelayLauncher.cs': 'fixture launcher source\n',
            'start-app.cmd': '@echo off\n', 'start-server.cmd': '@echo off\n', 'stop-server.cmd': '@echo off\n',
            'crates/relay-client/src/main.rs': 'fn main() {}\n',
            'crates/relay-client/Cargo.toml': '[package]\nname="fixture"\n',
            'docs/verification.md': 'fixture verification\n', 'docs/idle.csv': 'memory\n1\n',
            'scripts/example.ps1': 'Write-Output "fixture"\n',
            'vendor/audiopus_sys/src/lib.rs': 'pub fn fixture() {}\n',
            'vendor/audiopus_sys/LICENSE': 'fixture upstream license\n',
            'vendor/audiopus_sys/RELAY-PATCH.md': 'fixture provenance\n',
            'dist/relay.exe': 'fixture native client', 'dist/relay-server.exe': 'fixture native server',
            'dist/relay-network.exe': 'fixture embedded private network',
            'dist/livekit-server.exe': 'fixture native SFU',
            'network-helper/go.mod': 'module fixture/network\n',
            'network-helper/main.go': 'package main\nfunc main() {}\n',
        }
        excluded = {
            'crates/relay-client/work/media-smoke/server.sqlite3': 'private database',
            'crates/relay-client/Work/network-smoke/runtime.exe': 'test executable',
            'crates/relay-client/logs/server.log': 'private log',
            'crates/relay-client/src/history.db-wal': 'database WAL',
            'crates/relay-client/src/history.sqlite3-shm': 'database shared memory',
            'crates/relay-client/src/session.pid': 'process ID',
            'crates/relay-client/src/preferences.json': 'private preferences',
            'crates/relay-client/src/test-server.exe': 'test executable',
            'crates/relay-client/src/__pycache__/module.pyc': 'cache',
            'docs/data/session.sqlite': 'runtime database', 'docs/work/secret.txt': 'runtime text',
            'scripts/target/build.db': 'build database', 'scripts/.git/config': 'repository metadata',
            'scripts/.env': 'private environment',
            'vendor/audiopus_sys/target/private.db': 'build data',
            'vendor/audiopus_sys/work/private.json': 'runtime data',
            'crates/relay-server/src/server-runtime.json':'private management token',
            'docs/livekit.yaml':'private SFU credentials',
            'scripts/stack-instance.json':'private process record', 'dist/unexpected.exe': 'unverified executable',
            'dist/server.log': 'private log', 'dist/preferences.json': 'private preferences',
            'crates/relay-client/src/' + 'a' * 64 + '.device': 'protected device checkpoint',
            'docs/' + 'b' * 64 + '.lock': 'private device actor lock',
            'scripts/fixture.device.retired': 'retired protected identity archive',
            'vendor/audiopus_sys/fixture.pending': 'unfinished protected checkpoint',
            'docs/device.history/chunk.history': 'protected history archive',
            'scripts/device.HISTORY/anything.txt': 'entire private history directory',
            'crates/relay-client/src/segment.history': 'protected history segment',
        }
        for name, content in {**inputs, **excluded}.items():
            destination = isolated / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(content, encoding='utf-8')
        # Real upstream crate files have epoch timestamps ZIP cannot represent.
        old_timestamp_source = isolated / 'vendor/audiopus_sys/LICENSE'
        os.utime(old_timestamp_source, (1, 1))
        shutil.copy2(project / 'scripts' / 'package.ps1', isolated / 'scripts' / 'package.ps1')
        support = args.support_root or project
        shutil.copy2(support / 'scripts' / 'shell-runtime.ps1', isolated / 'scripts' / 'shell-runtime.ps1')
        shutil.copy2(project / 'scripts' / 'write-binary-manifest.ps1', isolated / 'scripts' / 'write-binary-manifest.ps1')
        shutil.copy2(project / 'scripts' / 'build-publication.ps1', isolated / 'scripts' / 'build-publication.ps1')
        original_manifest = b'{"source_manifest":"must remain unchanged"}'
        (isolated / 'dist' / 'manifest.json').write_bytes(original_manifest)
        result = subprocess.run([
            shell, '-NoLogo', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File',
            str(isolated / 'scripts' / 'package.ps1'),
        ], capture_output=True, text=True, timeout=60)
        if result.returncode:
            raise AssertionError(result.stdout + result.stderr)
        assert (isolated / 'dist' / 'manifest.json').read_bytes() == original_manifest
        archive = isolated.parent / 'relay-0.6.0-windows-x64.zip'
        with zipfile.ZipFile(archive) as package:
            assert package.testzip() is None
            entries = set(package.namelist())
            assert not ({f'Relay/{name}' for name in excluded} & entries)
            for name in inputs:
                assert package.read(f'Relay/{name}') == (isolated / name).read_bytes(), name
            assert package.getinfo('Relay/vendor/audiopus_sys/LICENSE').date_time[0] == 2000
            assert old_timestamp_source.stat().st_mtime == 1, 'staging must not mutate source timestamps'
            forbidden = {'work', 'target', 'data', 'logs', '.git', '__pycache__'}
            assert not any(set(name.lower().split('/')) & forbidden for name in entries)
            assert package.read('Relay/scripts/package.ps1') == (project / 'scripts' / 'package.ps1').read_bytes()
            assert package.read('Relay/scripts/shell-runtime.ps1') == (support / 'scripts' / 'shell-runtime.ps1').read_bytes()
            manifest = json.loads(package.read('Relay/dist/manifest.json').decode('utf-8-sig'))
            assert manifest['version'] == '0.6.0'
            assert {item['name'] for item in manifest['binaries']} == {'relay.exe', 'relay-server.exe', 'relay-network.exe', 'livekit-server.exe'}
            for binary in manifest['binaries']:
                content = package.read('Relay/dist/' + binary['name'])
                assert len(content) == binary['bytes']
                assert hashlib.sha256(content).hexdigest() == binary['sha256']
        # Explicit replacement inputs belong only to the staged private archive.
        # The selected checkout's binaries and old manifest are read-only inputs.
        override = Path(temporary) / 'new-client.exe'
        override.write_bytes(b'new staged client bytes')
        result = subprocess.run([
            shell, '-NoLogo', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File',
            str(isolated / 'scripts' / 'package.ps1'), '-ClientBinary', str(override),
        ], capture_output=True, text=True, timeout=60)
        if result.returncode:
            raise AssertionError(result.stdout + result.stderr)
        assert (isolated / 'dist' / 'relay.exe').read_text() == inputs['dist/relay.exe']
        assert (isolated / 'dist' / 'manifest.json').read_bytes() == original_manifest
        with zipfile.ZipFile(archive) as package:
            assert package.read('Relay/dist/relay.exe') == override.read_bytes()
        archive_before = archive.read_bytes()
        (isolated / 'dist' / 'relay-network.exe').unlink()
        result = subprocess.run([
            shell, '-NoLogo', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File',
            str(isolated / 'scripts' / 'package.ps1'), '-Version', '0.10.0',
        ], capture_output=True, text=True, timeout=60)
        assert result.returncode and 'relay-network.exe' in result.stderr
        assert archive.read_bytes() == archive_before
        assert (isolated / 'dist' / 'manifest.json').read_bytes() == original_manifest
    print('PASS: recursive runtime/source-executable exclusions; source/docs/scripts/verified dist preservation; manifest hashes; epoch timestamp staging preserves original source. All fixture data stayed under work.')


if __name__ == '__main__':
    main()
