[CmdletBinding()]
param([string]$CacheDirectory,[string]$VerifyDirectory,[switch]$PassThru)
$ErrorActionPreference='Stop'
. ([IO.Path]::Combine($PSScriptRoot,'shell-runtime.ps1'))
$packageRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if(-not $CacheDirectory){
    $workspaceRoot=[IO.Path]::GetFullPath((Join-Path $packageRoot '..\..'))
    if((Split-Path (Split-Path $packageRoot -Parent) -Leaf) -eq 'outputs'){$CacheDirectory=Join-Path $workspaceRoot 'work\relay-webrtc-89d790b'}else{$CacheDirectory=Join-Path $packageRoot 'work\relay-webrtc-89d790b'}
}
function Assert-DirectSdkPath([string]$Path){
    $current=[IO.Path]::GetFullPath($Path)
    while($current){
        try{if(([IO.File]::GetAttributes($current) -band [IO.FileAttributes]::ReparsePoint) -ne 0){throw "Native SDK path contains a junction or symbolic link: $current"}}catch [IO.FileNotFoundException]{}catch [IO.DirectoryNotFoundException]{}
        $current=[IO.Path]::GetDirectoryName($current)
    }
}
function Stream-Sha256($Stream){
    $algorithm=[Security.Cryptography.SHA256]::Create()
    try{return [BitConverter]::ToString($algorithm.ComputeHash($Stream)).Replace('-','').ToLowerInvariant()}finally{$algorithm.Dispose()}
}
$cache=[IO.Path]::GetFullPath($CacheDirectory)
Assert-DirectSdkPath $cache
New-Item -ItemType Directory -Path $cache -Force | Out-Null
$zipPath=Join-Path $cache 'webrtc-win-x64-release.zip'
$url='https://github.com/livekit/rust-sdks/releases/download/webrtc-89d790b/webrtc-win-x64-release.zip'
# Official GitHub release asset SHA256, checked 2026-10-04. This is the SDK's
# existing pinned codec build, not a change to its ABI or WebRTC version.
$expected='5c2349c960bae4f06f71c102f58552d0d09c912a142811acfcadfb7c883cbf58'
Assert-DirectSdkPath $zipPath
if(-not(Test-Path -LiteralPath $zipPath -PathType Leaf)){
    $pending=Join-Path $cache ('download-'+[Guid]::NewGuid().ToString('N')+'.pending')
    Invoke-WebRequest -Uri $url -OutFile $pending -UseBasicParsing
    if((Get-FileHash -LiteralPath $pending -Algorithm SHA256).Hash -ne $expected){throw 'Official native WebRTC download checksum mismatch.'}
    Assert-DirectSdkPath $zipPath
    Move-Item -LiteralPath $pending -Destination $zipPath
}
if((Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash -ne $expected){throw 'Cached native WebRTC archive checksum mismatch.'}
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive=[IO.Compression.ZipFile]::OpenRead($zipPath)
$tree=Join-Path $cache 'verified\win-x64-release'
$extraction=$null
try{
    $files=@{};$total=0L
    if($archive.Entries.Count -gt 50000){throw 'Native SDK archive contains too many entries.'}
    foreach($entry in $archive.Entries){
        $name=$entry.FullName.Replace('/','\')
        if(-not $name.StartsWith('win-x64-release\',[StringComparison]::Ordinal) -or $name.Contains(':') -or ($name.Split('\') -contains '..') -or [IO.Path]::IsPathRooted($name)){throw 'Native SDK archive contains an unsafe path.'}
        if(($entry.ExternalAttributes -shr 16 -band 61440) -eq 40960){throw 'Native SDK archive contains a symbolic link.'}
        $total+=$entry.Length;if($total -gt 2GB){throw 'Native SDK archive expands beyond the bounded size.'}
        if($entry.Name){
            $relative=$name.Substring('win-x64-release\'.Length)
            if($files.ContainsKey($relative)){throw 'Native SDK archive has duplicate file paths.'}
            $files[$relative]=$entry
        }
    }
    if(-not $files.ContainsKey('lib\webrtc.lib') -or -not $files.ContainsKey('LICENSE.md') -or -not $files.ContainsKey('webrtc.ninja')){throw 'Native SDK archive is missing required build/license files.'}
    if($VerifyDirectory){$tree=[IO.Path]::GetFullPath($VerifyDirectory)}
    Assert-DirectSdkPath $tree
    if(-not(Test-Path -LiteralPath $tree -PathType Container)){
        if($VerifyDirectory){throw 'Native SDK directory to verify was not found.'}
        $extraction=Join-Path $cache ('extract-'+[Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $extraction | Out-Null
        foreach($entry in $archive.Entries){
            $target=[IO.Path]::GetFullPath((Join-Path $extraction $entry.FullName.Replace('/','\')))
            if(-not $target.StartsWith($extraction.TrimEnd('\')+'\',[StringComparison]::OrdinalIgnoreCase)){throw 'Native SDK archive escaped extraction.'}
            if($entry.Name){New-Item -ItemType Directory -Path (Split-Path $target -Parent) -Force | Out-Null;[IO.Compression.ZipFileExtensions]::ExtractToFile($entry,$target,$false)}else{New-Item -ItemType Directory -Path $target -Force | Out-Null}
        }
        $tree=Join-Path $extraction 'win-x64-release'
    }
    # Derive each expected hash from the digest-checked ZIP itself, not from an
    # editable cache manifest. Check the entire library/header tree on every use.
    $seen=@{};$directories=[Collections.Generic.Stack[string]]::new();$directories.Push($tree)
    while($directories.Count){
        $directory=$directories.Pop();Assert-DirectSdkPath $directory
        foreach($item in Get-ChildItem -LiteralPath $directory -Force){
            Assert-DirectSdkPath $item.FullName
            if($item.PSIsContainer){$directories.Push($item.FullName);continue}
            $relative=$item.FullName.Substring($tree.Length+1)
            if(-not $files.ContainsKey($relative)){throw "Unverified file in native SDK directory: $relative"}
            $entry=$files[$relative]
            if($item.Length -ne $entry.Length){throw "Native SDK cache integrity mismatch: $relative"}
            $stream=$entry.Open()
            try{$fileExpected=Stream-Sha256 $stream}finally{$stream.Dispose()}
            if((Get-FileHash -LiteralPath $item.FullName -Algorithm SHA256).Hash -ne $fileExpected){throw "Native SDK cache integrity mismatch: $relative"}
            $seen[$relative]=$true
        }
    }
    if($seen.Count -ne $files.Count){throw 'Native SDK cache is incomplete.'}
    if($extraction){
        $parent=Join-Path $cache 'verified';Assert-DirectSdkPath $parent
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
        $destination=Join-Path $parent 'win-x64-release';Assert-DirectSdkPath $destination
        [IO.Directory]::Move($tree,$destination);$tree=$destination
        [IO.Directory]::Delete($extraction,$false)
    }
    Write-Host "Pinned native WebRTC archive and $($seen.Count) library/header/license files verified."
    if($PassThru){Write-Output $tree}
}finally{$archive.Dispose()}
