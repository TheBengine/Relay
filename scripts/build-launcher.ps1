[CmdletBinding()]
param([string]$OutputPath)

$ErrorActionPreference = 'Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'build-publication.ps1'))
$packageRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$source = Join-Path $packageRoot 'launcher\RelayLauncher.cs'
if (-not $OutputPath) { $OutputPath = Join-Path $packageRoot 'Relay.exe' }
$OutputPath = Get-RelayDirectPath $OutputPath

$compilerCandidates = @(
    (Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'),
    (Join-Path $env:WINDIR 'Microsoft.NET\Framework\v4.0.30319\csc.exe')
)
$compiler = $compilerCandidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
if (-not $compiler) { throw 'The installed Windows .NET Framework C# compiler could not be found.' }
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Launcher source is missing: $source" }

# Prefer the same asset used by the Relay client. The other locations support
# source-light build folders without requiring an icon download or a new tool.
$iconCandidates = @(
    (Join-Path $packageRoot 'crates\relay-client\assets\relay.ico'),
    (Join-Path $packageRoot 'assets\relay.ico'),
    (Join-Path $packageRoot 'dist\relay.ico'),
    (Join-Path $packageRoot 'relay.ico')
)
$icon = $iconCandidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
$outputDirectory = Split-Path $OutputPath -Parent
New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null
$pending = Join-Path $outputDirectory ('launcher-build-' + [Guid]::NewGuid().ToString('N') + '.exe')
$compilerArguments = @(
    '/nologo', '/target:winexe', '/platform:anycpu', '/optimize+', '/utf8output',
    '/reference:System.Windows.Forms.dll', "/out:$pending"
)
if ($icon) { $compilerArguments += "/win32icon:$icon" }
$compilerArguments += $source
try {
    & $compiler @compilerArguments
    if ($LASTEXITCODE -ne 0) { throw "Relay launcher build failed with exit code $LASTEXITCODE." }
    if (-not [IO.File]::Exists($pending)) { throw 'The compiler did not produce Relay.exe.' }
    Publish-RelayFiles -Files @(@{Source=$pending;Target=$OutputPath}) -TransactionDirectory ($pending+'.transaction')
} finally {
    if ([IO.File]::Exists($pending)) { [void](Get-RelayDirectPath $pending); [IO.File]::Delete($pending) }
}
Write-Output $OutputPath
