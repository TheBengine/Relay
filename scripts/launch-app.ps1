[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
$packageRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$appExe = Join-Path $packageRoot 'dist\relay.exe'
if (-not (Test-Path -LiteralPath $appExe -PathType Leaf)) {
    throw 'App binary is missing. Run scripts\build.ps1, or use the compiled package.'
}
Start-Process -FilePath $appExe -WorkingDirectory $packageRoot -WindowStyle Normal | Out-Null
