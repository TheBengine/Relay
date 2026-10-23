#!/usr/bin/env python3
"""Check actual Windows direct-host ACL protection and link rejection in work."""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import uuid

package = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--server", type=Path, default=package / "dist/relay-server.exe")
args = parser.parse_args()
if os.name != "nt":
    raise SystemExit("This acceptance check requires Windows ACLs")
workspace = package.parents[1] if package.parent.name == "outputs" else package
fixture = workspace / "work" / ("data-privacy-" + uuid.uuid4().hex)
fixture.mkdir(parents=True)
server = fixture / "relay-server.exe"
shutil.copyfile(args.server.resolve(), server)
shell = shutil.which("pwsh") or shutil.which("powershell")
quote = lambda value: "'" + str(value).replace("'", "''") + "'"


def powershell(code):
    bootstrap = ". " + quote(package / "scripts/shell-runtime.ps1") + "; "
    return subprocess.run([shell, "-NoProfile", "-Command", bootstrap + code], capture_output=True, text=True, timeout=20, check=True).stdout


def acl(path):
    return json.loads(powershell("$acl=Get-Acl -LiteralPath " + quote(path) + "; "
                                "$rules=@($acl.Access | ForEach-Object { @{sid=$_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value;type=$_.AccessControlType.ToString();inherited=$_.IsInherited} }); "
                                "@{protected=$acl.AreAccessRulesProtected;sddl=$acl.Sddl;rules=$rules} | ConvertTo-Json -Depth 4"))


def make_permissive(path):
    powershell("$acl=Get-Acl -LiteralPath " + quote(path) + "; $acl.SetAccessRuleProtection($true,$false); "
               "$inherit=if(Test-Path -LiteralPath " + quote(path) + " -PathType Container){'ContainerInherit,ObjectInherit'}else{'None'}; "
               "$rule=[Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new('S-1-1-0'),'FullControl',$inherit,'None','Allow'); "
               "$acl.SetAccessRule($rule); Set-Acl -LiteralPath " + quote(path) + " -AclObject $acl")


def migrate(path):
    return subprocess.run([str(server), "--data", str(path), "--migrate-only"], capture_output=True, text=True, timeout=20)


user_sid = powershell("[Security.Principal.WindowsIdentity]::GetCurrent().User.Value").strip()
data = fixture / "permissive data"
(data / "uploads").mkdir(parents=True)
existing = data / "uploads/keep.txt"
existing.write_bytes(b"preserved existing attachment")
make_permissive(data)
make_permissive(existing)
assert "S-1-1-0" in {rule["sid"] for rule in acl(existing)["rules"]}
result = migrate(data)
assert result.returncode == 0, result.stdout + result.stderr
assert existing.read_bytes() == b"preserved existing attachment"
checked = {}
for path in (data, data / "uploads", existing, data / "relay.sqlite3", data / "server.lock"):
    permissions = acl(path)
    assert {rule["sid"] for rule in permissions["rules"]} == {user_sid, "S-1-5-18"}, permissions
    assert all(rule["type"] == "Allow" for rule in permissions["rules"]), permissions
    if path in (data, existing):
        assert permissions["protected"], permissions
    checked[str(path.relative_to(fixture))] = True

outside = fixture / "outside data"
outside.mkdir()
sentinel = outside / "sentinel.txt"
sentinel.write_bytes(b"outside unchanged")
outside_acl, sentinel_acl = acl(outside), acl(sentinel)
links = []
try:
    root_alias = fixture / "root alias"
    powershell("New-Item -ItemType Junction -Path " + quote(root_alias) + " -Target " + quote(outside) + " | Out-Null")
    links.append(root_alias)
    for path in (root_alias, root_alias / "new child"):
        rejected = migrate(path)
        assert rejected.returncode != 0 and "junction or symbolic link" in rejected.stdout + rejected.stderr
    nested = fixture / "nested data"
    nested.mkdir()
    root_before = acl(nested)
    nested_link = nested / "uploads"
    powershell("New-Item -ItemType Junction -Path " + quote(nested_link) + " -Target " + quote(outside) + " | Out-Null")
    links.append(nested_link)
    rejected = migrate(nested)
    assert rejected.returncode != 0 and "junction or symbolic link" in rejected.stdout + rejected.stderr
    assert acl(nested) == root_before
    hard = fixture / "hard-linked data"
    hard.mkdir()
    os.link(sentinel, hard / "shared.txt")
    hard_before = acl(hard)
    rejected = migrate(hard)
    assert rejected.returncode != 0 and "hard-linked" in rejected.stdout + rejected.stderr
    assert acl(hard) == hard_before
    assert acl(outside) == outside_acl and acl(sentinel) == sentinel_acl
    assert sentinel.read_bytes() == b"outside unchanged"
    assert not (outside / "relay.sqlite3").exists()
finally:
    for link in reversed(links):
        link.rmdir() # Remove only our junctions, never recursively their targets.

evidence = {"scope": "Synthetic Windows direct-host data only, inside work; no other-user login or administrator isolation claim",
            "server_sha256": hashlib.sha256(server.read_bytes()).hexdigest(), "permissive_acl_replaced": checked,
            "root_and_ancestor_junction_rejected": True, "nested_junction_rejected_before_acl_mutation": True,
            "hard_link_rejected_before_acl_mutation": True, "outside_bytes_and_acl_preserved": True}
(fixture / "result.json").write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
print("PASS: actual direct server protects permissive existing/new files; links are rejected and outside bytes/ACLs preserved. Evidence:", fixture / "result.json")
