"""Materialize hash-bound public release installers at the original update URLs.
Git blobs stay below 100 MiB; deployment artifacts permit larger native targets.
No credentials are used. Existing immutable targets are preserved.
"""
from pathlib import Path, PurePosixPath
import hashlib, json, re, urllib.request


def validate(row):
    assert set(row) == {'tag', 'path', 'sha256', 'bytes'}
    assert re.fullmatch(r'v[0-9.]+-preview\.[0-9.]+', row['tag'])
    assert re.fullmatch(r'updates/beta/targets/windows-x64/beta/Relay-[0-9.]+-[0-9]+-Setup\.exe', row['path'])
    assert re.fullmatch(r'[0-9a-f]{64}', row['sha256'])
    assert type(row['bytes']) is int and 0 < row['bytes'] <= 512 * 1024 * 1024
    assert not PurePosixPath(row['path']).is_absolute() and '..' not in PurePosixPath(row['path']).parts


def materialize(root, row, opener=urllib.request.urlopen):
    validate(row)
    destination = root / row['path']
    for part in [destination, *destination.parents]:
        assert not part.is_symlink(), 'Symlink target refused'
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        assert destination.stat().st_size == row['bytes']
        with destination.open('rb') as stream:
            assert hashlib.file_digest(stream, 'sha256').hexdigest() == row['sha256']
        return
    temporary = destination.with_suffix('.download')
    url = 'https://github.com/TheBengine/Relay/releases/download/' + row['tag'] + '/Relay-Setup.exe'
    request = urllib.request.Request(url, headers={'User-Agent': 'Relay-Pages-Publisher'})
    count = 0
    digest = hashlib.sha256()
    with opener(request, timeout=120) as response, temporary.open('xb') as output:
        assert response.geturl().startswith('https://'), 'HTTPS download required'
        while block := response.read(1024 * 1024):
            if count == 0:
                assert block[:2] == b'MZ'
            count += len(block)
            assert count <= row['bytes'], 'Release asset exceeds reviewed length'
            digest.update(block)
            output.write(block)
    assert count == row['bytes'] and digest.hexdigest() == row['sha256'], 'Release asset hash mismatch'
    assert not destination.exists(), 'Immutable target already exists'
    temporary.rename(destination)


def main():
    root = Path.cwd()
    records = json.loads((root / '.github/large-targets.json').read_text())
    assert records['schema'] == 1
    assert len({row['path'] for row in records['targets']}) == len(records['targets'])
    for row in records['targets']:
        materialize(root, row)
    release = json.loads((root / 'updates/beta/targets/windows-x64/beta/release.json').read_text())
    path = root / 'updates/beta/targets' / release['installer_target']
    assert path.is_file() and path.stat().st_size == release['installer_bytes']
    with path.open('rb') as stream:
        assert hashlib.file_digest(stream, 'sha256').hexdigest() == release['installer_sha256']
    print('All large targets and the active signed installer match reviewed hashes.')


if __name__ == '__main__':
    main()
