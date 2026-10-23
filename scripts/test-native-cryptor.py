"""Prove reviewed native frame cryptor behavior with owned synthetic frames only."""
from pathlib import Path
import argparse
import hashlib
import json
import os
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--sdk', type=Path, required=True)
parser.add_argument('--base-sdk', type=Path, required=True)
parser.add_argument('--work', type=Path, required=True, help='A new, isolated proof directory')
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
sdk, base, work = args.sdk.resolve(), args.base_sdk.resolve(), args.work.resolve()
if os.name != 'nt' or work == root or root in work.parents:
    raise SystemExit('Use Windows and a proof directory outside the package sources.')
work.mkdir(parents=True, exist_ok=False)
probe = root/'vendor/relay-frame-crypto/tests'
source = root/'vendor/relay-frame-crypto/frame_crypto_transformer.cc'
manifest = json.loads((sdk/'relay-frame-crypto.json').read_text(encoding='utf-8-sig'))


def sha(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


assert manifest['patch'] == 'relay-frame-iv-v1'
assert manifest['patch_sha256'] == sha(source)
assert manifest['library_sha256'] == sha(sdk/'lib/webrtc.lib')
vswhere = Path(os.environ['ProgramFiles(x86)'])/'Microsoft Visual Studio/Installer/vswhere.exe'
installation = subprocess.check_output([str(vswhere), '-latest', '-products', '*', '-requires',
                                       'Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
                                       '-property', 'installationPath'], text=True).strip()
version = (Path(installation)/'VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt').read_text().strip()
librarian = Path(installation)/'VC/Tools/MSVC'/version/'bin/HostX64/x64/lib.exe'
without = work/'webrtc-without-crypto.lib'
member = 'obj/api/crypto/frame_crypto_transformer/frame_crypto_transformer.obj'
with (work/'remove-original.log').open('w') as log:
    subprocess.run([str(librarian), '/nologo', '/remove:'+member, '/out:'+str(without),
                    str(base/'lib/webrtc.lib')], stdout=log, stderr=subprocess.STDOUT, check=True)
text = source.read_text()
faults = {
    'rng': ('if (RAND_bytes(iv.data(), iv.size()) != 1)', 'if (0 != 1)'),
    'quota': ('(uint64_t{1} << 32)', 'uint64_t{2}'),
}
checks = []
for name in ['reviewed-library', 'rng', 'quota']:
    env = dict(os.environ, LK_CUSTOM_WEBRTC=str(sdk), RUSTFLAGS='-Ctarget-feature=+crt-static',
               CARGO_TARGET_DIR=str(work/('target-'+name)))
    for variable in ['RELAY_TEST_CRYPTO_SOURCE', 'RELAY_TEST_CRYPTO_LIBRARY', 'RELAY_PROBE_VARIANT']:
        env.pop(variable, None)
    if name in faults:
        before, after = faults[name]
        assert text.count(before) == 1
        fault = work/(name+'.cc')
        fault.write_text(text.replace(before, after))
        env.update(RELAY_TEST_CRYPTO_SOURCE=str(fault), RELAY_TEST_CRYPTO_LIBRARY=str(without),
                   RELAY_PROBE_VARIANT=name)
    log_path = work/(name+'.log')
    command = ['cargo', 'run', '--locked', '--offline', '--release']
    with log_path.open('w') as log:
        result = subprocess.run(command, cwd=probe, env=env, stdout=log, stderr=subprocess.STDOUT)
    if result.returncode:
        raise SystemExit('Native proof failed; inspect '+str(log_path))
    lines = log_path.read_text().splitlines()
    outcome = json.loads(next(line for line in reversed(lines) if line.startswith('{')))
    if name == 'reviewed-library':
        assert outcome['synthetic_video_frames'] == outcome['native_decrypted'] == 256
        assert outcome['sample_iv_collisions'] == 0 and outcome['tampered_tags_rejected'] == 32
    else:
        assert outcome['no_output_on_failure'] and outcome['new_cryptor_still_refused']
        assert outcome['accepted_before_limit'] == (2 if name == 'quota' else 0)
    binary = work/('target-'+name)/'release/relay-owned-native-iv-probe.exe'
    checks.append({'name': name, 'command': command, 'outcome': outcome,
                   'log': str(log_path), 'log_sha256': sha(log_path),
                   'binary': str(binary), 'binary_sha256': sha(binary)})
    print('PASS: '+name, flush=True)
record = {'scope': 'Actual reviewed native SDK archive; generated VP8 transform payloads only. '
          'Separate owned fault translation units prove refusal. No physical media, personal keys or network.',
          'library_sha256': sha(sdk/'lib/webrtc.lib'), 'marker_sha256': sha(sdk/'relay-frame-crypto.json'),
          'source_sha256': sha(source), 'probe_inputs': {str(p.relative_to(root)): sha(p)
          for p in sorted(probe.rglob('*')) if p.is_file()}, 'checks': checks, 'installation': False}
(work/'verification.json').write_text(json.dumps(record, indent=2)+'\n')
