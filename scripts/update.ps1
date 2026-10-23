[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Archive,
    [Parameter(Mandatory)][ValidatePattern('^[a-fA-F0-9]{64}$')][string]$ExpectedSha256,
    [string]$DestinationDirectory,
    [string]$DataDirectory
)
$ErrorActionPreference='Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
function Assert-RelayUpdatePath {
    param([Parameter(Mandatory)][string]$Path)
    # GetFullPath alone does not resolve existing junctions/symbolic links. Refuse
    # those at every level rather than writing through an installation alias.
    $current=[IO.Path]::GetFullPath($Path)
    while($current){
        try{
            $attributes=[IO.File]::GetAttributes($current)
            if(($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0){throw "Update path contains a junction or symbolic link: $current. Choose a direct installation/data directory; existing files were left in place."}
        }catch [IO.FileNotFoundException]{}catch [IO.DirectoryNotFoundException]{}
        $current=[IO.Path]::GetDirectoryName($current)
    }
}
$packageRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if(-not $DestinationDirectory){$DestinationDirectory=$packageRoot}
$destination=[IO.Path]::GetFullPath($DestinationDirectory)
. (Join-Path $PSScriptRoot 'hosting-paths.ps1')
$data=Resolve-RelayDataDirectory -DataDirectory $DataDirectory -PackageRoot $packageRoot
Assert-RelayUpdatePath $destination
Assert-RelayUpdatePath $data
$archivePath=[IO.Path]::GetFullPath($Archive)
if((Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash -ne $ExpectedSha256){throw 'Archive checksum mismatch; installation left unchanged.'}
$destinationPrefix=$destination.TrimEnd('\')+'\'
if($data -eq $destination -or $data.StartsWith($destinationPrefix,[StringComparison]::OrdinalIgnoreCase)){throw 'Updates require server data outside the application directory. Move portable data first.'}
$protected=@([IO.Path]::GetPathRoot($destination),$env:SystemRoot,$env:USERPROFILE,$env:LOCALAPPDATA)
if($destination.TrimEnd('\') -eq [IO.Path]::GetPathRoot($destination).TrimEnd('\') -or $protected -contains $destination.TrimEnd('\')){throw 'Choose a dedicated application directory.'}
foreach($process in Get-Process -ErrorAction SilentlyContinue){
    try{if($process.Path -eq (Join-Path $destination 'dist\relay.exe')){throw 'Close Relay before updating its application files.'}}catch [System.ComponentModel.Win32Exception]{}
}
Add-Type -AssemblyName System.IO.Compression.FileSystem
$work=Join-Path $env:LOCALAPPDATA ('Relay\update-work\'+[Guid]::NewGuid().ToString('N'))
$staging=Join-Path $work 'staging';$rollback=Join-Path $work 'rollback'
Assert-RelayUpdatePath $work
New-Item -ItemType Directory -Path $staging,$rollback -Force | Out-Null
$zip=[IO.Compression.ZipFile]::OpenRead($archivePath)
try{
    if($zip.Entries.Count -gt 2000){throw 'Archive contains too many entries.'}
    $total=0L
    foreach($entry in $zip.Entries){
        $total+=$entry.Length;if($total -gt 2GB){throw 'Archive is too large.'}
        $name=$entry.FullName.Replace('/','\')
        if(-not $name.StartsWith('Relay\',[StringComparison]::Ordinal) -or [IO.Path]::IsPathRooted($name) -or ($name.Split('\') -contains '..')){throw 'Archive contains an unsafe path.'}
        if($name.Contains(':')){throw 'Archive contains an alternate-stream path.'}
        if(($entry.ExternalAttributes -shr 16 -band 61440) -eq 40960){throw 'Archive contains a symbolic link.'}
        $target=[IO.Path]::GetFullPath((Join-Path $staging $name))
        if(-not $target.StartsWith($staging.TrimEnd('\')+'\',[StringComparison]::OrdinalIgnoreCase)){throw 'Archive path escaped staging.'}
        if($entry.Name){New-Item -ItemType Directory -Path (Split-Path $target -Parent) -Force | Out-Null;[IO.Compression.ZipFileExtensions]::ExtractToFile($entry,$target,$false)}else{New-Item -ItemType Directory -Path $target -Force | Out-Null}
    }
}finally{$zip.Dispose()}
$source=Join-Path $staging 'Relay'
$manifest=Get-Content -LiteralPath (Join-Path $source 'dist\manifest.json') -Raw | ConvertFrom-Json
if($manifest.platform -ne 'windows-x64'){throw 'Unexpected package platform.'}
if($manifest.version -isnot [string] -or $manifest.version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$'){throw 'Unexpected package version.'}
$names=@()
foreach($binary in $manifest.binaries){
    if($binary.name -notin @('relay.exe','relay-server.exe','livekit-server.exe')){throw 'Unexpected executable in binary manifest.'}
    if($names -contains $binary.name){throw 'Duplicate binary manifest record.'};$names+=$binary.name
    $path=Join-Path $source ('dist\'+$binary.name)
    if((Get-Item -LiteralPath $path).Length -ne $binary.bytes -or (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ne $binary.sha256){throw 'Binary integrity mismatch.'}
}
if($names -notcontains 'relay.exe' -or $names -notcontains 'relay-server.exe' -or $names -notcontains 'livekit-server.exe'){throw 'The complete package is missing a native binary.'}
foreach($file in Get-ChildItem -LiteralPath (Join-Path $source 'dist') -File){if($file.Name -notin ($names+@('manifest.json'))){throw 'Unverified file in binary directory.'}}
foreach($file in Get-ChildItem -LiteralPath $source -File -Recurse){
    Assert-RelayUpdatePath (Join-Path $destination ($file.FullName.Substring($source.Length+1)))
}
# Finish all integrity checks before stopping an existing managed host.
$oldServer=Join-Path $destination 'dist\relay-server.exe'
if(-not(Test-Path -LiteralPath (Join-Path $data 'server-runtime.json'))){
    foreach($process in Get-Process -ErrorAction SilentlyContinue){try{if($process.Path -eq $oldServer){throw 'Stop the older server with its original stop script before updating.'}}catch [System.ComponentModel.Win32Exception]{}}
}
if(Test-Path -LiteralPath (Join-Path $data 'server-runtime.json')){
    & $oldServer --data $data --stop
    if($LASTEXITCODE -ne 0){throw 'Managed server could not stop safely; application files left unchanged.'}
    for($attempt=0;$attempt -lt 100;$attempt++){
        if(-not(Test-Path -LiteralPath (Join-Path $data 'server-runtime.json')) -and -not(Test-Path -LiteralPath (Join-Path $data 'stack-instance.json'))){break}
        Start-Sleep -Milliseconds 100
    }
    if((Test-Path -LiteralPath (Join-Path $data 'server-runtime.json')) -or (Test-Path -LiteralPath (Join-Path $data 'stack-instance.json'))){throw 'The managed stack is still stopping; application files left unchanged.'}
}
if(Test-Path -LiteralPath (Join-Path $data 'stack-instance.json')){throw 'The owned video stack is still stopping; retry when its supervisor has finished cleanup.'}
if(Test-Path -LiteralPath (Join-Path $data 'relay.sqlite3')){
    $backup=Join-Path $data ('backups\before-update-'+[Guid]::NewGuid().ToString()+'.sqlite3')
    Assert-RelayUpdatePath $backup
    New-Item -ItemType Directory -Path (Split-Path $backup -Parent) -Force | Out-Null
    & (Join-Path $source 'dist\relay-server.exe') --data $data --raw-backup $backup
    if($LASTEXITCODE -ne 0){throw 'Recovery backup failed; application files left unchanged.'}
}
New-Item -ItemType Directory -Path $destination -Force | Out-Null
$ledger=@()
try{
    foreach($file in Get-ChildItem -LiteralPath $source -File -Recurse){
        $relative=$file.FullName.Substring($source.Length+1)
        $target=Join-Path $destination $relative
        $saved=Join-Path $rollback $relative
        Assert-RelayUpdatePath $target
        $existed=Test-Path -LiteralPath $target -PathType Leaf
        if($existed){New-Item -ItemType Directory -Path (Split-Path $saved -Parent) -Force | Out-Null;Copy-Item -LiteralPath $target -Destination $saved}
        $ledger+=@{target=$target;saved=$saved;existed=$existed}
        New-Item -ItemType Directory -Path (Split-Path $target -Parent) -Force | Out-Null
        Assert-RelayUpdatePath $target
        Copy-Item -LiteralPath $file.FullName -Destination $target -Force
    }
}catch{
    $failure=$_
    foreach($record in $ledger){
        try{
            Assert-RelayUpdatePath $record.target
            if($record.existed){Copy-Item -LiteralPath $record.saved -Destination $record.target -Force}elseif(Test-Path -LiteralPath $record.target){Remove-Item -LiteralPath $record.target}
        }catch{Write-Warning "Could not restore $($record.target) safely; recovery files remain in $rollback."}
    }
    throw $failure
}
Write-Host "Installed Relay $($manifest.version). Source and binaries verified; rollback files retained in $rollback"
Write-Host 'Start the app/server normally. Existing community data remains in its separate directory.'
