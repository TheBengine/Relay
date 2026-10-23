[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$SourceDirectory,
    [string]$DataDirectory,
    [switch]$OfflineConfirmed,
    [string]$ServerBinary
)
$ErrorActionPreference='Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
if(-not $OfflineConfirmed){throw 'Stop the older host first, then pass -OfflineConfirmed to import its persistent data.'}
$packageRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if(-not $ServerBinary){$ServerBinary=Join-Path $packageRoot 'dist\relay-server.exe'}
$source=[IO.Path]::GetFullPath($SourceDirectory).TrimEnd('\')
. (Join-Path $PSScriptRoot 'hosting-paths.ps1')
$destination=(Resolve-RelayDataDirectory -DataDirectory $DataDirectory -PackageRoot $packageRoot).TrimEnd('\')
if($source -eq $destination -or $destination -eq [IO.Path]::GetPathRoot($destination).TrimEnd('\')){throw 'Choose distinct, dedicated source and destination folders.'}
if(-not(Test-Path -LiteralPath (Join-Path $source 'relay.sqlite3') -PathType Leaf)){throw 'Source community database was not found.'}
if(Test-Path -LiteralPath $destination){if(@(Get-ChildItem -LiteralPath $destination -Force).Count -ne 0){throw 'Destination is not empty; it was left unchanged.'}}
if(Test-Path -LiteralPath (Join-Path $source 'server-runtime.json')){throw 'The source still has a managed runtime record; stop that host before importing.'}
$parent=Split-Path $destination -Parent
New-Item -ItemType Directory -Path $parent -Force | Out-Null
$staging=[IO.Path]::GetFullPath((Join-Path $parent ('relay-import-'+[Guid]::NewGuid().ToString('N'))))
if(-not $staging.StartsWith($parent.TrimEnd('\')+'\',[StringComparison]::OrdinalIgnoreCase)){throw 'Import staging escaped its destination parent.'}
New-Item -ItemType Directory -Path $staging | Out-Null
# Preserve all ordinary persistent files (including older filesystem attachments)
# while omitting active process credentials/configuration and SQLite sidecars.
$runtimeNames=@('relay.sqlite3','relay.sqlite3-wal','relay.sqlite3-shm','server.lock','stack-launch.lock','managed-instance.json','server-runtime.json','sfu-instance.json','stack-instance.json','stack-supervisor.json','livekit.yaml')
function Copy-PersistentFiles([string]$From,[string]$To){
    foreach($item in Get-ChildItem -LiteralPath $From -Force){
        if($item.Attributes -band [IO.FileAttributes]::ReparsePoint){continue}
        if($runtimeNames -contains $item.Name -or $item.Extension -eq '.pid'){continue}
        $target=Join-Path $To $item.Name
        if($item.PSIsContainer){New-Item -ItemType Directory -Path $target | Out-Null;Copy-PersistentFiles $item.FullName $target}else{Copy-Item -LiteralPath $item.FullName -Destination $target}
    }
}
& $ServerBinary --data $source --raw-backup (Join-Path $staging 'relay.sqlite3')
if($LASTEXITCODE -ne 0){throw "Verified snapshot failed. Source was preserved; staging remains at $staging"}
Copy-PersistentFiles $source $staging
# Both paths are resolved inside the explicitly selected destination parent.
# Refuse an unexpected concurrent destination change; delete only an empty folder.
if(Test-Path -LiteralPath $destination){[IO.Directory]::Delete($destination,$false)}
[IO.Directory]::Move($staging,$destination)
Write-Host 'Verified community and persistent attachment files imported. Source was preserved. Start the new host to migrate the imported copy.'
