#!/usr/bin/env python3
"""Read the bundled Go SFU's embedded modules and query public OSV versions.

No SFU process is started. This is a module-version scan, not govulncheck's
package/function reachability analysis or a native libwebrtc security audit.
"""
from __future__ import annotations
import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
from pathlib import Path
import re


def build_info(binary):
    if len(binary) > 150 * 1024 * 1024 or not binary.startswith(b"MZ"):
        raise ValueError("Expected the bounded Windows SFU executable")
    marker = b"\xff Go buildinf:"
    offset = binary.find(marker)
    if offset < 0 or offset % 16 or offset + 32 >= len(binary) or not binary[offset + 15] & 2:
        raise ValueError("Expected inline Go1.18+ build information")
    cursor = offset + 32
    def string():
        nonlocal cursor
        size = 0
        for shift in range(0, 70, 7):
            byte = binary[cursor]
            cursor += 1
            size |= (byte & 127) << shift
            if byte < 128:
                break
        else:
            raise ValueError("Invalid Go build-info string length")
        if size > 1024 * 1024 or cursor + size > len(binary):
            raise ValueError("Go build information exceeded bounds")
        text = binary[cursor:cursor + size]
        cursor += size
        return text
    version, framed = string().decode("ascii"), string()
    if not re.fullmatch(r"go\d+\.\d+\.\d+", version) or len(framed) < 33 or framed[-17] != 10:
        raise ValueError("Invalid Go build-info framing/version")
    return version, framed[16:-16].decode("utf-8")


def main():
    package = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sfu", type=Path, default=package / "dist/livekit-server.exe")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.sfu.read_bytes()
    go_version, text = build_info(binary)
    modules, unversioned = [], []
    for line in text.splitlines():
        fields = line.split("\t")
        if fields[0] in ("dep", "mod") and len(fields) >= 3:
            item = (fields[1], fields[2])
            if fields[2].startswith("v"):
                modules.append(item)
            else:
                unversioned.append({"name": fields[1], "version": fields[2]})
        elif fields[0] == "=>" and len(fields) >= 3:
            if not modules:
                raise ValueError("Unresolved Go module replacement")
            modules.pop()
            if fields[2].startswith("v"):
                modules.append((fields[1], fields[2]))
            else:
                unversioned.append({"name": fields[1], "version": fields[2]})
    if not modules:
        raise ValueError("SFU contains no versioned module information")
    packages = sorted(set(modules + [("stdlib", go_version.removeprefix("go"))]))
    spec = importlib.util.spec_from_file_location("relay_osv", package / "scripts/audit-dependencies.py")
    audit = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(audit)
    findings = audit.scan(packages, ecosystem="Go")
    active = sum(not record.get("withdrawn") for item in findings for record in item["advisories"])
    result = {"checked_at_utc": datetime.now(timezone.utc).isoformat(), "source": "https://api.osv.dev",
              "build_info_format_reference": "https://go.dev/src/debug/buildinfo/buildinfo.go",
              "scope": "Embedded Go module/toolchain version matches; not package/function reachability or native libwebrtc",
              "binary_sha256": hashlib.sha256(binary).hexdigest(), "go_toolchain": go_version,
              "queried_count": len(packages), "packages": [{"name": name, "version": version} for name, version in packages],
              "unversioned_modules": unversioned, "active_advisory_records": active, "findings": findings}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(f"Queried {len(packages)} embedded Go module/toolchain versions; {active} active advisory records. Details: {args.output}")
    raise SystemExit(1 if active else 0)


if __name__ == "__main__":
    main()
