[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$PackageRoot,
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$')][string]$Version,
    [string[]]$BinaryNames = @('relay.exe', 'relay-server.exe', 'relay-network.exe'),
    [string]$SourceRoot
)
$ErrorActionPreference = 'Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
. ([IO.Path]::Combine($PSScriptRoot, 'build-publication.ps1'))
$root = Get-RelayDirectPath $PackageRoot
if (-not $SourceRoot) { $SourceRoot=$root }
$SourceRoot=Get-RelayDirectPath $SourceRoot
if (-not $Version) {
    $source = [IO.File]::ReadAllText((Get-RelayDirectPath (Join-Path $SourceRoot 'Cargo.toml')))
    $section = [regex]::Match($source, '(?ms)^\[workspace\.package\]\s*\r?\n(?<body>.*?)(?=^\[|\z)')
    $match = [regex]::Match($section.Groups['body'].Value, '(?m)^\s*version\s*=\s*"(?<version>[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?)"\s*(?:#.*)?$')
    if (-not $match.Success) { throw 'Cannot determine workspace.package.version.' }
    $Version = $match.Groups['version'].Value
}
if (@($BinaryNames | Sort-Object -Unique).Count -ne $BinaryNames.Count) { throw 'Duplicate binary manifest names.' }
$dist = Join-Path $root 'dist'
$records = foreach ($name in $BinaryNames) {
    if ($name -notin @('relay.exe', 'relay-server.exe', 'relay-network.exe', 'relay-iroh.exe', 'livekit-server.exe')) { throw 'Unexpected binary manifest name.' }
    $path = Get-RelayDirectPath (Join-Path $dist $name)
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Final binary is missing: $path" }
    [ordered]@{ name=$name; bytes=(Get-Item -LiteralPath $path).Length; sha256=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
}
$manifest = [ordered]@{ version=$Version; platform='windows-x64'; generatedUtc=[DateTime]::UtcNow.ToString('o'); binaries=@($records) }
$json = $manifest | ConvertTo-Json -Depth 4
$pending = Join-Path $dist ('manifest-' + [Guid]::NewGuid().ToString('N') + '.pending')
try {
    [IO.File]::WriteAllText($pending, $json, [Text.UTF8Encoding]::new($false))
    Publish-RelayFiles -Files @(@{Source=$pending;Target=(Join-Path $dist 'manifest.json')}) -TransactionDirectory ($pending+'.transaction')
} finally {
    if (Test-Path -LiteralPath $pending) { Remove-Item -LiteralPath $pending -Force }
}
