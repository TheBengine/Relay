[CmdletBinding()]
param([string]$DataDirectory,[string]$Destination)
$ErrorActionPreference='Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
$packageRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
. (Join-Path $PSScriptRoot 'hosting-paths.ps1')
$dataRoot=Resolve-RelayDataDirectory -DataDirectory $DataDirectory -PackageRoot $packageRoot
if(-not $Destination){New-Item -ItemType Directory -Path (Join-Path $dataRoot 'backups') -Force | Out-Null;$Destination=Join-Path $dataRoot ('backups\manual-'+[Guid]::NewGuid().ToString()+'.sqlite3')}
& (Join-Path $packageRoot 'dist\relay-server.exe') --data $dataRoot --backup ([IO.Path]::GetFullPath($Destination))
if($LASTEXITCODE -ne 0){throw 'Backup failed. For a running server, export a backup from the administration panel.'}
