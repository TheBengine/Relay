[CmdletBinding()]
param([Parameter(Mandatory)][string]$BackupFile,[string]$DataDirectory)
$ErrorActionPreference='Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
$packageRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
. (Join-Path $PSScriptRoot 'hosting-paths.ps1')
$DataDirectory=Resolve-RelayDataDirectory -DataDirectory $DataDirectory -PackageRoot $packageRoot
if(-not(Test-Path -LiteralPath $BackupFile -PathType Leaf)){throw 'Backup file does not exist.'}
& (Join-Path $packageRoot 'dist\relay-server.exe') --data ([IO.Path]::GetFullPath($DataDirectory)) --restore ([IO.Path]::GetFullPath($BackupFile))
if($LASTEXITCODE -ne 0){throw 'Restore failed; existing server data was preserved. Stop the server before offline restore.'}
