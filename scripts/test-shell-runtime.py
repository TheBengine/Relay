"""Verify child-shell built-ins despite inherited/shadowing module paths."""
from __future__ import annotations
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

package = Path(__file__).resolve().parents[1]
workspace = package.parents[1] if package.parent.name == 'outputs' else package
work = workspace/'work'
work.mkdir(parents=True, exist_ok=True)
shell = shutil.which('powershell.exe')
assert os.name == 'nt' and shell, 'Windows PowerShell is required'


def quote(value):
    return "'"+str(value).replace("'", "''")+"'"


with tempfile.TemporaryDirectory(prefix='relay-shell-test-', dir=work) as temporary:
    fixture = Path(temporary).resolve()
    assert fixture.parent == work.resolve()
    file = fixture/'hash source.txt'
    file.write_bytes(b'known release hash fixture')
    expected = hashlib.sha256(file.read_bytes()).hexdigest()
    original = os.environ.copy()
    helper = quote(package/'scripts/shell-runtime.ps1')
    check = ('. '+helper+'; $h=(Get-FileHash -LiteralPath '+quote(file)+
             ' -Algorithm SHA256).Hash; if($h -ne '+quote(expected)+
             "){throw 'Wrong shell hash implementation'}")

    def run(code, env):
        return subprocess.run([shell, '-NoProfile', '-Command', code], env=env,
                              capture_output=True, text=True, timeout=20)

    # Keep the parent's real module paths: reproduces Core->Windows shell launches.
    inherited = run(check, original)
    assert inherited.returncode == 0, inherited.stdout+inherited.stderr
    shadow = fixture/'modules/Microsoft.PowerShell.Utility'
    shadow.mkdir(parents=True)
    (shadow/'Microsoft.PowerShell.Utility.psm1').write_text("Export-ModuleMember -Function @()\n", encoding='utf-8')
    (shadow/'Microsoft.PowerShell.Utility.psd1').write_text(
        "@{RootModule='Microsoft.PowerShell.Utility.psm1'; ModuleVersion='99.0.0'; FunctionsToExport=@(); CmdletsToExport=@()}\n",
        encoding='utf-8')
    polluted = original.copy()
    inherited_paths = next((value for key,value in original.items() if key.lower()=='psmodulepath'), '')
    for key in list(polluted):
        if key.lower()=='psmodulepath':polluted.pop(key)
    polluted['PSMODULEPATH'] = str(fixture/'modules')+';'+inherited_paths
    unavailable = run('Get-Command Get-FileHash -ErrorAction Stop', polluted)
    assert unavailable.returncode != 0, 'fixture must reproduce unavailable built-in hashing'
    repaired = run(check, polluted)
    assert repaired.returncode == 0, repaired.stdout+repaired.stderr
    assert os.environ.copy() == original, 'child initialization must not mutate parent environment'
print('PASS: inherited and deliberately shadowing module paths use matching shell built-ins; parent environment unchanged.')
