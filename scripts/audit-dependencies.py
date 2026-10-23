#!/usr/bin/env python3
"""Query OSV for the exact locked Windows normal/build Rust dependency graph.

This sends public crate names/versions only. Native WebRTC and the bundled SFU
need separate upstream security review; a clean Rust result does not cover them.
"""
from __future__ import annotations
import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import subprocess
import urllib.request


def request_json(path, payload=None):
    raw = json.dumps(payload).encode() if payload is not None else None
    request = urllib.request.Request("https://api.osv.dev/v1/" + path, data=raw,
                                    headers={"Content-Type": "application/json", "User-Agent": "Relay-release-audit"})
    with urllib.request.urlopen(request, timeout=45) as response:
        content = response.read(16 * 1024 * 1024 + 1)
    if len(content) > 16 * 1024 * 1024:
        raise ValueError("OSV response exceeded the bounded audit size")
    return json.loads(content)


def scan(packages, ecosystem="crates.io"):
    queries = [{"package": {"ecosystem": ecosystem, "name": name}, "version": version}
               for name, version in packages]
    matches = [set() for _ in queries]
    pending = list(enumerate(queries))
    while pending:
        batch = pending[:100]
        pending = pending[100:]
        results = request_json("querybatch", {"queries": [query for _, query in batch]})["results"]
        if len(results) != len(batch):
            raise ValueError("OSV returned an incomplete package batch")
        for (index, query), result in zip(batch, results):
            matches[index].update(item["id"] for item in result.get("vulns", []))
            if result.get("next_page_token"):
                pending.append((index, {**query, "page_token": result["next_page_token"]}))
    ids = sorted(set().union(*matches))
    with ThreadPoolExecutor(max_workers=4) as pool:
        records = dict(zip(ids, pool.map(lambda identifier: request_json("vulns/" + identifier), ids)))
    return [{"name": name, "version": version,
             "advisories": [records[identifier] for identifier in sorted(found)]}
            for (name, version), found in zip(packages, matches) if found]


def main():
    package = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    command = ["cargo", "tree", "--workspace", "--locked", "--prefix", "none", "--edges", "normal,build",
               "--target", "x86_64-pc-windows-msvc", "--features", "relay-client/native-rtc"]
    tree = subprocess.run(command, cwd=package, capture_output=True, text=True, check=True).stdout
    packages = sorted(set(re.findall(r"^([\w-]+) v([\w.+-]+)", tree, re.MULTILINE)))
    metadata = json.loads(subprocess.run(["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"],
                          cwd=package, capture_output=True, text=True, check=True).stdout)
    workspace = set(metadata["workspace_members"])
    own_names = {item["name"] for item in metadata["packages"] if item["id"] in workspace}
    packages = [(name, version) for name, version in packages if name not in own_names]
    findings = scan(packages)
    active = sum(not record.get("withdrawn") for item in findings for record in item["advisories"])
    result = {"checked_at_utc": datetime.now(timezone.utc).isoformat(), "source": "https://api.osv.dev",
              "scope": "Locked Windows x64 normal/build Rust graph with native-rtc; upstream path-patch version included",
              "excludes": ["Native libwebrtc binary", "Bundled LiveKit Go server", "Dynamic Windows/driver libraries", "Other-platform dependencies"],
              "lock_sha256": hashlib.sha256((package / "Cargo.lock").read_bytes()).hexdigest(),
              "queried_count": len(packages), "packages": [{"name": name, "version": version} for name, version in packages],
              "active_advisory_records": active, "findings": findings}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(f"Queried {len(packages)} locked Windows Rust packages; {active} active advisory records. Details: {args.output}")
    raise SystemExit(1 if active else 0)


if __name__ == "__main__":
    main()
