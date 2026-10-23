#!/usr/bin/env python3
"""Collect dependency license/notice files from the fetched Cargo sources."""
from __future__ import annotations
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib
import urllib.error
import urllib.request


def upstream_notices(source, metadata, cache, memo, offline=False):
    """Some workspace crates omit root license files from published packages."""
    info_path = source / '.cargo_vcs_info.json'
    if not info_path.is_file():
        return []
    commit = json.loads(info_path.read_text(encoding='utf-8')).get('git', {}).get('sha1')
    repository = metadata.get('repository') or metadata.get('homepage', '')
    match = re.match(r'https://github\.com/([^/]+/[^/]+)', repository)
    if not match or not commit:
        return []
    repo = match.group(1).removesuffix('.git')
    key = repo, commit
    if key in memo:
        return memo[key]
    destination = cache / repo.replace('/', '_') / commit
    destination.mkdir(parents=True, exist_ok=True)
    names = ['LICENSE-MIT', 'LICENSE-APACHE', 'LICENSE', 'LICENSE.txt', 'LICENSE.md', 'COPYING', 'NOTICE']

    def fetch(name):
        url = f'https://raw.githubusercontent.com/{repo}/{commit}/{name}'
        cached = destination / name
        try:
            if cached.is_file():
                content = cached.read_text(encoding='utf-8')
            else:
                if offline:
                    return None
                with urllib.request.urlopen(url, timeout=8) as response:
                    content = response.read().decode('utf-8')
                cached.write_text(content, encoding='utf-8')
            return url, content
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError):
            return None

    with ThreadPoolExecutor(max_workers=7) as executor:
        memo[key] = [result for result in executor.map(fetch, names) if result]
    return memo[key]


def main():
    package = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default="x86_64-pc-windows-msvc")
    parser.add_argument("--output", type=Path, default=package / "THIRD_PARTY_NOTICES.txt")
    parser.add_argument("--without-native-rtc", action="store_true", help="Use the minimal development feature graph")
    parser.add_argument("--offline", action="store_true", help="Do not retrieve missing root licenses from upstream")
    args = parser.parse_args()
    command = ["cargo", "tree", "--workspace", "--locked", "--prefix", "none", "--edges", "normal,build", "--target", args.target]
    if not args.without_native_rtc:
        command += ['--features','relay-client/native-rtc']
    if args.offline:
        command.append('--offline')
    tree = subprocess.run(command, cwd=package, capture_output=True, text=True, check=True).stdout
    dependencies = sorted(set(re.findall(r"^([\w-]+) v([\w.+-]+)", tree, re.MULTILINE)))
    cargo_root = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    registries = list((cargo_root / "registry" / "src").glob("*"))
    # Path patches are part of the shipped source, so preserve their exact local
    # license/provenance rather than accidentally collecting an unpatched registry copy.
    local_sources = {}
    manifest = tomllib.loads((package/'Cargo.toml').read_text(encoding='utf-8'))
    workspace_names = set()
    for member in manifest.get('workspace', {}).get('members', []):
        source = (package/member).resolve()
        if not source.is_relative_to(package.resolve()):
            raise ValueError('Workspace source must remain inside the packaged source')
        workspace_names.add(tomllib.loads((source/'Cargo.toml').read_text(encoding='utf-8'))['package']['name'])
    for patch in manifest.get('patch', {}).get('crates-io', {}).values():
        if isinstance(patch, dict) and patch.get('path'):
            source = (package/patch['path']).resolve()
            if not source.is_relative_to(package.resolve()):
                raise ValueError('Dependency path patch must remain inside the packaged source')
            info = tomllib.loads((source/'Cargo.toml').read_text(encoding='utf-8'))['package']
            local_sources[info['name'], info['version']] = source
    blocks = ["Relay third-party notices\n\n"
              "Collected from source packages in the Windows dependency graph.\n"
              "Libraries retain the licenses and copyright notices below.\n"
              "Relay-authored code is source available under PolyForm Perimeter 1.0.0; see LICENSE and LICENSING.md.\n"
              "Native WebRTC binary licenses: docs/LICENSE-libwebrtc.md.\n"
              "Bundled LiveKit server license: docs/LICENSE-livekit.txt.\n"]
    missing = []
    work = package.parents[1] / 'work' if package.parent.name == 'outputs' else package / 'work'
    cache, memo = work / 'relay-license-cache', {}
    for name, version in dependencies:
        if name in workspace_names:
            continue
        source = local_sources.get((name, version)) or next((registry / f"{name}-{version}" for registry in registries
                       if (registry / f"{name}-{version}" / "Cargo.toml").is_file()), None)
        if not source:
            missing.append(f"{name} {version}: source unavailable")
            continue
        metadata = tomllib.loads((source / "Cargo.toml").read_text(encoding="utf-8"))["package"]
        blocks.append(f"\n{'=' * 72}\n{name} {version}\nLicense: {metadata.get('license', 'See license file')}\n"
                      f"Repository: {metadata.get('repository', '')}\n")
        if name in {'hpke-rs', 'hpke-rs-crypto', 'hpke-rs-rust-crypto'} and version == '0.7.0' and metadata.get('license') == 'MPL-2.0':
            # The published crates declare MPL, while this exact VCS workspace
            # contains Apache/MIT root texts. Preserve the package declaration;
            # unrelated workspace licenses must not silently change its terms.
            blocks.append('Published Cargo.toml declares MPL-2.0. The workspace root texts below do not replace that declaration.\n'
                          f"Published package author: {', '.join(metadata.get('authors', []))}\n"
                          f'Unmodified covered source: https://crates.io/api/v1/crates/{name}/{version}/download\n'
                          'The covered source is distributed under MPL-2.0; Relay applies no modifications to these crates.\n'
                          'Standard license from https://www.mozilla.org/media/MPL/2.0/index.txt:\n'
                          + (package/'docs/LICENSE-MPL-2.0.txt').read_text(encoding='utf-8'))
        if (name, version) in local_sources:
            blocks.append(f"Packaged source: {source.relative_to(package)}\n")
            provenance = source/'RELAY-PATCH.md'
            if provenance.is_file():
                blocks.append(provenance.read_text(encoding='utf-8'))
        license_files = [item for item in source.rglob("*") if item.is_file() and
                         (re.match(r"^(license|licence|copying|notice)([._-].*)?$", item.name, re.IGNORECASE)
                          or (item.parent.name == 'fonts' and item.suffix.lower() == '.txt'))]
        declared = metadata.get("license-file")
        if declared and (source / declared).is_file():
            license_files.append(source / declared)
        license_files = sorted(set(license_files))
        if not license_files:
            # This old crate predates Cargo VCS metadata and declares its Rust-derived
            # source's copyright and dual MIT/Apache terms directly in lib.rs.
            fx_source = (source / 'lib.rs').read_text(encoding='utf-8') if name == 'fxhash' else ''
            if name == 'fxhash' and version == '0.2.1' and fx_source.startswith('// Copyright 2015 The Rust Project Developers.'):
                copyright_header = '\n'.join(fx_source.splitlines()[:9])
                mit_permission = (package / 'LICENSES/MIT.txt').read_text(encoding='utf-8').split('Permission is hereby granted', 1)[1]
                upstream = [('Published lib.rs header, MIT option', f'{copyright_header}\n\nMIT License\n\nCopyright 2015 The Rust Project Developers.\n\nPermission is hereby granted{mit_permission}')]
            elif name in {'from_variants','from_variants_impl'} and version=='1.0.2' and metadata.get('license')=='MIT/Apache-2.0':
                authors=', '.join(metadata.get('authors',[]))
                terms=(package/'LICENSES/MIT.txt').read_text(encoding='utf-8').split('Permission is hereby granted',1)[1]
                upstream=[('Published Cargo.toml dual-license declaration, MIT option',
                    f'MIT License\n\nPublished package author: {authors}\n'
                    'Upstream declares MIT/Apache-2.0 in Cargo.toml; no standalone copyright notice is supplied.\n\n'
                    f'Permission is hereby granted{terms}')]
            elif name == 'qrcodegen' and version == '1.8.0' and metadata.get('license') == 'MIT':
                # Nayuki publishes the complete notice in the Rust source header.
                header = (source/'src/lib.rs').read_text(encoding='utf-8').split('*/', 1)[0]
                if 'Copyright (c) Project Nayuki.' not in header or 'Permission is hereby granted' not in header:
                    raise ValueError('The pinned qrcodegen source license header changed')
                upstream = [('Published src/lib.rs license header', header + '*/')]
            elif ((name == 'realfft' and version == '3.5.0' and metadata.get('license') == 'MIT')
                  or (name == 'typed-path' and version == '0.9.3'
                      and metadata.get('license') == 'MIT OR Apache-2.0')):
                # This package and its exact upstream commit omit a standalone license
                # file. Preserve its published MIT declaration/author credit and the
                # standard terms, without inventing a copyright notice or year.
                authors = ', '.join(metadata.get('authors', []))
                terms = (package / 'LICENSES/MIT.txt').read_text(encoding='utf-8').split('Permission is hereby granted', 1)[1]
                upstream = [('Published Cargo.toml / README MIT declaration',
                    f'MIT License\n\nPublished package author: {authors}\n'
                    'Upstream offers MIT in Cargo.toml and README.md; no separate copyright notice is supplied.\n\n'
                    f'Permission is hereby granted{terms}')]
            else:
                upstream = upstream_notices(source, metadata, cache, memo, args.offline)
            if upstream:
                for url, content in upstream:
                    blocks.append(f"\n--- Upstream workspace license: {url} ---\n{content}\n")
            else:
                missing.append(f"{name} {version}: no license text in source package or exact upstream commit")
        for license_file in license_files:
            try:
                content = license_file.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                content = license_file.read_text(encoding="utf-8", errors="replace")
            blocks.append(f"\n--- {license_file.relative_to(source)} ---\n{content}\n")
    # Offline common-password data is not a Cargo dependency, but its MIT
    # attribution must be present in every binary distribution too.
    blocks.append("\n" + "=" * 72 + "\nSecLists common-password corpus\nLicense: MIT\n"
                  "Source: https://github.com/danielmiessler/SecLists/tree/913b327317496d062bcc7cace524aaad8a693be2\n"
                  "Used only for offline new-password checks; see assets/security/common-passwords.provenance.json.\n\n"
                  + (package / 'assets/security/LICENSE-SecLists.txt').read_text(encoding='utf-8'))
    # Stable CRLF output matches the existing Windows notice artifact.
    content = '\n'.join(line.rstrip() for line in '\n'.join(blocks).splitlines()).rstrip() + '\n'
    args.output.write_bytes(content.replace('\n', '\r\n').encode('utf-8'))
    print(f"Collected license notices for {sum(name not in workspace_names for name, _ in dependencies)} dependency packages: {args.output}")
    if missing:
        print("License text requiring review:")
        for item in missing:
            print(f"  {item}")
    return 1 if missing else 0


if __name__ == "__main__":
    raise SystemExit(main())
