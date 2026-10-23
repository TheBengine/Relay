"""Build a local Inno Setup installer from an explicit, hash-checked release.

This verifies payload integrity. It does not establish publisher trust or sign
an installer. No personal profiles, registry keys or installed apps are used.
"""
import argparse
from contextlib import ExitStack, contextmanager
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tomllib
import uuid
import zipfile

APP_ID = '3bed570c-afb7-4f36-8a88-8c6f1772d6af'
RECOVERY_APP_ID = '68e09dab-cada-4de3-b9d3-e2a9bdb8036c'
RECOVERY_APP_NAME = 'Relay Recovery Fixture'
RECOVERY_PROBES = ('temp', 'intent', 'payload', 'committed', 'done')
BINARY_NAMES = {'relay.exe', 'relay-server.exe', 'livekit-server.exe'}
DISTRIBUTION_BINARIES = {'Relay.exe', 'dist/relay.exe', 'dist/relay-server.exe', 'dist/livekit-server.exe', 'dist/relay-network.exe'}
# Present only in a build which includes the account-free connection.
OPTIONAL_BINARIES = {'dist/relay-iroh.exe'}
DISTRIBUTION_GUIDE = 'LAES-MIG.txt'
INTERNAL_DOCUMENTATION = frozenset(name.casefold() for name in (
    'LAES-MIG.md', 'DISCORD.md', 'NATIVE-KONTROL.md', 'RESSOURCER.md',
    'OPDATERINGER.md', 'ENHEDSGODKENDELSE.md', 'GIFS.md',
))
MAX_RELEASE_VERSION_BYTES = 64
MAX_CARGO_BYTES = 65536
U64_MAX_DECIMAL = '18446744073709551615'


def release_version(value):
    """Shared release grammar: bounded ASCII SemVer, without build metadata.

    The updater accepts stable/beta SemVer releases with no build metadata and
    at most 64 bytes. Core integers fit u64; numeric prerelease identifiers do
    not have leading zeros. Version syntax does not establish publisher trust.
    """
    if (not isinstance(value, str) or not value or len(value) > MAX_RELEASE_VERSION_BYTES
            or not value.isascii()):
        raise ValueError('Release version must be at most 64 ASCII bytes.')
    core, separator, prerelease = value.partition('-')
    parts = core.split('.')
    if len(parts) != 3:
        raise ValueError('Release version requires three decimal components.')
    for part in parts:
        if (not re.fullmatch(r'0|[1-9][0-9]*', part) or len(part) > 20
                or len(part) == 20 and part > U64_MAX_DECIMAL):
            raise ValueError('Release version components must be canonical u64 decimals.')
    if separator:
        for identifier in prerelease.split('.'):
            if (not re.fullmatch(r'[A-Za-z0-9-]+', identifier)
                    or identifier.isdigit() and len(identifier) > 1 and identifier.startswith('0')):
                raise ValueError('Release prerelease identifiers must follow SemVer syntax.')
    return value


def cargo_release_version(raw):
    if not isinstance(raw, bytes) or len(raw) > MAX_CARGO_BYTES:
        raise ValueError('Source Cargo.toml exceeds the version metadata budget.')
    document = tomllib.loads(raw.decode('utf-8'))
    try:
        version = document['workspace']['package']['version']
    except (KeyError, TypeError) as error:
        raise ValueError('Source Cargo.toml requires workspace.package.version.') from error
    return release_version(version)


def workspace_release(root):
    with direct_reader(root / 'Cargo.toml') as stream:
        raw = stream.read(MAX_CARGO_BYTES + 1)
    return cargo_release_version(raw), hashlib.sha256(raw).hexdigest()


def payload_release(payload, files):
    """Read only the Cargo metadata in the exact hash-bound source archive."""
    with direct_reader(payload / 'sources/Relay-source.zip') as stream:
        if hashlib.file_digest(stream, 'sha256').hexdigest() != files['sources/Relay-source.zip']:
            raise ValueError('Source archive changed before release-version validation.')
        stream.seek(0)
        with zipfile.ZipFile(stream) as archive:
            entries = [entry for entry in archive.infolist() if entry.filename == 'Relay/Cargo.toml']
            if len(entries) != 1 or entries[0].file_size > MAX_CARGO_BYTES:
                raise ValueError('Source archive requires one bounded Relay/Cargo.toml.')
            with archive.open(entries[0]) as cargo:
                raw = cargo.read(MAX_CARGO_BYTES + 1)
    return cargo_release_version(raw)


def select_release_version(workspace_version, source_version, requested, app_id, name):
    workspace_version, source_version = map(release_version, (workspace_version, source_version))
    version = workspace_version if requested is None else release_version(requested)
    if version != workspace_version and (app_id != RECOVERY_APP_ID or name != RECOVERY_APP_NAME):
        raise ValueError('A different source-control version is allowed only for the exact isolated recovery fixture.')
    if version != source_version:
        raise ValueError('Incoming version must match the hash-bound source archive Cargo version.')
    return version


def release_marker(app_id, version, sequence, payload_hash):
    version = release_version(version)
    return ('[Relay]\r\nSchema=1\r\nAppId=' + app_id + '\r\nVersion=' + version
            + '\r\nSequence=' + sequence + '\r\nPayloadSHA256=' + payload_hash + '\r\n')


def installer_filename(version, sequence):
    return 'Relay-' + release_version(version) + '-' + sequence + '-Setup.exe'


def build_sequence(value):
    if not isinstance(value, str) or not re.fullmatch(r'[0-9]{14}', value):
        raise ValueError('Release sequence must be a fourteen-digit UTC build identifier.')
    try:
        stamp = datetime.strptime(value, '%Y%m%d%H%M%S').replace(tzinfo=timezone.utc)
    except ValueError as error:
        raise ValueError('Release sequence must identify a real UTC date and second.') from error
    if stamp.year < 1970:
        raise ValueError('Release sequence must not precede the Unix epoch.')
    return stamp


def bound_client_build(metadata, files, version, sequence, required=False):
    """Validate publisher-supplied identity without executing a payload program.

    The assembler must read --build-info from its exact, freshly built client.
    This binds that record to the manifest client hash and installer identity;
    it does not independently attest source provenance or publisher trust.
    Old packages may be rebuilt unless --require-build-identity is requested.
    """
    if 'client_build' not in metadata:
        if required:
            raise ValueError('This installer requires the exact client build identity.')
        return None
    info = metadata['client_build']
    keys = {'schema', 'version', 'build_id', 'sequence', 'built_at', 'build_date',
            'source_sha256', 'source_scope', 'git_revision', 'git_dirty'}
    if isinstance(info, dict) and 'invitation_protocol' in info:
        keys.add('invitation_protocol')
        if type(info['invitation_protocol']) is not int or info['invitation_protocol'] not in (0, 1):
            raise ValueError('Unsupported invitation protocol capability.')
    if not isinstance(info, dict) or set(info) != keys or type(info['schema']) is not int or info['schema'] != 1:
        raise ValueError('Unsupported or incomplete client build identity.')
    if (release_version(info['version']) != version or metadata.get('version') != version
            or info['sequence'] != sequence
            or metadata.get('client_sha256') != files.get('dist/relay.exe')):
        raise ValueError('Client build identity must match this payload, version and installer sequence.')
    stamp = build_sequence(info['sequence'])
    source = info['source_sha256']
    if (not isinstance(source, str) or not re.fullmatch(r'[0-9a-f]{64}', source)
            or info['source_scope'] not in ('relay-workspace-source-v1', 'relay-client-inputs-v2')
            or info['build_id'] != sequence + '-' + source[:12]
            or info['built_at'] != stamp.strftime('%Y-%m-%dT%H:%M:%SZ')
            or info['build_date'] != stamp.strftime('%Y-%m-%d')):
        raise ValueError('Client build date, identifier or source fingerprint is inconsistent.')
    revision, dirty = info['git_revision'], info['git_dirty']
    if (not isinstance(revision, str)
            or revision != 'nogit' and not re.fullmatch(r'[0-9a-f]{12}', revision)
            or dirty is not None and type(dirty) is not bool
            or revision == 'nogit' and dirty is not None):
        raise ValueError('Client build Git provenance is invalid.')
    return info


def protocol_registration_define(client_build, app_id, name):
    """Only the bound capable client in a production installer gets hooks.

    Older payloads cannot interpret maintenance flags. Recovery fixtures must
    never publish the user's real relay: handler as an incidental test effect.
    """
    enabled = (client_build is not None and client_build.get('invitation_protocol') == 1
               and app_id == APP_ID and name == 'Relay')
    return '/DRelayRegisterProtocol=' + ('1' if enabled else '0')


def release_version_define(version):
    return '/DRelayVersion=' + release_version(version)


def direct_path(value):
    raw = os.fspath(value)
    if raw.startswith('\\\\?\\'):
        if not re.match(r'^[A-Za-z]:[\\/]', raw[4:]):
            raise ValueError('Network and device extended paths are refused.')
        raw = raw[4:]
    if raw.startswith(('\\\\', '//')):
        raise ValueError('Use a direct local path, not a network share.')
    path = Path(os.path.abspath(raw))
    if not path.is_absolute():
        raise ValueError('Use a direct local path.')
    if os.name == 'nt':
        import ctypes
        from ctypes import wintypes
        if path.drive.upper() not in ('C:', 'D:'):
            raise ValueError('Only the permitted local C: and D: drives may be used.')
        get_drive_type = ctypes.WinDLL('kernel32', use_last_error=True).GetDriveTypeW
        get_drive_type.argtypes = [wintypes.LPCWSTR]
        get_drive_type.restype = wintypes.UINT
        if get_drive_type(path.drive + '\\') != 3:
            raise ValueError('Use a fixed local drive, not a mapped share.')
    for parent in reversed([path, *path.parents]):
        try:
            info = os.lstat(parent)
        except FileNotFoundError:
            continue
        if stat.S_ISLNK(info.st_mode) or getattr(info, 'st_file_attributes', 0) & 1024:
            raise ValueError('Path contains a redirect: ' + str(parent))
        if not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)):
            raise ValueError('Path contains a nonregular filesystem object: ' + str(parent))
        if stat.S_ISREG(info.st_mode) and info.st_nlink != 1:
            raise ValueError('Hard-linked files are refused: ' + str(parent))
    return path


def regular_file(info):
    if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1
            or stat.S_ISLNK(info.st_mode)
            or getattr(info, 'st_file_attributes', 0) & 1024):
        raise ValueError('A direct, regular, single-hardlink file is required.')


def windows_read_handle(path, directory):
    """Open the actual Windows object without following a reparse point."""
    import ctypes
    from ctypes import wintypes

    class FileInformation(ctypes.Structure):
        _fields_ = [('attributes', wintypes.DWORD), ('creation', wintypes.FILETIME),
                    ('access', wintypes.FILETIME), ('write', wintypes.FILETIME),
                    ('volume', wintypes.DWORD), ('size_high', wintypes.DWORD),
                    ('size_low', wintypes.DWORD), ('links', wintypes.DWORD),
                    ('index_high', wintypes.DWORD), ('index_low', wintypes.DWORD)]

    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    create = kernel.CreateFileW
    create.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                       ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    create.restype = wintypes.HANDLE
    close = kernel.CloseHandle
    close.argtypes = [wintypes.HANDLE]
    close.restype = wintypes.BOOL
    information = kernel.GetFileInformationByHandle
    information.argtypes = [wintypes.HANDLE, ctypes.POINTER(FileInformation)]
    information.restype = wintypes.BOOL
    file_type = kernel.GetFileType
    file_type.argtypes = [wintypes.HANDLE]
    file_type.restype = wintypes.DWORD
    # Share-read only pins ancestors against rename/reparse modification and the
    # record against concurrent writes. No object is created or inherited.
    handle = create(str(path), 0x80 if directory else 0x80000000, 1, None, 3,
                    0x00200000 | 0x02000000, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    info = FileInformation()
    try:
        if not information(handle, ctypes.byref(info)):
            raise ctypes.WinError(ctypes.get_last_error())
        if (file_type(handle) != 1 or info.attributes & 1024
                or bool(info.attributes & 16) != directory
                or not directory and info.links != 1):
            raise ValueError('Redirected, nonregular, or hard-linked files are refused.')
    except BaseException:
        close(handle)
        raise
    return handle, close


@contextmanager
def direct_reader(value):
    path = direct_path(value)
    with ExitStack() as opened:
        if os.name == 'nt':
            import msvcrt
            for parent in reversed(path.parents):
                handle, close = windows_read_handle(parent, True)
                opened.callback(close, handle)
            handle, close = windows_read_handle(path, False)
            try:
                descriptor = msvcrt.open_osfhandle(handle, os.O_RDONLY | os.O_BINARY)
            except BaseException:
                close(handle)
                raise
        else:
            descriptor = os.open(path, os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0)
                                 | getattr(os, 'O_NONBLOCK', 0))
        stream = opened.enter_context(os.fdopen(descriptor, 'rb'))
        regular_file(os.fstat(stream.fileno()))
        yield stream


def digest_size(path):
    with direct_reader(path) as stream:
        size = os.fstat(stream.fileno()).st_size
        return hashlib.file_digest(stream, 'sha256').hexdigest(), size


def digest(path):
    return digest_size(path)[0]


def relative_name(value):
    if not isinstance(value, str) or not value or len(value) > 240:
        raise ValueError('Unsafe or oversized payload name.')
    parts = value.split('/')
    for part in parts:
        if (not 1 <= len(part) <= 120 or part in ('.', '..')
                or not re.fullmatch(r'[A-Za-z0-9 _.-]+', part)
                or part.endswith((' ', '.'))):
            raise ValueError('Unsafe payload path component.')
        device = part.split('.', 1)[0].strip(' ').upper()
        if device in ('CON', 'PRN', 'AUX', 'NUL') or re.fullmatch(r'(COM|LPT)[1-9]', device):
            raise ValueError('DOS device payload names are refused.')
    if value in BINARY_NAMES or value in DISTRIBUTION_BINARIES or value in OPTIONAL_BINARIES:
        return value
    if value == DISTRIBUTION_GUIDE:
        return value
    if value.startswith('scripts/') and len(parts) == 2 and value.endswith('.ps1'):
        return value
    if value == 'sources/Relay-source.zip':
        return value
    if value.startswith('licenses/') and len(parts) == 2 and value.endswith(('.txt', '.md')):
        return value
    if '/' not in value and value.endswith(('.md', '.json')):
        return value
    if value == 'Start Relay.cmd':
        return value
    raise ValueError('Unexpected payload file: ' + value)


def manifest_files(contents):
    files = contents.get('files')
    if not isinstance(files, dict) or not 4 <= len(files) <= 64:
        raise ValueError('Incomplete or oversized release manifest.')
    distribution = contents.get('schema') == 1 and contents.get('entry_point') == 'Relay.exe'
    required = DISTRIBUTION_BINARIES if distribution else BINARY_NAMES
    if not required <= files.keys():
        raise ValueError('Incomplete executable payload.')
    if distribution:
        if 'sources/Relay-source.zip' in files or 'build-info.json' not in files:
            raise ValueError('Binary distribution requires build metadata and no source archive.')
        if DISTRIBUTION_GUIDE not in files:
            raise ValueError('Binary distribution requires the short LAES-MIG.txt user guide.')
        if any(not isinstance(name, str) or name.casefold() in INTERNAL_DOCUMENTATION for name in files):
            raise ValueError('Binary distribution must not include obsolete guides or internal development notes.')
        for item in files.values():
            if (not isinstance(item, dict) or set(item) != {'sha256', 'bytes'}
                    or not isinstance(item['bytes'], int) or isinstance(item['bytes'], bool)
                    or not 0 <= item['bytes'] <= 2 * 1024 ** 3):
                raise ValueError('Invalid distribution payload size.')
        files = {name:item['sha256'] for name,item in files.items()}
    elif 'sources/Relay-source.zip' not in files:
        raise ValueError('A source-included or current binary distribution is required.')
    return files, distribution


def pascal(value):
    return "'" + str(value).replace("'", "''") + "'"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--payload', required=True)
    parser.add_argument('--manifest', required=True)
    parser.add_argument('--compiler', required=True)
    parser.add_argument('--output', required=True)
    parser.add_argument('--sequence', required=True)
    parser.add_argument('--require-build-identity', action='store_true', help='Require a hash-bound client --build-info record matching this installer sequence.')
    parser.add_argument('--version', help='Explicit source release version; must match source Cargo. Different control-root version is isolated recovery-fixture only.')
    parser.add_argument('--app-id', default=APP_ID, help='Override only for isolated installer tests.')
    parser.add_argument('--name', default='Relay')
    parser.add_argument('--recovery-probe', choices=RECOVERY_PROBES, help='Isolated fixture-only installer interruption probe.')
    args = parser.parse_args()
    build_sequence(args.sequence)
    app_id = str(uuid.UUID(args.app_id))
    if args.recovery_probe is not None and (args.app_id != RECOVERY_APP_ID or args.name != RECOVERY_APP_NAME):
        raise ValueError('Recovery probes require the exact isolated fixture GUID and display name.')
    if not re.fullmatch(r'[A-Za-z0-9 _-]{1,60}', args.name):
        raise ValueError('Unsafe application display name.')
    payload, manifest, compiler, output = map(direct_path, [args.payload, args.manifest, args.compiler, args.output])
    if output.exists():
        raise ValueError('Choose a new output directory; prior installers are immutable.')
    with direct_reader(manifest) as stream:
        contents = json.loads(stream.read().decode('utf-8'))
    files, distribution = manifest_files(contents)
    if distribution:
        if manifest != direct_path(payload / 'manifest.json'):
            raise ValueError('Use the binary distribution manifest inside its payload.')
        files['manifest.json'] = digest(manifest)
    case_names, total = set(), 0
    for name, expected in files.items():
        relative_name(name)
        if name.casefold() in case_names:
            raise ValueError('Case-colliding payload names.')
        case_names.add(name.casefold())
        path = direct_path(payload / name)
        if not isinstance(expected, str) or not re.fullmatch('[0-9a-f]{64}', expected):
            raise ValueError('Payload integrity mismatch: ' + name)
        actual, size = digest_size(path)
        if actual != expected or distribution and name != 'manifest.json' and size != contents['files'][name]['bytes']:
            raise ValueError('Payload integrity mismatch: ' + name)
        total += size
    if total > 2 * 1024 ** 3:
        raise ValueError('Payload exceeds installer budget.')
    root = direct_path(Path(__file__).parent.parent)
    workspace_version, workspace_cargo_hash = workspace_release(root)
    if distribution:
        with direct_reader(payload / 'build-info.json') as stream:
            raw_metadata = stream.read(65537)
        if len(raw_metadata) > 65536:
            raise ValueError('Build metadata exceeds its size budget.')
        metadata = json.loads(raw_metadata.decode('utf-8-sig'))
        source_version = release_version(metadata['version'])
    else:
        source_version = payload_release(payload, files)
    version = select_release_version(workspace_version, source_version, args.version, app_id, args.name)
    if args.require_build_identity and not distribution:
        raise ValueError('Required client build identity is available only for binary distributions.')
    client_build = bound_client_build(metadata, files, version, args.sequence, args.require_build_identity) if distribution else None
    script = direct_path(root / 'deploy/windows/Relay.iss')
    recovery_script = direct_path(root / 'deploy/windows/Recovery.iss')
    stop_script = direct_path(root / 'deploy/windows/StopLocalServer.ps1')
    icon = direct_path(root / 'crates/relay-client/assets/relay.ico')
    license_file = direct_path(root / 'LICENSE')
    compiler_inputs = {
        'source_script_sha256': script,
        'recovery_script_sha256': recovery_script,
        'stop_script_sha256': stop_script,
        'builder_sha256': direct_path(__file__),
        'compiler_sha256': compiler,
        'icon_sha256': icon,
        'license_sha256': license_file,
        'workspace_cargo_sha256': direct_path(root / 'Cargo.toml'),
    }
    before_hashes = {name: digest(path) for name, path in compiler_inputs.items()}
    if before_hashes['workspace_cargo_sha256'] != workspace_cargo_hash:
        raise RuntimeError('Source Cargo version changed before compiler inputs were frozen.')

    def require_unchanged_compiler_inputs():
        changed = [name for name, path in compiler_inputs.items() if digest(path) != before_hashes[name]]
        if changed:
            raise RuntimeError('Installer build inputs changed; no completed build record: ' + ', '.join(changed))

    payload_hash = hashlib.sha256(json.dumps(files, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
    output.mkdir(parents=True)
    stage = direct_path(output / 'stage'); stage.mkdir()
    generated, validation, path_validation, final_validation = [], [], [], []
    entry = 'Relay.exe' if distribution else 'relay.exe'
    names = [entry] + sorted(set(files) - {entry})
    server_name = 'dist/relay-server.exe' if distribution else 'relay-server.exe'
    stop_payload = f'verify-{names.index(server_name):02d}.payload'
    for index, name in enumerate(names):
        expected = files[name]
        path = direct_path(stage / name); path.parent.mkdir(parents=True, exist_ok=True)
        with direct_reader(payload / name) as source, path.open('xb') as target:
            shutil.copyfileobj(source, target)
        if digest(path) != expected:
            raise ValueError('Payload changed during staging: ' + name)
        relative = name.replace('/', '\\')
        parent = relative.rpartition('\\')[0]
        destination = '{app}' if not parent else '{app}\\' + parent
        temp_name = f'verify-{index:02d}.payload'
        hook = '; BeforeInstall: InstallationCheckFile(' + pascal(relative) + ')'
        if name == ('dist/relay.exe' if distribution else 'relay.exe'):
            hook += '; AfterInstall: InstallationPayloadInstalled'
        generated.append(f'Source: "{path}"; DestDir: "{destination}"; Flags: ignoreversion' + hook)
        generated.append(f'Source: "{path}"; DestDir: "{{tmp}}"; DestName: "{temp_name}"; Flags: dontcopy')
        validation.extend([
            '  if not InstallationRegularPath(ExpandConstant(' + pascal('{app}\\' + relative) + '), True, Error) then begin Result := Error; exit; end;',
            '  ExtractTemporaryFile(' + pascal(temp_name) + ');',
            '  if not InstallationRegularPath(ExpandConstant(' + pascal('{tmp}\\' + temp_name) + '), False, Error) then begin Result := Error; exit; end;',
            '  if InstallationHash(ExpandConstant(' + pascal('{tmp}\\' + temp_name) + ')) <> ' + pascal(expected) + " then begin Result := 'The release files could not be verified. Installation was left unchanged.'; exit; end;",
        ])
        path_validation.append('  if not InstallationRegularPath(AddBackslash(Path) + ' + pascal(relative) + ', True, Error) then exit;')
        final_validation.extend([
            '  if not InstallationRegularPath(AddBackslash(Path) + ' + pascal(relative) + ', False, Error) then begin Result := False; exit; end;',
            '  if InstallationHash(AddBackslash(Path) + ' + pascal(relative) + ') <> ' + pascal(expected) + " then begin Error := 'The installed release files could not be verified. Run the Relay installer again.'; Result := False; exit; end;",
        ])
    marker = release_marker(app_id, version, args.sequence, payload_hash)
    marker_path = direct_path(stage / 'relay-installation.ini'); marker_path.write_text(marker, encoding='ascii', newline='')
    marker_hash = digest(marker_path)
    generated.append(f'Source: "{marker_path}"; DestDir: "{{app}}"; Flags: ignoreversion; BeforeInstall: InstallationCheckFile(\'relay-installation.ini\'); AfterInstall: InstallationMarkerInstalled')
    path_validation.append("  if not InstallationRegularPath(AddBackslash(Path) + 'relay-installation.ini', True, Error) then exit;")
    final_validation.extend([
        "  if not InstallationRegularPath(AddBackslash(Path) + 'relay-installation.ini', False, Error) then begin Result := False; exit; end;",
        "  if InstallationHash(AddBackslash(Path) + 'relay-installation.ini') <> " + pascal(marker_hash) + " then begin Error := 'The installed ownership record could not be verified. Run the Relay installer again.'; Result := False; exit; end;",
    ])
    direct_path(stage / 'generated-files.iss').write_text('\n'.join(generated) + '\n', encoding='utf-8-sig')
    direct_path(stage / 'generated-validation.iss').write_text('\n'.join(validation) + '\n', encoding='utf-8-sig')
    direct_path(stage / 'generated-path-validation.iss').write_text('\n'.join(path_validation) + '\n', encoding='utf-8-sig')
    direct_path(stage / 'generated-final-validation.iss').write_text('\n'.join(final_validation) + '\n', encoding='utf-8-sig')
    staged_stop = direct_path(stage / 'stop-local-server.ps1')
    with direct_reader(stop_script) as source, staged_stop.open('xb') as target:
        shutil.copyfileobj(source, target)
    if digest(staged_stop) != before_hashes['stop_script_sha256']:
        raise RuntimeError('Installer shutdown helper changed during staging.')
    with direct_path(stage / 'generated-files.iss').open('a', encoding='utf-8-sig') as target:
        target.write(f'Source: "{staged_stop}"; DestDir: "{{tmp}}"; DestName: "stop-local-server.ps1"; Flags: dontcopy\n')
    before = ('Install Relay for your Windows account. Run a newer installer later to update the same installation; you do not need to uninstall first.\r\n\r\n'
              'Your accounts, conversations, encryption keys and server data are kept in their existing folders.\r\n\r\n'
              'Close the Relay window before updating. Setup can stop this installation\'s local server normally, then continue. No process is force-closed. Accounts, messages and keys are kept.\r\n\r\n'
              'This preview is not digitally signed.\r\n')
    direct_path(stage / 'before.txt').write_text(before, encoding='utf-8-sig', newline='')

    command = [str(compiler), '/Qp', '/DStage=' + str(stage), '/DOutput=' + str(output), release_version_define(version), '/DReleaseSequence=' + args.sequence, '/DReleasePayloadHash=' + payload_hash, '/DRelayAppId=' + app_id, '/DRelayAppName=' + args.name, '/DRelayDisplayIcon=' + (r'dist\relay.exe' if distribution else 'relay.exe'), '/DRelayIcon=' + str(icon), '/DRelayLicense=' + str(license_file), str(script)]
    command.insert(-1, protocol_registration_define(client_build, app_id, args.name))
    command.insert(-1, '/DRelayClientExecutable=' + (r'dist\relay.exe' if distribution else 'relay.exe'))
    command.insert(-1, '/DRelayStopServerPayload=' + stop_payload)
    command.insert(-1, '/DRelayStopServerSHA256=' + files[server_name])
    command.insert(-1, '/DRelayStopScriptSHA256=' + before_hashes['stop_script_sha256'])
    if args.recovery_probe is not None:
        command.insert(-1, '/DRelayRecoveryProbe=' + args.recovery_probe)
    require_unchanged_compiler_inputs()
    completed = subprocess.run(command, capture_output=True, text=True, encoding='utf-8', errors='replace', creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0)
    direct_path(output / 'compiler.log').write_text(completed.stdout + completed.stderr, encoding='utf-8')
    require_unchanged_compiler_inputs()
    if completed.returncode:
        raise RuntimeError('Installer compiler failed; see compiler.log.')
    installer = direct_path(output / installer_filename(version, args.sequence))
    if not installer.is_file():
        raise RuntimeError('Compiler did not create the named installer.')
    record = {'app_id': app_id, 'display_name': args.name, 'version': version, 'workspace_version': workspace_version, 'source_archive_version': None if distribution else source_version, 'binary_distribution': distribution, 'sequence': args.sequence, 'client_build': client_build, 'client_build_identity_verified': client_build is not None, 'payload_sha256': payload_hash, 'payload_files': files, 'payload_bytes': total, **before_hashes, 'compiler_inputs_verified_unchanged': True, 'recovery_probe': args.recovery_probe, 'installer': str(installer), 'installer_sha256': digest(installer), 'publisher_verified_or_signed': False}
    direct_path(output / 'build.json').write_text(json.dumps(record, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({'installer': str(installer), 'sha256': record['installer_sha256'], 'files': len(files), 'signed': False}))


def version_self_tests():
    """Pure version regressions: no filesystem writes or compiler/process calls."""
    import unittest

    class ReleaseVersionTests(unittest.TestCase):
        def test_invitation_hooks_require_bound_capability_and_exact_production_identity(self):
            enabled = {'invitation_protocol': 1}
            self.assertEqual(protocol_registration_define(enabled, APP_ID, 'Relay'), '/DRelayRegisterProtocol=1')
            for info, app_id, name in ((None, APP_ID, 'Relay'), ({}, APP_ID, 'Relay'),
                                       ({'invitation_protocol': 0}, APP_ID, 'Relay'),
                                       (enabled, RECOVERY_APP_ID, RECOVERY_APP_NAME),
                                       (enabled, APP_ID, RECOVERY_APP_NAME)):
                self.assertEqual(protocol_registration_define(info, app_id, name), '/DRelayRegisterProtocol=0')

        def test_client_build_identity_matches_actual_payload_hash_version_and_sequence(self):
            sequence = '20261015123456'
            source = 'b' * 64
            info = {'schema':1, 'version':'0.10.0', 'build_id':sequence+'-'+source[:12],
                    'sequence':sequence, 'built_at':'2026-10-15T12:34:56Z', 'build_date':'2026-10-15',
                    'source_sha256':source, 'source_scope':'relay-workspace-source-v1',
                    'git_revision':'0123456789ab', 'git_dirty':True}
            files = {'dist/relay.exe':'a' * 64}
            metadata = {'version':'0.10.0', 'client_sha256':'a' * 64, 'client_build':info}
            self.assertEqual(bound_client_build(metadata, files, '0.10.0', sequence, True), info)
            capable = {**info, 'invitation_protocol': 1}
            self.assertEqual(bound_client_build({**metadata, 'client_build': capable}, files,
                                               '0.10.0', sequence, True), capable)
            for value in (True, False, None, '1', 2, -1):
                with self.subTest(capability=value), self.assertRaises(ValueError):
                    bound_client_build({**metadata, 'client_build': {**info, 'invitation_protocol': value}},
                                       files, '0.10.0', sequence, True)
            client_inputs = {**info, 'source_scope':'relay-client-inputs-v2'}
            self.assertEqual(bound_client_build({**metadata, 'client_build':client_inputs}, files,
                                               '0.10.0', sequence, True), client_inputs)
            for key, value in (('schema',True), ('version','0.10.1'), ('sequence','20261015123457'),
                               ('built_at','2026-10-15T12:34:57Z'), ('build_date','2026-10-14'),
                               ('source_sha256','z'*64), ('source_scope','unknown'), ('build_id','wrong'),
                               ('git_revision','nogit'), ('git_dirty',1)):
                with self.subTest(key=key), self.assertRaises(ValueError):
                    bound_client_build({**metadata, 'client_build':{**info,key:value}}, files, '0.10.0', sequence, True)
            with self.assertRaises(ValueError):
                bound_client_build(metadata, {'dist/relay.exe':'c'*64}, '0.10.0', sequence, True)
            with self.assertRaises(ValueError):
                bound_client_build({**metadata, 'client_build':{k:v for k,v in info.items() if k != 'sequence'}}, files, '0.10.0', sequence, True)
            no_git = {**info, 'git_revision':'nogit', 'git_dirty':None}
            self.assertEqual(bound_client_build({**metadata,'client_build':no_git}, files, '0.10.0', sequence), no_git)

        def test_legacy_build_identity_is_explicitly_optional_and_required_mode_rejects_it(self):
            self.assertIsNone(bound_client_build({'version':'0.10.0'}, {}, '0.10.0', '20261015123456'))
            with self.assertRaises(ValueError):
                bound_client_build({'version':'0.10.0'}, {}, '0.10.0', '20261015123456', True)

        def test_release_sequence_is_an_actual_utc_second(self):
            for sequence in ('20260229000000', '20261301000000', '20261015240000',
                             '20261015126000', '20261015123460', '19691231235959', '20261015', True):
                with self.subTest(sequence=sequence), self.assertRaises(ValueError):
                    build_sequence(sequence)
            self.assertEqual(build_sequence('20240229235959').isoformat(), '2024-02-29T23:59:59+00:00')

        def test_current_binary_distribution_does_not_require_or_include_sources(self):
            files = {name:{'sha256':'a' * 64,'bytes':1} for name in DISTRIBUTION_BINARIES | {'build-info.json', DISTRIBUTION_GUIDE}}
            manifest = {'schema':1,'entry_point':'Relay.exe','files':files}
            normalized, distribution = manifest_files(manifest)
            self.assertTrue(distribution)
            self.assertEqual(set(normalized), set(files))
            self.assertTrue(all(value == 'a' * 64 for value in normalized.values()))
            for name in files:
                self.assertEqual(relative_name(name), name)
            for missing in DISTRIBUTION_BINARIES:
                with self.subTest(missing=missing), self.assertRaises(ValueError):
                    manifest_files({**manifest, 'files':{k:v for k,v in files.items() if k != missing}})
            with self.assertRaises(ValueError):
                manifest_files({**manifest,'files':{**files,'sources/Relay-source.zip':{'sha256':'b'*64,'bytes':1}}})
            for size in (-1, True, '1', 2 * 1024**3 + 1):
                with self.subTest(size=size), self.assertRaises(ValueError):
                    manifest_files({**manifest,'files':{**files,'build-info.json':{'sha256':'a'*64,'bytes':size}}})
            legacy = {name:'a'*64 for name in BINARY_NAMES | {'sources/Relay-source.zip'}}
            self.assertEqual(manifest_files({'files':legacy}), (legacy, False))

        def test_installer_payload_rejects_unsafe_or_unknown_new_paths(self):
            for name in ['../relay.exe','dist/../../relay.exe','dist/unknown.exe','scripts/../secret.ps1','scripts/private.key','dist/CON.exe','dist/relay.exe:stream']:
                with self.subTest(name=name), self.assertRaises(ValueError):
                    relative_name(name)

        def test_old_new_and_beta_releases_are_generic(self):
            for version in ('0.9.9', '0.10.0', '0.10.1', '1.0.0-beta.1',
                            '2.0.0-rc.0.alpha-1', '18446744073709551615.0.0'):
                self.assertEqual(release_version(version), version)

        def test_invalid_semver_and_output_injection_are_refused(self):
            for version in ('', '0.10', '0.10.1.0', 'v0.10.1', '01.10.1', '0.010.1',
                            '0.10.01', '18446744073709551616.0.0', '0.10.1-',
                            '0.10.1-beta..1', '0.10.1-.beta', '0.10.1-beta.',
                            '0.10.1-beta.01', '0.10.1-beta_1', '0.10.1+build.1',
                            '0.10.1-beta.1+build.1', '0.10.1 beta', '0.10.1\r',
                            '0.10.1-é', '0.10.1\"/DRelayAppId=foreign'):
                with self.subTest(version=version), self.assertRaises(ValueError):
                    release_version(version)
                with self.assertRaises(ValueError):
                    release_version_define(version)

        def test_version_size_and_numeric_boundaries(self):
            longest = '1.2.3-' + 'a' * (MAX_RELEASE_VERSION_BYTES - 6)
            self.assertEqual(release_version(longest), longest)
            with self.assertRaises(ValueError):
                release_version(longest + 'a')
            self.assertEqual(release_version('18446744073709551615.18446744073709551615.0'),
                             '18446744073709551615.18446744073709551615.0')

        def test_workspace_version_comes_from_structured_cargo_metadata(self):
            raw = b'version="99.99.99"\n[workspace.package]\nversion="0.10.1"\n'
            self.assertEqual(cargo_release_version(raw), '0.10.1')
            for invalid in (b'[package]\nversion="0.10.1"\n',
                            b'[workspace.package]\nversion="0.10.01"\n',
                            b'[workspace.package]\nversion=10\n', b'x' * (MAX_CARGO_BYTES + 1)):
                with self.assertRaises((ValueError, tomllib.TOMLDecodeError)):
                    cargo_release_version(invalid)

        def test_controlled_override_requires_real_source_version_and_exact_fixture(self):
            self.assertEqual(select_release_version('0.10.1', '0.10.1', None, APP_ID, 'Relay'), '0.10.1')
            self.assertEqual(select_release_version('0.10.1', '0.10.1', '0.10.1', APP_ID, 'Relay'), '0.10.1')
            self.assertEqual(select_release_version('0.10.1', '0.10.0', '0.10.0',
                                                    RECOVERY_APP_ID, RECOVERY_APP_NAME), '0.10.0')
            for app_id, name in ((APP_ID, 'Relay'), (RECOVERY_APP_ID, 'Relay'),
                                 ('b751c2ba-0384-4942-8e02-9e3770126465', RECOVERY_APP_NAME)):
                with self.assertRaises(ValueError):
                    select_release_version('0.10.0', '0.10.1', '0.10.1', app_id, name)
            with self.assertRaises(ValueError):
                select_release_version('0.10.1', '0.10.0', None, APP_ID, 'Relay')

        def test_incoming_version_is_consistent_in_marker_compiler_and_filename(self):
            for version in ('0.10.0', '0.10.1', '0.11.0-beta.2'):
                selected = select_release_version(version, version, None, APP_ID, 'Relay')
                marker = release_marker(APP_ID, selected, '20261009123456', 'a' * 64)
                self.assertIn('\r\nVersion=' + version + '\r\n', marker)
                self.assertIn('\r\nSequence=20261009123456\r\n', marker)
                self.assertIn('\r\nPayloadSHA256=' + 'a' * 64 + '\r\n', marker)
                self.assertEqual(release_version_define(selected), '/DRelayVersion=' + version)
                self.assertEqual(installer_filename(selected, '20261009123456'),
                                 'Relay-' + version + '-20261009123456-Setup.exe')

    suite = unittest.defaultTestLoader.loadTestsFromTestCase(ReleaseVersionTests)
    return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1


if __name__ == '__main__':
    if sys.argv[1:] == ['--self-test']:
        raise SystemExit(version_self_tests())
    main()
