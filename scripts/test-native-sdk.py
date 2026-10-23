#!/usr/bin/env python3
"""Reject tampered SDK caches and reparse destinations without modifying the real SDK."""
from __future__ import annotations
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import uuid

package = Path(__file__).resolve().parents[1]
workspace = package.parents[1] if package.parent.name == "outputs" else package
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--cache", type=Path, default=workspace / "work/relay-webrtc-89d790b")
args = parser.parse_args()
cache = args.cache.resolve()
archive = cache / "webrtc-win-x64-release.zip"
expected = "5c2349c960bae4f06f71c102f58552d0d09c912a142811acfcadfb7c883cbf58"
assert hashlib.sha256(archive.read_bytes()).hexdigest() == expected, "Run fetch-webrtc.ps1 first"
fixture = workspace / "work" / ("sdk-integrity-" + uuid.uuid4().hex)
fixture.mkdir(parents=True)
shell = shutil.which("pwsh") or shutil.which("powershell")
quote = lambda value: "'" + str(value).replace("'", "''") + "'"


def run(*arguments):
    return subprocess.run([shell, "-NoProfile", "-File", str(package / "scripts/fetch-webrtc.ps1"), *map(str, arguments)], capture_output=True, text=True, timeout=90)


bad_cache = fixture / "tampered archive"
bad_cache.mkdir()
(bad_cache / archive.name).write_bytes(b"tampered cached archive")
result = run("-CacheDirectory", bad_cache)
assert result.returncode != 0 and "archive checksum mismatch" in result.stdout + result.stderr
bad_tree = fixture / "tampered tree"
bad_tree.mkdir()
license_bytes = bytearray((cache / "verified/win-x64-release/LICENSE.md").read_bytes())
license_bytes[0] ^= 1
(bad_tree / "LICENSE.md").write_bytes(license_bytes)
result = run("-CacheDirectory", cache, "-VerifyDirectory", bad_tree)
assert result.returncode != 0 and "cache integrity mismatch: LICENSE.md" in result.stdout + result.stderr
outside = fixture / "outside"
outside.mkdir()
sentinel = outside / "keep.txt"
sentinel.write_bytes(b"outside preserved")
links = []
try:
    for alias, arguments in [(fixture / "cache alias", ("-CacheDirectory",)),
                             (fixture / "tree alias", ("-CacheDirectory", cache, "-VerifyDirectory"))]:
        subprocess.run([shell, "-NoProfile", "-Command", "New-Item -ItemType Junction -Path " + quote(alias) + " -Target " + quote(outside) + " | Out-Null"], check=True, capture_output=True)
        links.append(alias)
        result = run(*arguments, alias)
        assert result.returncode != 0 and "junction or symbolic link" in result.stdout + result.stderr
        assert sentinel.read_bytes() == b"outside preserved"
        assert list(outside.iterdir()) == [sentinel]
finally:
    for link in reversed(links):
        link.rmdir()
# Fail the real builder before Cargo in an isolated fake source package and
# check restoration in the same shell process, including a pre-existing SDK.
fake_package = fixture / "builder/outputs/relay"
(fake_package / "scripts").mkdir(parents=True)
for name in ("build.ps1", "fetch-webrtc.ps1", "shell-runtime.ps1"):
    shutil.copyfile(package / "scripts" / name, fake_package / "scripts" / name)
fake_cache = fixture / "builder/work/relay-webrtc-89d790b"
fake_cache.mkdir(parents=True)
(fake_cache / archive.name).write_bytes(b"fail before any cargo command")
restoration = subprocess.run([shell, "-NoProfile", "-Command",
    "$env:LK_CUSTOM_WEBRTC='existing_sdk_marker';$env:CARGO_TARGET_DIR='existing_target_marker';$env:RUSTFLAGS='existing_flags_marker';$env:CMAKE_POLICY_VERSION_MINIMUM='existing_cmake_marker';$originalPath=$env:PATH;"
    "try { & " + quote(fake_package / "scripts/build.ps1") + "; throw 'Expected build failure' } catch { if($_.ToString() -notmatch 'archive checksum mismatch'){throw} };"
    "if($env:LK_CUSTOM_WEBRTC -ne 'existing_sdk_marker' -or $env:CARGO_TARGET_DIR -ne 'existing_target_marker' -or $env:RUSTFLAGS -ne 'existing_flags_marker' -or $env:CMAKE_POLICY_VERSION_MINIMUM -ne 'existing_cmake_marker' -or $env:PATH -ne $originalPath){throw 'Builder leaked environment changes'}; 'Environment restored'"],
    capture_output=True, text=True, timeout=40)
assert restoration.returncode == 0 and "Environment restored" in restoration.stdout, restoration.stdout + restoration.stderr
result = {"scope": "Synthetic SDK cache files/junctions in work only; the real verified SDK is read only",
          "official_archive_sha256": expected, "tampered_archive_rejected": True,
          "same_length_tampered_file_rejected": True, "cache_and_tree_junctions_rejected": True,
          "outside_files_preserved": True, "builder_environment_restored_after_integrity_failure": True}
(fixture / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
print("PASS: tampered ZIP/file and SDK reparse destinations rejected; outside sentinel preserved. Evidence:", fixture / "result.json")
