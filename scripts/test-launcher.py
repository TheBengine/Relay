#!/usr/bin/env python3
"""Test the Windows launcher without opening Relay or changing a user profile."""
from __future__ import annotations

import argparse
import base64
import ctypes
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile
import time


def run(command: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(command, check=True, capture_output=True, text=True,
                          timeout=30, **kwargs)


def decoded_lines(output: str) -> list[str]:
    # splitlines preserves the empty base64 line representing an empty argument.
    return [base64.b64decode(line).decode('utf-8') for line in output.splitlines()]


def gui_subsystem(path: Path) -> int:
    image = path.read_bytes()
    pe = struct.unpack_from('<I', image, 0x3C)[0]
    assert image[pe:pe + 4] == b'PE\0\0'
    optional_header = pe + 24
    return struct.unpack_from('<H', image, optional_header + 68)[0]


def has_icon(path: Path) -> bool:
    image = path.read_bytes()
    pe = struct.unpack_from('<I', image, 0x3C)[0]
    optional = pe + 24
    magic = struct.unpack_from('<H', image, optional)[0]
    directory = optional + (96 if magic == 0x10B else 112)
    resource_rva = struct.unpack_from('<I', image, directory + 16)[0]
    section_count = struct.unpack_from('<H', image, pe + 6)[0]
    section_start = optional + struct.unpack_from('<H', image, pe + 20)[0]
    for index in range(section_count):
        section = section_start + index * 40
        virtual_size, address, raw_size, raw = struct.unpack_from('<IIII', image, section + 8)
        if address <= resource_rva < address + max(virtual_size, raw_size):
            root = raw + resource_rva - address
            named, numbered = struct.unpack_from('<HH', image, root + 12)
            types = [struct.unpack_from('<I', image, root + 16 + entry * 8)[0]
                     for entry in range(named + numbered)]
            return 14 in types  # RT_GROUP_ICON
    return False


def assert_process_exited(pid: int) -> None:
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.OpenProcess.argtypes = (ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong)
    kernel.OpenProcess.restype = ctypes.c_void_p
    kernel.WaitForSingleObject.argtypes = (ctypes.c_void_p, ctypes.c_ulong)
    kernel.WaitForSingleObject.restype = ctypes.c_ulong
    kernel.CloseHandle.argtypes = (ctypes.c_void_p,)
    handle = kernel.OpenProcess(0x00100000, False, pid)  # SYNCHRONIZE only
    if not handle:
        assert ctypes.get_last_error() == 87, 'Could not verify the fake client exited.'
        return
    try:
        assert kernel.WaitForSingleObject(handle, 5000) == 0, 'The fake client was left running.'
    finally:
        kernel.CloseHandle(handle)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work-directory', type=Path, required=True)
    args = parser.parse_args()
    if os.name != 'nt':
        raise SystemExit('This test uses the Windows process argument parser.')
    project = Path(__file__).resolve().parents[1]
    work = args.work_directory.absolute()
    if work.drive.upper() not in ('C:', 'D:') or str(work).startswith('\\\\'):
        raise SystemExit('Tests require a local C: or D: work directory.')
    for ancestor in (work, *work.parents):
        if ancestor.exists() and ancestor.is_symlink():
            raise SystemExit(f'The test work directory must not use a redirect: {ancestor}')
        if ancestor.exists() and getattr(ancestor.stat(follow_symlinks=False), 'st_file_attributes', 0) & 0x400:
            raise SystemExit(f'The test work directory must not use a reparse point: {ancestor}')
    work.mkdir(parents=True, exist_ok=True)
    shell = shutil.which('pwsh') or shutil.which('powershell')
    if not shell:
        raise SystemExit('PowerShell is needed to test build-launcher.ps1.')
    compiler = Path(os.environ.get('WINDIR', r'C:\Windows')) / 'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
    if not compiler.is_file():
        compiler = Path(os.environ.get('WINDIR', r'C:\Windows')) / 'Microsoft.NET/Framework/v4.0.30319/csc.exe'
    if not compiler.is_file():
        raise SystemExit('The installed .NET Framework compiler is needed.')

    with tempfile.TemporaryDirectory(prefix='launcher-test-', dir=work) as temporary:
        isolated = Path(temporary)
        harness = isolated / 'launcher harness.exe'
        run([str(compiler), '/nologo', '/target:exe', '/define:LAUNCHER_TEST',
             '/reference:System.Windows.Forms.dll', f'/out:{harness}',
             str(project / 'launcher/RelayLauncher.cs')])
        cases = [
            [],
            ['--background'],
            ['', 'one two', 'tab\tvalue', 'line\nbreak'],
            ['"', 'before"after', '\\"', '\\\\"'],
            ['C:\\path with spaces\\', 'trailing\\', '\\', '\\\\'],
            ['& echo unwanted', '|', '%USERPROFILE%', '$()', ';', 'æøå 漢字'],
        ]
        for case in cases:
            result = run([str(harness), '--roundtrip', *case], cwd=isolated)
            assert decoded_lines(result.stdout) == case, (case, result.stdout)

        package = isolated / 'package with spaces'
        package.mkdir()
        elsewhere = isolated / 'unrelated working directory'
        elsewhere.mkdir()
        plan = run([str(harness), '--plan', str(package), '--background'], cwd=elsewhere).stdout.splitlines()
        assert decoded_lines('\n'.join(plan[:3])) == [
            str(package / 'dist/relay.exe'), str(package), '"--background"']
        assert plan[3:] == ['direct', 'hidden']

        launcher = package / 'Relay.exe'
        run([shell, '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File',
             str(project / 'scripts/build-launcher.ps1'), '-OutputPath', str(launcher)])
        assert gui_subsystem(launcher) == 2, 'The launcher must be a GUI executable without a console.'
        assert launcher.stat().st_size < 100_000, 'The launcher should stay small.'
        if (project / 'crates/relay-client/assets/relay.ico').is_file():
            assert has_icon(launcher), 'The launcher must include the Relay icon.'

        # Use a tiny fake client to exercise the actual production entry point.
        # The fake is a windowless GUI executable: no real Relay/host is opened.
        stub_source = isolated / 'FakeRelayClient.cs'
        stub_source.write_text('''using System;
using System.IO;
using System.Text;
using System.Diagnostics;
class FakeRelayClient {
    static int Main(string[] args) {
        string receipt = Environment.GetEnvironmentVariable("RELAY_LAUNCHER_TEST_RECEIPT");
        string[] lines = new string[args.Length + 2];
        lines[0] = Process.GetCurrentProcess().Id.ToString();
        lines[1] = Convert.ToBase64String(Encoding.UTF8.GetBytes(Directory.GetCurrentDirectory()));
        for (int i = 0; i < args.Length; i++)
            lines[i + 2] = Convert.ToBase64String(Encoding.UTF8.GetBytes(args[i]));
        File.WriteAllLines(receipt, lines);
        return 0;
    }
}''', encoding='utf-8')
        fake_client = package / 'dist/relay.exe'
        fake_client.parent.mkdir()
        run([str(compiler), '/nologo', '/target:winexe', f'/out:{fake_client}', str(stub_source)])
        receipt = isolated / 'launch receipt.txt'
        environment = os.environ.copy()
        environment['RELAY_LAUNCHER_TEST_RECEIPT'] = str(receipt)
        forwarded = ['--background', '', 'space value', 'quote"value', 'C:\\trailing path\\', '& echo forbidden']
        run([str(launcher), *forwarded], cwd=elsewhere, env=environment)
        deadline = time.monotonic() + 5
        while not receipt.is_file() and time.monotonic() < deadline:
            time.sleep(0.02)
        assert receipt.is_file(), 'The production launcher did not start the fake client.'
        lines = receipt.read_text(encoding='utf-8-sig').splitlines()
        assert decoded_lines('\n'.join(lines[1:])) == [str(package), *forwarded]
        assert_process_exited(int(lines[0]))
        print(f'PASS: {len(cases)} Windows argument round trips, package path, working directory, GUI build and icon.')
        print('PASS: the actual launcher forwarded arguments to an isolated fake client; both processes exited.')
        print('No Relay application or server was opened.')


if __name__ == '__main__':
    main()
