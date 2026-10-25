#!/usr/bin/env python3
"""Prepare Relay's signed update repository locally; never upload or install.

Requires the existing Python cryptography package and Windows current-user
DPAPI. Explicit `init` creates new publisher keys; importing/running --help does
not. Keep --store outside all source/publication folders and back it up safely.
DPAPI recovery requires the same Windows user/profile; copying its encrypted
files to a different machine is NOT a portable recovery plan. Two different
root keys in one store meet the cryptographic threshold, not independent custody.

Example (operator selects new, direct C:/D: paths):
  python publish-updates.py init --store C:/private/relay-publisher --output C:/work/relay-root
  python publish-updates.py publish --store C:/private/relay-publisher --channel beta \
    --package C:/work/release/package.json --installer C:/work/release/Relay-Setup.exe \
    --notes C:/work/release/update-notes.txt --output C:/work/update-staging

Publish ONLY output/repository/ under https://thebengine.github.io/Relay/updates/CHANNEL/.
Public policy.json/root.json are candidates for the compiled app, not runtime
trust discovery. GitHub release download redirects are deliberately not used.
Preserve existing immutable installer targets and numbered root files; replace
targets/snapshot before timestamp (or publish one atomic Pages deployment).
Re-run publish with the identical package/notes before timestamp expiry to
refresh metadata. It reserves a new monotonically increasing version even if
an earlier staged generation was never uploaded. No scheduler is installed.

TUF authenticates publisher-approved bytes. It is not Windows Authenticode:
an unsigned installer may still show Windows reputation/security prompts.
"""
import argparse
import contextlib
import csv
import datetime as dt
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey

from dev_profile_core import direct
from friends_backup_crypto import dpapi

APP_ID = '3bed570c-afb7-4f36-8a88-8c6f1772d6af'
BASE = 'https://thebengine.github.io/Relay/updates/'
ROLES = ('targets', 'snapshot', 'timestamp')
MAX_JSON = 2 * 1024 * 1024
# A directly hosted Pages Git blob must also fit GitHub's ordinary file limit.
MAX_INSTALLER = 100 * 1024 * 1024 - 1
HEX = re.compile(r'[0-9a-f]{64}')


class PublisherError(Exception):
    """Safe, non-secret operator diagnostics."""


def require(condition, message):
    if not condition:
        raise PublisherError(message)


def local(value, missing=True):
    return direct(str(Path(value)), missing)


def separate(store, output):
    require(store != output and store not in output.parents and output not in store.parents,
            'Private store and public output must be separate directories.')


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, 'Duplicate JSON member refused.')
        result[key] = value
    return result


def decode(raw):
    return json.loads(raw, object_pairs_hook=unique_object)


def encoded(value):
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':')) + '\n').encode('utf-8')


def canonical(value):
    # The generated signed metadata uses ASCII strings without control bytes.
    # In this subset sorted compact JSON is also OLPC canonical JSON (tough).
    # Release notes are a hashed target, never inserted in signed metadata.
    def check(item):
        if isinstance(item, str):
            require(all(32 <= ord(char) < 127 for char in item), 'Noncanonical metadata string refused.')
        elif isinstance(item, dict):
            for key, child in item.items():
                require(isinstance(key, str), 'Metadata keys must be strings.')
                check(key)
                check(child)
        elif isinstance(item, list):
            for child in item:
                check(child)
        else:
            require(type(item) in (int, bool) or item is None, 'Noncanonical metadata value refused.')
    check(value)
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode('utf-8')


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def read(path, limit=MAX_JSON):
    path = local(path, False)
    require(path.is_file() and path.stat().st_size <= limit, 'Input file is missing, nonregular or too large.')
    with path.open('rb') as stream:
        raw = stream.read(limit + 1)
    require(len(raw) <= limit, 'Input file grew beyond its limit.')
    return raw


def write_new(path, raw):
    path = local(path)
    local(path.parent, False)
    with path.open('xb') as stream:
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())


def make_directory(path):
    path = local(path)
    local(path.parent, False)
    path.mkdir()
    return path


def restrict_store(path):
    """New folder only: remove inherited ACLs; grant this user and SYSTEM."""
    whoami = direct(r'C:\Windows\System32\whoami.exe', False, _system_tool=True)
    icacls = direct(r'C:\Windows\System32\icacls.exe', False, _system_tool=True)
    response = subprocess.run([str(whoami), '/user', '/fo', 'csv', '/nh'], check=True,
                              capture_output=True, text=True, timeout=15)
    rows = list(csv.reader(io.StringIO(response.stdout)))
    require(len(rows) == 1 and len(rows[0]) == 2 and re.fullmatch(r'S-1-[0-9-]+', rows[0][1]),
            'Could not determine the current Windows user SID.')
    subprocess.run([str(icacls), str(path), '/inheritance:r', '/grant:r',
                    '*' + rows[0][1] + ':(OI)(CI)F', '*S-1-5-18:(OI)(CI)F'],
                   check=True, capture_output=True, timeout=15)


def seed(key):
    return key.private_bytes(serialization.Encoding.Raw, serialization.PrivateFormat.Raw,
                             serialization.NoEncryption())


def public_key(key):
    return {'keytype': 'ed25519', 'scheme': 'ed25519', 'keyval': {'public':
            key.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw).hex()}}


def key_id(key):
    return digest(canonical(public_key(key)))


def sign(value, keys):
    raw = canonical(value)
    return encoded({'signed': value, 'signatures': [
        {'keyid': key_id(key), 'sig': key.sign(raw).hex()} for key in keys]})


def expiry(now, days):
    return (now + dt.timedelta(days=days)).strftime('%Y-%m-%dT%H:%M:%SZ')


def instant(value):
    return dt.datetime.strptime(value, '%Y-%m-%dT%H:%M:%SZ').replace(tzinfo=dt.timezone.utc)


def root_metadata(roots, online, now):
    keys = [*roots, *online.values()]
    require(len({key_id(key) for key in keys}) == 5, 'Publisher roles require five distinct keys.')
    return sign({'_type': 'root', 'spec_version': '1.0.0', 'version': 1,
                 'expires': expiry(now, 365), 'consistent_snapshot': False,
                 'keys': {key_id(key): public_key(key) for key in keys},
                 'roles': {'root': {'keyids': [key_id(key) for key in roots], 'threshold': 2},
                           **{role: {'keyids': [key_id(online[role])], 'threshold': 1} for role in ROLES}}}, roots)


def verify(raw, root, role):
    envelope = decode(raw)
    body = envelope['signed']
    policy = root['roles'][role]
    require(type(policy['threshold']) is int and policy['threshold'] >= (2 if role == 'root' else 1),
            'Publisher signature threshold is insufficient.')
    allowed = set(policy['keyids'])
    accepted = set()
    materials = set()
    for signer in allowed:
        key = root['keys'][signer]
        require(key['keytype'] == key['scheme'] == 'ed25519' and digest(canonical(key)) == signer,
                'Publisher key identity is invalid.')
        material = bytes.fromhex(key['keyval']['public'])
        require(len(material) == 32 and material not in materials, 'Publisher key aliases refused.')
        materials.add(material)
    for signature in envelope['signatures']:
        signer = signature['keyid']
        if signer in allowed and signer not in accepted:
            key = root['keys'][signer]
            Ed25519PublicKey.from_public_bytes(bytes.fromhex(key['keyval']['public'])).verify(
                bytes.fromhex(signature['sig']), canonical(body))
            accepted.add(signer)
    require(len(accepted) >= policy['threshold'], 'Publisher signatures are incomplete.')
    require(body['_type'] == role, 'Metadata role differs from its signature policy.')
    return body


def save_online(store, state):
    encrypted = dpapi(encoded(state), True)
    target = local(store / 'online.dpapi')
    fd, temporary = tempfile.mkstemp(prefix='.online-', dir=store)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(encrypted)
            stream.flush()
            os.fsync(stream.fileno())
        local(temporary, False)
        os.replace(temporary, target)
    finally:
        if Path(temporary).exists():
            local(temporary, False).unlink()


def init(args):
    store, output = local(args.store), local(args.output)
    separate(store, output)
    require(not store.exists() and not output.exists(), 'Init requires new private and public directories.')
    # Restrict access before generating keys. A partial initialization is kept
    # for operator inspection and is never silently replaced.
    make_directory(store)
    restrict_store(store)
    make_directory(output)
    roots = [Ed25519PrivateKey.generate(), Ed25519PrivateKey.generate()]
    online = {role: Ed25519PrivateKey.generate() for role in ROLES}
    root = root_metadata(roots, online, dt.datetime.now(dt.timezone.utc))
    verify(root, decode(root)['signed'], 'root')
    for index, key in enumerate(roots, 1):
        write_new(store / f'root-{index}.dpapi', dpapi(b'relay-publisher-root-v1\0' + seed(key), True))
    make_directory(store / 'roots')
    write_new(store / 'roots/1.root.json', root)
    state = {'schema': 1, 'purpose': 'relay-update-publisher-v1', 'initial_root_sha256': digest(root),
             'keys': {role: seed(key).hex() for role, key in online.items()}, 'channels': {}}
    save_online(store, state)
    write_new(output / 'root.json', root)
    write_new(output / '1.root.json', root)
    return {'status': 'publisher keys created and DPAPI protected', 'root_sha256': digest(root),
            'root_expires': decode(root)['signed']['expires'],
            'custody': 'Current Windows user; separate cryptographic keys, not independent custodians.'}


@contextlib.contextmanager
def locked(store):
    lock = local(store / '.publishing.lock')
    with lock.open('xb') as stream:
        stream.write(str(os.getpid()).encode('ascii'))
        stream.flush()
    try:
        yield
    finally:
        local(lock, False).unlink()


def version(value):
    require(isinstance(value, str) and len(value) <= 64
            and re.fullmatch(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)', value),
            'Expected the compiled three-part package version without build/prerelease suffixes.')
    parts = tuple(map(int, value.split('.')))
    require(all(part <= 2**64 - 1 for part in parts), 'Package version exceeds the client version bound.')
    return parts


def validate_release(release, previous):
    version(release['version'])
    sequence = release['sequence']
    require(isinstance(sequence, str) and re.fullmatch(r'[0-9]{14}', sequence), 'Invalid build sequence.')
    dt.datetime.strptime(sequence, '%Y%m%d%H%M%S')
    require(len(release['notes'].encode('utf-8')) <= 4096, 'Update notes exceed 4096 bytes.')
    require(type(release['installer_bytes']) is int and 0 < release['installer_bytes'] <= MAX_INSTALLER
            and isinstance(release['installer_sha256'], str) and HEX.fullmatch(release['installer_sha256']),
            'Installer size/hash is invalid or exceeds direct Pages hosting limits.')
    if previous is not None:
        require(sequence >= previous['sequence'] and version(release['version']) >= version(previous['version']),
                'A previous publisher build cannot be replaced with an older release.')
        require(sequence != previous['sequence'] or release == previous,
                'The same build sequence cannot describe different release contents; keep its original notes/handoff.')


def description(raw, metadata_version=None):
    value = {'length': len(raw), 'hashes': {'sha256': digest(raw)}}
    if metadata_version is not None:
        value['version'] = metadata_version
    return value


def prepare_metadata(root, online, release, metadata_version, now):
    require(type(metadata_version) is int and 0 < metadata_version < 2**53, 'Invalid metadata version.')
    require(instant(root['expires']) > now + dt.timedelta(days=90),
            'Renew the publisher root before creating metadata that outlives it.')
    release_bytes = encoded(release)
    target_map = {f"windows-x64/{release['channel']}/release.json": description(release_bytes),
                  release['installer_target']: {'length': release['installer_bytes'],
                                                'hashes': {'sha256': release['installer_sha256']}}}
    metadata = {}
    targets = {'_type': 'targets', 'spec_version': '1.0.0', 'version': metadata_version,
               'expires': expiry(now, 90), 'targets': target_map}
    metadata['targets.json'] = sign(targets, [online['targets']])
    snapshot = {'_type': 'snapshot', 'spec_version': '1.0.0', 'version': metadata_version,
                'expires': expiry(now, 30),
                'meta': {'targets.json': description(metadata['targets.json'], metadata_version)}}
    metadata['snapshot.json'] = sign(snapshot, [online['snapshot']])
    timestamp = {'_type': 'timestamp', 'spec_version': '1.0.0', 'version': metadata_version,
                 'expires': expiry(now, 30),
                 'meta': {'snapshot.json': description(metadata['snapshot.json'], metadata_version)}}
    metadata['timestamp.json'] = sign(timestamp, [online['timestamp']])
    for role in ROLES:
        verify(metadata[role + '.json'], root, role)
    return release_bytes, metadata


def copy_installer(source, destination, expected):
    source = local(source, False)
    destination = local(destination)
    hashed, count = hashlib.sha256(), 0
    with source.open('rb') as reader, destination.open('xb') as writer:
        require(reader.read(2) == b'MZ', 'The installer is not a Windows executable.')
        reader.seek(0)
        while block := reader.read(1024 * 1024):
            count += len(block)
            require(count <= expected['installer_bytes'], 'Installer exceeds the reviewed package size.')
            writer.write(block)
            hashed.update(block)
        writer.flush()
        os.fsync(writer.fileno())
    require(count == expected['installer_bytes'] and hashed.hexdigest() == expected['installer_sha256'],
            'Installer bytes differ from the reviewed package receipt.')


def publish(args):
    store, output = local(args.store, False), local(args.output)
    separate(store, output)
    require(not output.exists(), 'Choose a new output folder for each metadata generation.')
    with locked(store):
        state = decode(dpapi(read(store / 'online.dpapi', 65536), False))
        require(state['schema'] == 1 and state['purpose'] == 'relay-update-publisher-v1', 'Unknown publisher store.')
        require(set(state['keys']) == set(ROLES), 'The store must have separate online role keys.')
        online = {role: Ed25519PrivateKey.from_private_bytes(bytes.fromhex(state['keys'][role])) for role in ROLES}
        require(len({key_id(key) for key in online.values()}) == 3, 'Online role keys must be distinct.')
        # Numbered roots are retained verbatim. A later root must satisfy both
        # the previous root threshold and its own threshold before it is staged.
        roots_dir = local(store / 'roots', False)
        roots = {}
        root = None
        for entry in roots_dir.iterdir():
            require(re.fullmatch(r'[1-9][0-9]*\.root\.json', entry.name), 'Unexpected file in publisher root history.')
            roots[int(entry.name.split('.')[0])] = read(entry)
        require(sorted(roots) == list(range(1, len(roots) + 1)) and roots, 'Publisher root history has a gap.')
        require(digest(roots[1]) == state['initial_root_sha256'], 'Initial root differs from the protected store.')
        for number in sorted(roots):
            body = decode(roots[number])['signed']
            require(body['version'] == number and body['consistent_snapshot'] is False, 'Unsupported root history.')
            if root is not None:
                verify(roots[number], root, 'root')
            root = verify(roots[number], body, 'root')
        for role, key in online.items():
            require(root['roles'][role] == {'keyids': [key_id(key)], 'threshold': 1},
                    'Online key differs from the active public root role.')
        package = decode(read(args.package))
        build, asset = package['client_build'], package['assets']['Relay-Setup.exe']
        release = {'schema': 1, 'app_id': APP_ID, 'platform': 'windows-x64', 'channel': args.channel,
                   'version': build['version'], 'sequence': build['sequence'],
                   'installer_target': f"windows-x64/{args.channel}/Relay-{build['version']}-{build['sequence']}-Setup.exe",
                   'installer_sha256': asset['sha256'], 'installer_bytes': asset['bytes'],
                   'notes': read(args.notes, 4096).decode('utf-8'), 'handoff': args.handoff}
        previous = state['channels'].get(args.channel)
        validate_release(release, previous['release'] if previous else None)
        metadata_version = previous['metadata_version'] + 1 if previous else 1
        release_bytes, metadata = prepare_metadata(root, online, release, metadata_version, dt.datetime.now(dt.timezone.utc))
        make_directory(output)
        repository = make_directory(output / 'repository')
        metadata_dir = make_directory(repository / 'metadata')
        targets_dir = make_directory(repository / 'targets')
        make_directory(targets_dir / 'windows-x64')
        make_directory(targets_dir / 'windows-x64' / args.channel)
        copy_installer(args.installer, targets_dir / release['installer_target'], release)
        write_new(targets_dir / f'windows-x64/{args.channel}/release.json', release_bytes)
        for number, raw in roots.items():
            write_new(metadata_dir / f'{number}.root.json', raw)
        write_new(metadata_dir / 'root.json', roots[max(roots)])
        for name, raw in metadata.items():
            write_new(metadata_dir / name, raw)
        # Reserve the version before exposing a completion receipt. If interrupted,
        # retain the staged output and never re-use a metadata version on retry.
        state['channels'][args.channel] = {'metadata_version': metadata_version, 'release': release}
        save_online(store, state)
        policy = {'schema': 1, 'mode': 'signed', 'channel': args.channel,
                  'metadata': BASE + args.channel + '/metadata/',
                  'targets': BASE + args.channel + '/targets/', 'root_sha256': digest(roots[1])}
        write_new(output / 'policy.json', encoded(policy))
        write_new(output / 'root.json', roots[1])
        # Enumerate only our known files, without traversing a changed directory.
        public_files = {**{'metadata/' + name: raw for name, raw in metadata.items()},
                        **{f'metadata/{number}.root.json': raw for number, raw in roots.items()},
                        'metadata/root.json': roots[max(roots)],
                        f"targets/windows-x64/{args.channel}/release.json": release_bytes}
        files = {}
        for relative, expected in public_files.items():
            actual = read(repository / relative)
            require(actual == expected, 'Staged public metadata changed during preparation.')
            files[relative] = description(actual)
        files['targets/' + release['installer_target']] = {
            'length': release['installer_bytes'], 'hashes': {'sha256': release['installer_sha256']}}
        receipt = {'status': 'staged locally; not uploaded', 'channel': args.channel,
                   'metadata_version': metadata_version, 'release': release, 'files': files,
                   'timestamp_expires': decode(metadata['timestamp.json'])['signed']['expires'],
                   'root_sha256': digest(roots[1]), 'authenticode_verified': False}
        write_new(output / 'publication.json', encoded(receipt))
        return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest='command', required=True)
    initialize = commands.add_parser('init', help='Explicitly create new DPAPI-protected publisher keys')
    stage = commands.add_parser('publish', help='Sign and stage a release or refresh identical release metadata locally')
    for command in (initialize, stage):
        command.add_argument('--store', required=True)
        command.add_argument('--output', required=True)
    stage.add_argument('--channel', choices=('beta', 'stable'), required=True)
    stage.add_argument('--package', required=True, help='Reviewed package.json with client_build and installer asset digest')
    stage.add_argument('--installer', required=True)
    stage.add_argument('--notes', required=True, help='UTF-8 release notes, at most 4096 bytes; keep identical on refresh')
    stage.add_argument('--handoff', choices=('in-place-v2', 'wait-parent-v1'), default='in-place-v2')
    args = parser.parse_args()
    try:
        require(os.name == 'nt', 'Publisher storage requires Windows current-user DPAPI and direct local C:/D: paths.')
        result = init(args) if args.command == 'init' else publish(args)
        print(json.dumps({key: result[key] for key in ('status', 'root_sha256')}, indent=2))
    except PublisherError as error:
        parser.exit(1, 'Publisher stopped: ' + str(error) + '\n')
    except Exception as error:
        # Never print a key-containing input, decrypted JSON, traceback or locals.
        parser.exit(1, f'Publisher stopped ({type(error).__name__}). Existing state and partial output are retained.\n')


if __name__ == '__main__':
    main()
