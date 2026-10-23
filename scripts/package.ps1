[CmdletBinding()]
param(
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$')]
    [string]$Version,
    [string]$ClientBinary,
    [string]$ServerBinary,
    [string]$NetworkBinary,
    [string]$ArchiveDirectory
)

$ErrorActionPreference = 'Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
. ([IO.Path]::Combine($PSScriptRoot, 'build-publication.ps1'))
$packageRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$outputRoot = [IO.Path]::GetFullPath((Split-Path $packageRoot -Parent))
if ($ArchiveDirectory) { $outputRoot=Get-RelayDirectPath $ArchiveDirectory }
$workspaceRoot = [IO.Path]::GetFullPath((Join-Path $packageRoot '..\..'))
$workRoot = if ((Split-Path $outputRoot -Leaf) -eq 'outputs') {
    Join-Path $workspaceRoot 'work'
} else {
    Join-Path $packageRoot 'work'
}
$workRoot = Get-RelayDirectPath $workRoot
if (-not $Version) {
    $workspaceSource = [IO.File]::ReadAllText((Join-Path $packageRoot 'Cargo.toml'))
    $workspacePackage = [regex]::Match($workspaceSource, '(?ms)^\[workspace\.package\]\s*\r?\n(?<body>.*?)(?=^\[|\z)')
    $workspaceVersion = [regex]::Match($workspacePackage.Groups['body'].Value, '(?m)^\s*version\s*=\s*"(?<version>[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?)"\s*(?:#.*)?$')
    if (-not $workspaceVersion.Success) { throw 'Cannot determine workspace.package.version; pass -Version explicitly.' }
    $Version = $workspaceVersion.Groups['version'].Value
}
$dist = Get-RelayDirectPath (Join-Path $packageRoot 'dist')
New-Item -ItemType Directory -Path $workRoot -Force | Out-Null
# This is the legacy, source-included PRIVATE archive path. It never publishes
# binaries or a new manifest back into the selected source checkout.
$binarySources=[ordered]@{
    'relay.exe'=$(if($ClientBinary){Get-RelayDirectPath $ClientBinary}else{Join-Path $dist 'relay.exe'})
    'relay-server.exe'=$(if($ServerBinary){Get-RelayDirectPath $ServerBinary}else{Join-Path $dist 'relay-server.exe'})
}
$networkSource=if($NetworkBinary){Get-RelayDirectPath $NetworkBinary}else{Join-Path $dist 'relay-network.exe'}
if([IO.File]::Exists((Get-RelayDirectPath $networkSource))){$binarySources['relay-network.exe']=$networkSource}
elseif(([Version]([regex]::Match($Version,'^[0-9]+\.[0-9]+\.[0-9]+').Value)) -ge [Version]'0.10.0'){
    throw 'The current complete package requires dist/relay-network.exe.'
}
if([IO.File]::Exists((Get-RelayDirectPath (Join-Path $dist 'livekit-server.exe')))){$binarySources['livekit-server.exe']=Join-Path $dist 'livekit-server.exe'}
# The account-free connection is optional: a package without it simply does not offer that route.
if([IO.File]::Exists((Get-RelayDirectPath (Join-Path $dist 'relay-iroh.exe')))){$binarySources['relay-iroh.exe']=Join-Path $dist 'relay-iroh.exe'}
$binaryNames=@($binarySources.Keys)
foreach($source in $binarySources.Values){
    if(-not [IO.File]::Exists((Get-RelayDirectPath $source))){throw "Final binary does not exist: $source"}
}
if(([Version]([regex]::Match($Version, '^[0-9]+\.[0-9]+\.[0-9]+').Value)) -ge [Version]'0.3.0' -and $binaryNames -notcontains 'livekit-server.exe'){throw 'The complete native RTC package requires dist/livekit-server.exe.'}

$stagingRoot = [IO.Path]::GetFullPath((Join-Path $workRoot ('relay-package-' + [Guid]::NewGuid().ToString('N'))))
$workPrefix = $workRoot.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
if (-not $stagingRoot.StartsWith($workPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Package staging must remain inside the work directory.'
}
$stagedPackage = Join-Path $stagingRoot 'Relay'
New-Item -ItemType Directory -Path $stagedPackage -Force | Out-Null

# Root inputs are explicit; source folders also prune runtime artifacts at every depth.
$files = @('.gitignore', 'Cargo.toml', 'Cargo.lock', 'LICENSE', 'LICENSING.md', 'CONTRIBUTING.md', 'SECURITY.md', 'README.md', 'THIRD_PARTY_NOTICES.txt',
    'Relay.exe', 'start-app.cmd', 'start-server.cmd', 'stop-server.cmd')
$directories = @('crates', 'docs', 'scripts', '.cargo', 'vendor', 'launcher', 'LICENSES')
if([IO.Directory]::Exists((Get-RelayDirectPath (Join-Path $packageRoot 'network-helper')))){$directories+='network-helper'}
if([IO.Directory]::Exists((Get-RelayDirectPath (Join-Path $packageRoot 'iroh-helper')))){$directories+='iroh-helper'}
$excludedDirectories = @('work', 'target', 'data', 'logs', 'relay-data', 'server-data',
    '.git', '__pycache__', '.pytest_cache', '.venv', 'venv', 'node_modules')
$excludedFilePattern = '(?i)(\.(db|sqlite|sqlite3)([-.].*)?$|\.sqlite3.*$|\.(log|pid|pyc|pyo|exe|pdb|obj|rlib|rmeta|tmp|device|retired|pending|history)$|^[a-f0-9]{64}\.lock$|^(preferences|tickets|server-runtime|managed-instance|stack-instance|stack-supervisor|sfu-instance)\.json$|^livekit\.yaml$|^(server|stack-launch)\.lock$|^\.env(\..*)?$)'

function Copy-PackageSourceDirectory {
    param([string]$Source, [string]$Destination)
    New-Item -ItemType Directory -Path $Destination -Force | Out-Null
    foreach ($item in Get-ChildItem -LiteralPath $Source -Force) {
        # Do not follow links to files or trees outside the selected source folder.
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { continue }
        if ($item.PSIsContainer) {
            if ($excludedDirectories -contains $item.Name.ToLowerInvariant() -or $item.Name.EndsWith('.history', [StringComparison]::OrdinalIgnoreCase)) { continue }
            Copy-PackageSourceDirectory -Source $item.FullName -Destination (Join-Path $Destination $item.Name)
        } elseif ($item.Name -notmatch $excludedFilePattern) {
            Copy-Item -LiteralPath $item.FullName -Destination (Join-Path $Destination $item.Name) -Force
        }
    }
}

foreach ($name in $files) {
    $source = Join-Path $packageRoot $name
    if (-not (Test-Path -LiteralPath $source)) { throw "Package input is missing: $source" }
    Copy-Item -LiteralPath $source -Destination $stagedPackage -Force
}
foreach ($name in $directories) {
    $source = Join-Path $packageRoot $name
    if (-not (Test-Path -LiteralPath $source -PathType Container)) { throw "Package input is missing: $source" }
    Copy-PackageSourceDirectory -Source $source -Destination (Join-Path $stagedPackage $name)
}
$stagedDist = Join-Path $stagedPackage 'dist'
New-Item -ItemType Directory -Path $stagedDist -Force | Out-Null
foreach ($name in $binaryNames) {
    Copy-Item -LiteralPath $binarySources[$name] -Destination (Join-Path $stagedDist $name) -Force
}
& (Join-Path $PSScriptRoot 'write-binary-manifest.ps1') -PackageRoot $stagedPackage -Version $Version -BinaryNames $binaryNames

# Upstream crate archives can carry Unix-epoch timestamps, outside ZIP's range.
# Change only staging metadata; original source timestamps and bytes stay intact.
$zipMinimumTimestamp = [DateTime]::new(1980, 1, 1, 0, 0, 0, [DateTimeKind]::Local)
$zipMaximumTimestamp = [DateTime]::new(2107, 12, 31, 23, 59, 58, [DateTimeKind]::Local)
$zipFallbackTimestampUtc = [DateTime]::new(2000, 1, 2, 12, 0, 0, [DateTimeKind]::Utc)
foreach ($stagedItem in Get-ChildItem -LiteralPath $stagedPackage -Force -Recurse) {
    if ($stagedItem.LastWriteTime -lt $zipMinimumTimestamp -or $stagedItem.LastWriteTime -gt $zipMaximumTimestamp) {
        $stagedItem.LastWriteTimeUtc = $zipFallbackTimestampUtc
    }
}

$archive = Join-Path $outputRoot "relay-$Version-windows-x64.zip"
try {
    $pendingArchive=Join-Path $stagingRoot 'package.zip'
    $pendingChecksum=Join-Path $stagingRoot 'package.zip.sha256'
    Compress-Archive -LiteralPath $stagedPackage -DestinationPath $pendingArchive -CompressionLevel Optimal
    $hash = (Get-FileHash -LiteralPath $pendingArchive -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath $pendingChecksum -Value "$hash  $([IO.Path]::GetFileName($archive))" -Encoding ascii
    Publish-RelayFiles -Files @(@{Source=$pendingArchive;Target=$archive},@{Source=$pendingChecksum;Target="$archive.sha256"}) -TransactionDirectory (Join-Path $stagingRoot 'archive-rollback')
    Write-Host "Source-included Windows package: $archive"
    Write-Host "SHA-256: $hash"
} finally {
    # Resolve again immediately before recursive cleanup; never delete the work directory itself.
    $cleanupPath = Get-RelayDirectPath $stagingRoot
    if ($cleanupPath -eq $workRoot -or -not $cleanupPath.StartsWith($workPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Unsafe staging cleanup path.'
    }
    if (Test-Path -LiteralPath $cleanupPath) { Remove-Item -LiteralPath $cleanupPath -Recurse -Force }
}
