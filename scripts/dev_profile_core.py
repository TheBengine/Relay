"""Owned local Relay development profiles; GUI lifecycle uses Cua Driver only.

Markers identify this helper's test storage, not an operating-system security
boundary. An uncertain process or filesystem identity causes refusal, never a
foreground fallback, broad kill, or recursive path-based delete.
"""
import argparse
from contextlib import contextmanager
import ctypes
from ctypes import wintypes
import hashlib
import json
import ntpath
import os
from pathlib import Path
import re
import secrets
import stat
import subprocess
import sys
import time

MANAGER = '.relay-dev-profiles-owner.json'
MARKER = '.relay-dev-profile.json'
PROFILE_FLAGS = ('--profile-dir', '--data', '--state-dir', '-DataDirectory')
RUNTIME_RECORDS = ('server-runtime.json', 'managed-instance.json', 'stack-instance.json', 'stack-supervisor.json', 'sfu-instance.json')


def validate_name(name):
    if (not isinstance(name, str) or not re.fullmatch(r'[a-z][a-z0-9-]{0,31}', name)
            or re.fullmatch(r'con|prn|aux|nul|com[0-9]|lpt[0-9]', name)):
        raise ValueError('Use a simple lower-case profile name such as review-1.')
    return name


def local_absolute(value):
    raw = os.fspath(value)
    pieces = raw[3:].split('\\') if len(raw) > 3 else []
    if (not re.match(r'^[CDcd]:\\', raw) or raw.startswith(('\\\\', '//'))
            or ':' in raw[2:] or any(part in ('.', '..', '') for part in pieces)
            or '/' in raw or any(part.endswith((' ', '.')) for part in pieces)
            or any(re.fullmatch(r'(?i)con|prn|aux|nul|com[0-9¹²³]|lpt[0-9¹²³]|clock\$|conin\$|conout\$',
                                part.split('.')[0].rstrip(' ')) for part in pieces)
            or any(ord(char) < 32 or char in '<>"|?*' for char in raw)):
        raise ValueError('Use a direct absolute path on local C: or D:.')
    return Path(os.path.abspath(raw))


def direct(value, missing=True, _system_tool=False):
    path = local_absolute(value)
    require_local_volume(path)
    for cursor in [*reversed(path.parents), path]:
        try:
            info = cursor.lstat()
        except FileNotFoundError:
            if not missing:
                raise
            continue
        if (stat.S_ISLNK(info.st_mode) or info.st_file_attributes & 0x400
                or not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode))
                or stat.S_ISREG(info.st_mode) and info.st_nlink != 1 and not (_system_tool and cursor == path)):
            raise ValueError('Redirected, hard-linked or nonregular storage is refused.')
    return path


def require_local_volume(path):
    """Validate the local C:/D: volume mapping before any descendant metadata."""
    if os.name != 'nt':
        raise RuntimeError('Windows profile operations are supported only on Windows.')
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.GetDriveTypeW.argtypes = [wintypes.LPCWSTR]
    kernel.GetDriveTypeW.restype = wintypes.UINT
    kernel.QueryDosDeviceW.argtypes = [wintypes.LPCWSTR, wintypes.LPWSTR, wintypes.DWORD]
    kernel.QueryDosDeviceW.restype = wintypes.DWORD
    if kernel.GetDriveTypeW(str(path.anchor)) != 3:
        raise ValueError('Mapped, network and non-fixed drives are refused.')
    device = ctypes.create_unicode_buffer(1024)
    if not kernel.QueryDosDeviceW(path.drive, device, len(device)) or not device.value.startswith('\\Device\\HarddiskVolume'):
        raise ValueError('The drive is not a direct local volume.')


def cua_link_target(link, raw):
    """Interpret link metadata only; never resolve an unvalidated destination."""
    if not isinstance(raw, str) or not raw or len(raw) > 32767:
        raise ValueError('The Cua tool redirect target is ambiguous.')
    raw = raw.replace('/', '\\')
    if raw.startswith('\\\\?\\'):
        if not re.match(r'^[CDcd]:\\', raw[4:]):
            raise ValueError('The Cua tool target is not an extended local C:/D: path.')
        raw = raw[4:]
    if not re.match(r'^[A-Za-z]:', raw):
        # A simple relative symlink target is relative to its checked parent.
        # Root-relative, dot-segment and device syntax are intentionally refused.
        if raw.startswith('\\') or any(part in ('', '.', '..') for part in raw.split('\\')):
            raise ValueError('The Cua tool target is not an unambiguous local path.')
        raw = str(link.parent / raw)
    path = local_absolute(raw)
    if len(path.parts) > 512 or any('~' in part for part in path.parts):
        raise ValueError('A short-name or oversized Cua tool alias is refused.')
    return path


def resolve_cua_tool(value, max_hops=12):
    """Resolve ONLY the configured Cua executable through checked local hops.

    Each ancestor is inspected root-first with lstat before its descendants.
    Reparse metadata is read without following it, then its new C:/D: volume
    and ancestor chain are checked from the root again. Storage guards never
    call this resolver and still reject every redirect.
    """
    path = local_absolute(value)
    if path.name.casefold() != 'cua-driver.exe' or not 1 <= max_hops <= 12:
        raise ValueError('Choose the configured cua-driver.exe tool path.')
    seen = set()
    hops = 0
    while True:
        if len(str(path)) > 32767 or len(path.parts) > 512 or any('~' in part for part in path.parts):
            raise ValueError('The Cua tool path is ambiguous or oversized.')
        identity = str(path).casefold()
        if identity in seen:
            raise ValueError('A cycle in the Cua tool redirects was refused.')
        seen.add(identity)
        require_local_volume(path)
        redirected = False
        for cursor in [*reversed(path.parents), path]:
            info = cursor.lstat()
            if stat.S_ISLNK(info.st_mode) or info.st_file_attributes & 0x400:
                if getattr(info, 'st_reparse_tag', None) not in (0xA0000003, 0xA000000C):
                    raise ValueError('An unsupported Cua tool redirect was refused.')
                if hops >= max_hops:
                    raise ValueError('Too many Cua tool redirect hops were refused.')
                target = cua_link_target(cursor, os.readlink(cursor))
                path = local_absolute(target.joinpath(*path.relative_to(cursor).parts))
                hops += 1
                redirected = True
                break
            if cursor != path and not stat.S_ISDIR(info.st_mode):
                raise ValueError('A Cua tool ancestor is not an ordinary directory.')
            if cursor == path and (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1):
                raise ValueError('The Cua tool must be an ordinary single-link executable.')
        if redirected:
            continue
        if path.name.casefold() != 'cua-driver.exe':
            raise ValueError('The resolved Cua executable name is unexpected.')
        return direct(path, missing=False)


def native_shell():
    # Windows services system executables through WinSxS hard links. This exact
    # OS-reported tool may be multi-linked; profile data never gets that exception.
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.GetSystemDirectoryW.argtypes = [wintypes.LPWSTR, wintypes.UINT]
    kernel.GetSystemDirectoryW.restype = wintypes.UINT
    buffer = ctypes.create_unicode_buffer(32768)
    length = kernel.GetSystemDirectoryW(buffer, len(buffer))
    if not length or length >= len(buffer):
        raise RuntimeError('The native Windows tool directory could not be identified.')
    system = local_absolute(buffer.value)
    return direct(system / 'WindowsPowerShell' / 'v1.0' / 'powershell.exe', missing=False, _system_tool=True)


def sha(path):
    with direct(path, missing=False).open('rb') as handle:
        return hashlib.file_digest(handle, 'sha256').hexdigest()


def read_json(path):
    with direct(path, missing=False).open('rb') as handle:
        raw = handle.read(32769)
    if len(raw) > 32768:
        raise ValueError('The profile owner record is oversized.')
    return json.loads(raw.decode('utf-8'))


def write_json(path, data):
    direct(path)
    temporary = path.with_name(path.name + '.' + secrets.token_hex(8) + '.pending')
    with direct(temporary).open('x', encoding='utf-8', newline='\n') as handle:
        json.dump(data, handle, indent=2)
        handle.write('\n')
    direct(path)
    os.replace(temporary, path)


def profile_location(temp, name):
    name = validate_name(name)
    temp = local_absolute(temp)
    if temp == Path(temp.anchor):
        raise ValueError('The temporary directory cannot be a drive root.')
    base = temp / 'relay-dev-profiles'
    profile = base / name
    if profile.parent != base or profile == base:
        raise ValueError('Profile must be one named child of the owned temporary base.')
    return base, profile


def validate_owner(base, profile, name, manager, marker, temp):
    expected_base, expected_profile = profile_location(temp, name)
    if (base != expected_base or profile != expected_profile
            or manager.get('schema') != 1 or manager.get('root') != str(base)
            or not re.fullmatch(r'[0-9a-f]{64}', str(manager.get('nonce', '')))
            or marker.get('schema') != 1 or marker.get('profile') != str(profile)
            or marker.get('name') != validate_name(name)
            or marker.get('manager_nonce') != manager['nonce']
            or profile.parent != base):
        raise ValueError('This profile is not owned by this helper and temporary base.')


def open_profile(temp, name, exe, create=False):
    base, profile = profile_location(temp, name)
    direct(base); direct(profile)
    if not base.exists():
        if not create:
            raise ValueError('No owned development profile base exists.')
        base.mkdir()
        write_json(base / MANAGER, {'schema': 1, 'root': str(base), 'nonce': secrets.token_hex(32)})
    manager = read_json(base / MANAGER)
    if not profile.exists():
        if not create:
            raise ValueError('The named development profile does not exist.')
        if manager.get('root') != str(base) or manager.get('schema') != 1 or not re.fullmatch(r'[0-9a-f]{64}', str(manager.get('nonce', ''))):
            raise ValueError('The development profile base has no valid owner.')
        profile.mkdir()
        write_json(profile / MARKER, {'schema': 1, 'name': name, 'profile': str(profile),
                   'manager_nonce': manager['nonce'], 'exe': str(exe), 'client': None, 'launch_pending': False})
    marker = read_json(profile / MARKER)
    validate_owner(base, profile, name, manager, marker, temp)
    return base, profile, marker


@contextmanager
def profile_lock(base, name):
    path = direct(base / ('.' + validate_name(name) + '.controller.lock'))
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                   ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    kernel.CreateFileW.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.WriteFile.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD,
                                 ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]
    # CREATE_NEW + exclusive sharing + DELETE_ON_CLOSE ties cleanup to this
    # exact owned handle. Windows closes it on normal exit and exceptions;
    # no still-open CRT unlink or replacement-path deletion is attempted.
    handle = kernel.CreateFileW(str(path), 0x40000000 | 0x10000, 0, None, 1,
                                0x04000000 | 0x00200000 | 0x100, None)
    if handle == wintypes.HANDLE(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        payload = (str(os.getpid()) + ':' + secrets.token_hex(32) + '\n').encode('ascii')
        buffer = ctypes.create_string_buffer(payload)
        written = wintypes.DWORD()
        if not kernel.WriteFile(handle, buffer, len(payload), ctypes.byref(written), None) or written.value != len(payload):
            raise RuntimeError('The exclusive controller lock could not be written.')
        yield
    finally:
        if not kernel.CloseHandle(handle):
            raise ctypes.WinError(ctypes.get_last_error())


def hidden_run(arguments, timeout=20):
    startup = subprocess.STARTUPINFO()
    startup.dwFlags |= subprocess.STARTF_USESHOWWINDOW
    startup.wShowWindow = subprocess.SW_HIDE
    return subprocess.run(arguments, capture_output=True, text=True, encoding='utf-8', errors='replace',
                          timeout=timeout, startupinfo=startup, creationflags=subprocess.CREATE_NO_WINDOW)


def driver_call(driver, tool, payload):
    driver = resolve_cua_tool(driver)
    result = hidden_run([str(driver), tool, json.dumps(payload, separators=(',', ':'))])
    try:
        reply = json.loads(result.stdout)
    except ValueError:
        raise RuntimeError('Cua Driver returned no confirmed structured result; do not retry a launch blindly.')
    if result.returncode or isinstance(reply, dict) and (reply.get('isError') or reply.get('error')):
        raise RuntimeError('Cua Driver refused or could not confirm this background action. No foreground fallback was attempted.')
    if not isinstance(reply, (dict, list)):
        raise RuntimeError('Cua Driver returned an unexpected result shape.')
    return reply.get('structuredContent', reply) if isinstance(reply, dict) else reply


def split_command(command):
    count = ctypes.c_int()
    shell = ctypes.WinDLL('shell32', use_last_error=True)
    shell.CommandLineToArgvW.argtypes = [wintypes.LPCWSTR, ctypes.POINTER(ctypes.c_int)]
    shell.CommandLineToArgvW.restype = ctypes.POINTER(wintypes.LPWSTR)
    result = shell.CommandLineToArgvW(command, ctypes.byref(count))
    if not result:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        return [result[index] for index in range(count.value)]
    finally:
        kernel = ctypes.WinDLL('kernel32', use_last_error=True)
        kernel.LocalFree.argtypes = [ctypes.c_void_p]
        kernel.LocalFree.restype = ctypes.c_void_p
        kernel.LocalFree(result)


def option_value(arguments, flag):
    positions = [i for i, value in enumerate(arguments) if value == flag or value.startswith(flag + '=')]
    if len(positions) != 1 or positions[0] + 1 >= len(arguments):
        return None
    # Owned commands use exactly the two-token syntax emitted by this helper.
    if arguments[positions[0]] != flag:
        return None
    return arguments[positions[0] + 1]


def references_profile(arguments, profile):
    """Detect active storage without following an arbitrary command-line alias.

    Relay canonicalizes --profile-dir and passes extended local paths to its
    children. Process metadata therefore permits their equivalent lexical
    spellings; helper inputs and ownership checks remain deliberately strict.
    An unresolved alias is activity, never permission to start/reset storage.
    """
    for flag in PROFILE_FLAGS:
        values = [arguments[index + 1] for index, value in enumerate(arguments[:-1]) if value == flag]
        values += [value[len(flag) + 1:] for value in arguments if value.startswith(flag + '=')]
        if arguments and arguments[-1] == flag:
            return True
        for value in values:
            try:
                path = process_profile_path(value)
            except (TypeError, ValueError):
                return True
            if path == profile or path.is_relative_to(profile):
                return True
            # A parent segment can erase a junction from the lexical spelling,
            # especially under extended-length Win32 syntax. Never use that
            # normalized mismatch as evidence that storage is unrelated.
            if '..' in value.replace('/', '\\').split('\\'):
                return True
            # A different spelling can still name our profile through a
            # junction or short-name alias. Reject uncertainty conservatively;
            # direct() checks root-first and stops before any redirect target.
            try:
                direct(path, missing=False)
            except (OSError, ValueError, RuntimeError):
                return True
    return False


def process_profile_path(value):
    if not isinstance(value, str) or not value or len(value) > 32767:
        raise ValueError('A process storage path is uncertain.')
    raw = value.replace('/', '\\')
    if raw.startswith('\\\\?\\'):
        if not re.match(r'^[CDcd]:\\', raw[4:]):
            raise ValueError('Only extended local C:/D: paths can be classified.')
        raw = raw[4:]
    if not re.match(r'^[CDcd]:\\', raw) or ':' in raw[2:]:
        raise ValueError('A process path is not a bounded local absolute path.')
    pieces = raw[3:].split('\\')
    if (len(pieces) > 512 or any(ord(char) < 32 or char in '<>"|?*~' for char in raw)
            or any(part not in ('', '.', '..') and part.endswith((' ', '.')) for part in pieces)):
        raise ValueError('A process storage alias cannot be classified safely.')
    return local_absolute(ntpath.normpath(raw))


def owned_client(process, marker, exe, profile):
    client = marker.get('client') or {}
    args = process.get('arguments', [])
    return (process.get('pid') == client.get('pid') and process.get('created') == client.get('created')
            and process.get('exe') == str(exe) and marker.get('exe') == str(exe)
            and option_value(args, '--profile-dir') == str(profile)
            and option_value(args, '--dev-profile-owner') == client.get('nonce')
            and re.fullmatch(r'[0-9a-f]{64}', str(client.get('nonce', ''))) is not None)


def native_process(pid):
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    kernel.QueryFullProcessImageNameW.argtypes = [wintypes.HANDLE, wintypes.DWORD, wintypes.LPWSTR, ctypes.POINTER(wintypes.DWORD)]
    kernel.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    handle = kernel.OpenProcess(0x1000 | 0x100000, False, pid)
    if not handle:
        if ctypes.get_last_error() == 87:  # ERROR_INVALID_PARAMETER: no such PID.
            return None
        raise RuntimeError('A process identity is inaccessible; no lifecycle action is safe.')
    try:
        # A terminated process object can remain open through another tool's
        # handle, while its image name is already unavailable. It is not live.
        wait = kernel.WaitForSingleObject(handle, 0)
        if wait == 0:
            return None
        if wait != 0x102:
            raise RuntimeError('The process liveness could not be checked.')
        times = [wintypes.FILETIME() for _ in range(4)]
        buffer = ctypes.create_unicode_buffer(32768)
        size = wintypes.DWORD(len(buffer))
        if not kernel.GetProcessTimes(handle, *(ctypes.byref(value) for value in times)) or not kernel.QueryFullProcessImageNameW(handle, 0, buffer, ctypes.byref(size)):
            if kernel.WaitForSingleObject(handle, 0) == 0:
                return None  # It exited between the liveness and image queries.
            raise RuntimeError('The exact process identity could not be checked.')
        return {'pid': int(pid), 'created': times[0].dwLowDateTime | times[0].dwHighDateTime << 32,
                'exe': str(local_absolute(buffer.value))}
    finally:
        kernel.CloseHandle(handle)


def relay_processes():
    shell = native_shell()
    query = ("$ErrorActionPreference='Stop'; Import-Module (Join-Path $PSHOME 'Modules\\CimCmdlets\\CimCmdlets.psd1') -Force; "
             "@(Get-CimInstance Win32_Process -Filter \"Name='relay.exe' OR Name='relay-server.exe' OR Name='relay-network.exe' OR Name='livekit-server.exe'\" | "
             "Select-Object ProcessId,ExecutablePath,CommandLine,@{Name='CreatedFileTime';Expression={$_.CreationDate.ToUniversalTime().ToFileTimeUtc()}}) | ConvertTo-Json -Compress")
    result = hidden_run([str(shell), '-NoLogo', '-NoProfile', '-NonInteractive', '-Command', query])
    if result.returncode:
        raise RuntimeError('Process ownership could not be checked; no process was stopped.')
    rows = json.loads(result.stdout or '[]') or []
    if isinstance(rows, dict):
        rows = [rows]
    processes = []
    for row in rows:
        if not row.get('ExecutablePath') or not row.get('CommandLine'):
            raise RuntimeError('A Relay process has inaccessible metadata; no lifecycle action is safe.')
        live = native_process(int(row['ProcessId']))
        if live:
            if not same_process_generation(row, live):
                raise RuntimeError('A Relay process changed during metadata inspection; no lifecycle action is safe.')
            live['arguments'] = split_command(row['CommandLine'])
            processes.append(live)
    return processes


def same_process_generation(row, live):
    """Bind CIM command-line data to the live handle, never PID/name alone.

    WMI CreationDate contains microseconds; native FILETIME contains 100 ns
    ticks. Compare the common precision, preserving integer timestamps.
    """
    created = row.get('CreatedFileTime')
    return (type(created) is int and created > 0
            and int(row['ProcessId']) == live.get('pid')
            and created // 10 == live.get('created', -1) // 10
            and str(local_absolute(row['ExecutablePath'])) == live.get('exe'))


def secondary_screen():
    class RECT(ctypes.Structure):
        _fields_ = [('left', ctypes.c_long), ('top', ctypes.c_long), ('right', ctypes.c_long), ('bottom', ctypes.c_long)]
    class INFO(ctypes.Structure):
        _fields_ = [('size', wintypes.DWORD), ('monitor', RECT), ('work', RECT), ('flags', wintypes.DWORD)]
    user = ctypes.WinDLL('user32', use_last_error=True)
    found = []
    callback_type = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HANDLE, wintypes.HDC, ctypes.POINTER(RECT), wintypes.LPARAM)
    user.GetMonitorInfoW.argtypes = [wintypes.HANDLE, ctypes.POINTER(INFO)]
    user.EnumDisplayMonitors.argtypes = [wintypes.HDC, ctypes.POINTER(RECT), callback_type, wintypes.LPARAM]
    def collect(handle, _dc, _rect, _data):
        info = INFO(); info.size = ctypes.sizeof(info)
        if user.GetMonitorInfoW(handle, ctypes.byref(info)) and not info.flags & 1:
            found.append({'x': info.work.left, 'y': info.work.top,
                          'width': info.work.right - info.work.left, 'height': info.work.bottom - info.work.top})
        return True
    callback = callback_type(collect)
    if not user.EnumDisplayMonitors(None, None, callback, 0):
        raise RuntimeError('Display metadata could not be read.')
    return found[0] if found else None


def launch_request(exe, profile, nonce, session, display):
    arguments = ['--profile-dir', str(profile), '--background', '--quit-on-close', '--dev-profile-owner', nonce]
    if display:
        if display['width'] < 980 or display['height'] < 690:
            raise ValueError('The secondary display is too small for Relay; no window was launched.')
        arguments += ['--position', f"{display['x'] + 20},{display['y'] + 20}", '--size',
                      f"{min(1000, display['width'] - 40)}x{min(1150, display['height'] - 40)}"]
    return {'path': str(exe), 'additional_arguments': arguments, 'session': session,
            'start_minimized': display is None}


def window_for(driver, process, session):
    reply = driver_call(driver, 'list_windows', {'pid': process['pid'], 'on_screen_only': False, 'session': session})
    rows = reply if isinstance(reply, list) else reply.get('windows', [])
    windows = [window for window in rows if window.get('pid') == process['pid'] and window.get('window_id')]
    if len(windows) != 1:
        raise RuntimeError('One exact owned Relay window could not be confirmed; no GUI input was sent.')
    return windows[0]


def start_profile(base, profile, marker, exe, driver):
    if marker.get('launch_pending') or any(references_profile(p['arguments'], profile) for p in relay_processes()):
        raise RuntimeError('The profile already has a pending or live process; inspect it instead of launching again.')
    nonce = secrets.token_hex(32)
    session = 'relay-dev-' + marker['name'] + '-' + nonce[:8]
    request = launch_request(exe, profile, nonce, session, secondary_screen())
    marker.update({'exe': str(exe), 'exe_sha256': sha(exe), 'client': None,
                   'launch_pending': True, 'session': session})
    write_json(profile / MARKER, marker)
    # launch_app's PID may name an existing app. Adopt only the fresh nonce and
    # exact executable/profile observed independently through process metadata.
    launch = driver_call(driver, 'launch_app', request)
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        candidates = [p for p in relay_processes() if p['exe'] == str(exe)
                      and option_value(p['arguments'], '--profile-dir') == str(profile)
                      and option_value(p['arguments'], '--dev-profile-owner') == nonce]
        if len(candidates) == 1:
            process = candidates[0]
            marker['client'] = {'pid': process['pid'], 'created': process['created'], 'nonce': nonce}
            marker['launch_pending'] = False
            write_json(profile / MARKER, marker)
            while True:
                try:
                    window = window_for(driver, process, session)
                    break
                except RuntimeError:
                    if time.monotonic() >= deadline:
                        raise RuntimeError('The owned process started but its exact window is unconfirmed; keep the profile for inspection.')
                    time.sleep(0.25)
            return {'profile': str(profile), 'pid': process['pid'], 'window': window, 'background': True,
                    'launch_active': launch.get('active') if isinstance(launch, dict) else None}
        if len(candidates) > 1:
            raise RuntimeError('Ambiguous launched processes; no retry or broad cleanup was attempted.')
        time.sleep(0.25)
    raise RuntimeError('Launch remains unconfirmed. Keep the owner marker and inspect before retrying.')


class PinnedTree:
    """Pin every ordinary object before deletion; never traverse a redirect.

    Handles deny rename/delete sharing. SetFileInformationByHandle deletes the
    validated object through its own DELETE handle, rather than reopening paths.
    """
    def __init__(self, root, delete=False):
        self.root, self.delete = direct(root, missing=False), delete
        self.entries, self.ancestors = [], []
        self.kernel = ctypes.WinDLL('kernel32', use_last_error=True)
        self.kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                           ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
        self.kernel.CreateFileW.restype = wintypes.HANDLE
        self.kernel.SetFileInformationByHandle.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
        self.kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        class FILE_INFO(ctypes.Structure):
            _fields_ = [('attributes', wintypes.DWORD), ('created', wintypes.FILETIME),
                        ('accessed', wintypes.FILETIME), ('written', wintypes.FILETIME),
                        ('volume', wintypes.DWORD), ('size_high', wintypes.DWORD),
                        ('size_low', wintypes.DWORD), ('links', wintypes.DWORD),
                        ('index_high', wintypes.DWORD), ('index_low', wintypes.DWORD)]
        self.file_info = FILE_INFO
        self.kernel.GetFileInformationByHandle.argtypes = [wintypes.HANDLE, ctypes.POINTER(FILE_INFO)]
        self.kernel.GetFinalPathNameByHandleW.argtypes = [wintypes.HANDLE, wintypes.LPWSTR, wintypes.DWORD, wintypes.DWORD]
        self.kernel.GetFinalPathNameByHandleW.restype = wintypes.DWORD
        self.kernel.ReadFile.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD,
                                        ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]

    def pin(self, path, deleting):
        direct(path, missing=False)
        access = 0x80 | (0x10000 if deleting else 0)
        if path == self.root / MARKER:
            access |= 0x80000000  # Read the owner record through this pinned handle.
        handle = self.kernel.CreateFileW(str(path), access, 3, None, 3, 0x00200000 | 0x02000000, None)
        if handle == wintypes.HANDLE(-1).value:
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            info = self.file_info()
            final_name = ctypes.create_unicode_buffer(32768)
            length = self.kernel.GetFinalPathNameByHandleW(handle, final_name, len(final_name), 0)
            if (not self.kernel.GetFileInformationByHandle(handle, ctypes.byref(info))
                    or not length or length >= len(final_name)):
                raise RuntimeError('The opened filesystem object could not be identified.')
            actual = final_name.value
            if actual.startswith('\\\\?\\') and re.match(r'^[CDcd]:\\', actual[4:]):
                actual = actual[4:]
            if (local_absolute(actual) != path or info.attributes & 0x400
                    or not info.attributes & 0x10 and info.links != 1):
                raise ValueError('The opened object is redirected, hard-linked or changed.')
            # Ancestors and this exact object are now pinned against rename.
            direct(path, missing=False)
        except BaseException:
            self.kernel.CloseHandle(handle)
            raise
        return handle

    def __enter__(self):
        try:
            for ancestor in reversed(self.root.parents):
                self.ancestors.append(self.pin(ancestor, False))
            pending = [self.root]
            while pending:
                path = pending.pop()
                handle = self.pin(path, self.delete)
                self.entries.append((path, handle))
                if len(self.entries) > 100000:
                    raise RuntimeError('Profile has too many entries for a bounded reset.')
                if path.is_dir():
                    with os.scandir(path) as children:
                        pending.extend(Path(child.path) for child in children)
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def remove(self):
        if not self.delete:
            raise RuntimeError('This tree was opened for validation only.')
        for path, handle in sorted(self.entries, key=lambda row: len(row[0].parts), reverse=True):
            disposition = wintypes.BOOL(True)
            if not self.kernel.SetFileInformationByHandle(handle, 4, ctypes.byref(disposition), ctypes.sizeof(disposition)):
                raise ctypes.WinError(ctypes.get_last_error())
            self.kernel.CloseHandle(handle)
            self.entries = [entry for entry in self.entries if entry[1] != handle]

    def owner_record(self):
        handle = next((handle for path, handle in self.entries if path == self.root / MARKER), None)
        if handle is None:
            raise RuntimeError('No pinned owner record exists.')
        buffer = ctypes.create_string_buffer(32769)
        count = wintypes.DWORD()
        if not self.kernel.ReadFile(handle, buffer, len(buffer), ctypes.byref(count), None):
            raise ctypes.WinError(ctypes.get_last_error())
        if count.value > 32768:
            raise ValueError('The pinned owner record is oversized.')
        return json.loads(buffer.raw[:count.value].decode('utf-8'))

    def __exit__(self, _kind, _value, _trace):
        for _path, handle in self.entries:
            self.kernel.CloseHandle(handle)
        self.entries.clear()
        for handle in reversed(self.ancestors):
            self.kernel.CloseHandle(handle)
        self.ancestors.clear()


def close_control(snapshot, process):
    # Modern Cua defaults to Markdown. Request structured elements explicitly;
    # never guess tokens from a stale or textual tree. Older clients retain
    # their original full-exit label. New Close window needs the isolated mode.
    labels = {'Close Relay'}
    if process.get('arguments', []).count('--quit-on-close') == 1:
        labels.add('Close window')
    close = [row for row in snapshot.get('elements', []) if row.get('label') in labels
             and row.get('element_token') and row.get('enabled', True)]
    if len(close) != 1:
        raise RuntimeError('The owned full-close control was unavailable; no force-kill or foreground fallback was attempted.')
    return close[0]


def stop_profile(profile, marker, exe, driver, project):
    processes = relay_processes()
    client = marker.get('client')
    live = next((p for p in processes if client and p['pid'] == client['pid']), None)
    exact_pid = native_process(client['pid']) if client else None
    unowned_client = any(p['exe'].casefold().endswith('\\relay.exe')
                         and references_profile(p['arguments'], profile)
                         and not owned_client(p, marker, exe, profile) for p in processes)
    if (marker.get('launch_pending') or unowned_client
            or exact_pid and (not live or exact_pid['created'] != client['created']
                             or exact_pid['exe'] != str(exe))
            or live and not owned_client(live, marker, exe, profile)):
        raise RuntimeError('Client identity is unconfirmed or changed; no process was stopped.')
    if sha(exe) != marker.get('exe_sha256'):
        raise RuntimeError('The recorded client executable changed; no process was stopped.')
    with PinnedTree(profile):
        pass
    data = direct(profile / 'server-data')
    if data.exists():
        stop_script = direct(project / 'scripts' / 'stop-server.ps1', missing=False)
        shell = native_shell()
        result = hidden_run([str(shell), '-NoLogo', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass',
                             '-File', str(stop_script), '-DataDirectory', str(data)], timeout=20)
        if result.returncode:
            raise RuntimeError('Graceful shutdown of the exact profile server did not complete. No process was killed.')
    if live:
        # Recheck generation/profile immediately before the exact background UI action.
        fresh = next((p for p in relay_processes() if p['pid'] == live['pid']), None)
        if not fresh or not owned_client(fresh, marker, exe, profile):
            raise RuntimeError('Client identity changed before close; no GUI input was sent.')
        session = marker['session']
        window = window_for(driver, fresh, session)
        snapshot = driver_call(driver, 'get_window_state', {'pid': fresh['pid'], 'window_id': window['window_id'],
                               'include_screenshot': False, 'tree_format': 'elements',
                               'max_elements': 200, 'max_depth': 10, 'session': session})
        close = close_control(snapshot, fresh)
        driver_call(driver, 'click', {'target': {'kind': 'window', 'pid': fresh['pid'], 'window_id': window['window_id']},
                    'element_token': close['element_token'], 'delivery_mode': 'background', 'session': session})
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        if not any(references_profile(p['arguments'], profile) for p in relay_processes()):
            if client:
                remaining = driver_call(driver, 'list_windows', {'pid': client['pid'], 'on_screen_only': False,
                                         'session': marker['session']})
                windows = remaining if isinstance(remaining, list) else remaining.get('windows', [])
                if any(row.get('pid') == client['pid'] for row in windows):
                    raise RuntimeError('A window for the recorded client remains; keep the profile for inspection.')
            marker['client'] = None
            marker['launch_pending'] = False
            write_json(profile / MARKER, marker)
            if marker.get('session'):
                driver_call(driver, 'end_session', {'session': marker['session']})
            return {'profile': str(profile), 'stopped': True, 'force_killed': False,
                    'owned_windows_remaining': 0, 'session_ended': bool(marker.get('session'))}
        time.sleep(0.25)
    raise RuntimeError('An owned-profile process is still present. Data was left in place.')


def reset_profile(base, profile, name, marker, processes, temp):
    expected_base, expected_profile = profile_location(temp, name)
    if base != expected_base or profile != expected_profile:
        raise ValueError('Reset is restricted to the exact named profile in the temporary base.')
    manager = read_json(base / MANAGER)
    validate_owner(base, profile, name, manager, marker, temp)
    if marker.get('launch_pending') or any(references_profile(p['arguments'], profile) for p in processes):
        raise RuntimeError('Stop the owned profile normally before resetting it.')
    if any(direct(profile / 'server-data' / name).exists() for name in RUNTIME_RECORDS):
        raise RuntimeError('Managed server records remain; reset was refused.')
    with PinnedTree(profile, delete=True) as pinned:
        # Re-read while all objects are pinned; a replaced marker cannot authorize reset.
        if pinned.owner_record() != marker:
            raise RuntimeError('The profile owner record changed before reset.')
        pinned.remove()
    return {'profile': str(profile), 'reset': True, 'manager_preserved': True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--name', default='review')
    parser.add_argument('--exe', required=True)
    parser.add_argument('--project-root', required=True)
    parser.add_argument('--driver')
    parser.add_argument('--stop', action='store_true')
    parser.add_argument('--reset', action='store_true')
    args = parser.parse_args()
    name = validate_name(args.name)
    exe = direct(args.exe, missing=False)
    if exe.name.casefold() != 'relay.exe' or exe.parent.name.casefold() != 'dist':
        raise ValueError('Choose the actual relay.exe client, normally under dist, not a launcher or another program.')
    project = direct(args.project_root, missing=False)
    driver = resolve_cua_tool(args.driver or str(Path(os.environ['LOCALAPPDATA']) / 'Programs' / 'Cua' / 'cua-driver' / 'bin' / 'cua-driver.exe'))
    temp = direct(os.environ.get('TEMP') or os.environ.get('TMP') or '', missing=False)
    base, profile, marker = open_profile(temp, name, exe, create=not (args.stop or args.reset))
    with profile_lock(base, name):
        marker = read_json(profile / MARKER)
        validate_owner(base, profile, name, read_json(base / MANAGER), marker, temp)
        if args.stop:
            result = stop_profile(profile, marker, exe, driver, project)
        if args.reset:
            result = reset_profile(base, profile, name, read_json(profile / MARKER), relay_processes(), temp)
        elif not args.stop:
            result = start_profile(base, profile, marker, exe, driver)
    print(json.dumps(result))


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print('Profile action refused: ' + str(error), file=sys.stderr)
        raise SystemExit(2)
