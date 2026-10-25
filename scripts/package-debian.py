"""Create the Debian home-server archives from the current Relay source tree."""
from pathlib import Path
import argparse
import hashlib
import json
import os
import shutil
import tarfile
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
# A separate folder per release keeps earlier packages and their checksums intact.
parser.add_argument('--output', type=Path, default=ROOT.parent, help='existing folder for the archives')
OUTPUT = parser.parse_args().output.resolve()
if not OUTPUT.is_dir():
    raise SystemExit(f'Output folder does not exist: {OUTPUT}')
WORK = ROOT.parents[1] / 'work'
WORK.mkdir(exist_ok=True)
stage = WORK / ('debian-package-' + uuid.uuid4().hex) / 'relay-debian-home'
stage.mkdir(parents=True)
assert stage.resolve().is_relative_to(WORK.resolve())

templates = ROOT / 'deploy' / 'debian'
for file in templates.rglob('*'):
    if not file.is_file() or '__pycache__' in file.parts:
        continue
    if file.is_symlink():
        raise ValueError(f'Unexpected source link: {file}')
    target = stage / file.relative_to(templates)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(file.read_bytes().replace(b'\r\n', b'\n'))

source = stage / 'source'
source.mkdir()
for name in ['Cargo.toml', 'Cargo.lock', 'LICENSE', 'THIRD_PARTY_NOTICES.txt']:
    shutil.copy2(ROOT / name, source / name)
for name in ['crates', 'vendor', '.cargo']:
    for file in (ROOT / name).rglob('*'):
        if not file.is_file():
            continue
        if file.is_symlink() or '__pycache__' in file.parts or file.suffix.lower() in {'.db', '.sqlite3', '.device', '.exe', '.dll', '.log', '.pyc'}:
            raise ValueError(f'Unexpected source file: {file}')
        target = source / file.relative_to(ROOT)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(file, target)

hashes = {file.relative_to(source).as_posix(): hashlib.sha256(file.read_bytes()).hexdigest()
          for file in sorted(source.rglob('*')) if file.is_file()}
(stage / 'SOURCE-SHA256.json').write_text(json.dumps(hashes, indent=2) + '\n', encoding='utf-8')
shutil.copy2(ROOT / 'LICENSE', stage / 'LICENSE')
shutil.copy2(ROOT / 'THIRD_PARTY_NOTICES.txt', stage / 'THIRD_PARTY_NOTICES.txt')

def public_member(info):
    info.uid = info.gid = 0
    info.uname = info.gname = 'root'
    info.mode = 0o755 if info.isdir() or info.name.endswith('.sh') or info.name.endswith('/relay-home') else 0o644
    return info

for suffix in ['.tar.gz', '.zip']:
    final = OUTPUT / ('relay-debian-home' + suffix)
    pending = Path(str(final) + '.pending')
    if suffix == '.tar.gz':
        with tarfile.open(pending, 'w:gz') as archive:
            archive.add(stage, arcname='relay-debian-home', filter=public_member)
    else:
        with zipfile.ZipFile(pending, 'w', zipfile.ZIP_DEFLATED) as archive:
            for file in sorted(stage.rglob('*')):
                if file.is_file():
                    archive.write(file, 'relay-debian-home/' + file.relative_to(stage).as_posix())
    os.replace(pending, final)
    digest = hashlib.sha256(final.read_bytes()).hexdigest()
    Path(str(final) + '.sha256').write_text(digest + '\n', encoding='ascii')
    print(f'{final}: {final.stat().st_size:,} bytes, SHA256 {digest}')
print(f'Staged source: {stage} ({len(hashes)} source files)')
