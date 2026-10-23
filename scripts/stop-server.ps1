[CmdletBinding()]
param([string]$DataDirectory)
$ErrorActionPreference='Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
$packageRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$serverExe=Join-Path $packageRoot 'dist\relay-server.exe'
. (Join-Path $PSScriptRoot 'hosting-paths.ps1')
$dataRoot=Resolve-RelayDataDirectory -DataDirectory $DataDirectory -PackageRoot $packageRoot
if(-not(Test-Path -LiteralPath (Join-Path $dataRoot 'server-runtime.json'))){Write-Host 'No managed server is running in this data directory.';exit 0}
& $serverExe --data $dataRoot --stop
if($LASTEXITCODE -ne 0){throw 'Graceful stop failed. No process was killed.'}
for($attempt=0;$attempt -lt 50;$attempt++){
    if(-not(Test-Path -LiteralPath (Join-Path $dataRoot 'server-runtime.json')) -and -not(Test-Path -LiteralPath (Join-Path $dataRoot 'stack-instance.json'))){Write-Host 'Relay and its owned video server stopped.';exit 0}
    Start-Sleep -Milliseconds 200
}
throw 'Shutdown was requested but has not finished. Inspect the server log.'
