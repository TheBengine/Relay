"""Regress Windows PowerShell hosting with a Rust-canonicalized local path."""
from __future__ import annotations

import json
from pathlib import Path
import subprocess


def main():
    project = Path(__file__).resolve().parents[1]
    shell = Path(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
    helper = str(project / "scripts" / "hosting-paths.ps1").replace("'", "''")
    code = r"""
. '__HELPER__'
$normal='C:\Relay test\profile\server-data'
$verbatim='\\?\'+$normal
foreach($inputPath in @($normal,$verbatim)){
    $resolved=Resolve-RelayDataDirectory -DataDirectory $inputPath
    if($resolved -cne $normal){throw 'Explicit data folder changed'}
    if((Join-Path $resolved 'managed-instance.json') -cne ($normal+'\managed-instance.json')){
        throw 'Windows PowerShell cannot combine the resolved path'
    }
}
$env:RELAY_SERVER_DATA_DIR=$verbatim
if((Resolve-RelayDataDirectory) -cne $normal){throw 'Environment data folder changed'}
if((Resolve-RelayDataDirectory -DataDirectory 'C:\Explicit choice') -cne 'C:\Explicit choice'){
    throw 'Explicit folder must take precedence'
}
'Local canonical paths and precedence passed'
""".replace("__HELPER__", helper)
    result = subprocess.run(
        [str(shell), "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", code],
        capture_output=True, text=True, timeout=20,
    )
    if result.returncode:
        raise AssertionError(result.stdout + result.stderr)
    print(json.dumps({"windows_powershell_canonical_path": True, "argument_precedence": True}))


if __name__ == "__main__":
    main()
