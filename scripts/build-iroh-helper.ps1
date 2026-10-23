[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$OutputPath,
    [Parameter(Mandatory=$true)][string]$BuildDirectory,
    [ValidateSet('x86_64-pc-windows-msvc')]
    [string]$Target = 'x86_64-pc-windows-msvc'
)
# Builds relay-iroh.exe, the account-free connection. It is a separate program
# with its own workspace and lockfile, so the main Relay build is not touched.
$ErrorActionPreference='Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
. ([IO.Path]::Combine($PSScriptRoot, 'build-publication.ps1'))
$packageRoot=Get-RelayDirectPath (Join-Path $PSScriptRoot '..')
$source=Get-RelayDirectPath (Join-Path $packageRoot 'iroh-helper')
$output=Get-RelayDirectPath $OutputPath
$buildDirectory=Get-RelayDirectPath $BuildDirectory
if(-not [IO.File]::Exists((Join-Path $source 'Cargo.toml'))){throw 'The account-free connection source (iroh-helper) is missing.'}
[void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($output))
$previousTarget=$env:CARGO_TARGET_DIR
$previousFlags=$env:RUSTFLAGS
$pending=Join-Path ([IO.Path]::GetDirectoryName($output)) ('iroh-build-'+[Guid]::NewGuid().ToString('N')+'.exe')
try {
    $env:CARGO_TARGET_DIR=$buildDirectory
    # The same static C runtime as the main client, so no extra runtime is needed on the PC.
    $env:RUSTFLAGS=Get-RelayStaticRuntimeFlags $previousFlags
    Push-Location $source
    try {
        # Offline and locked: the exact dependencies in iroh-helper/Cargo.lock, nothing downloaded.
        & cargo build --release --locked --offline --target $Target
        if($LASTEXITCODE -ne 0){throw 'The account-free connection build failed. Its dependencies must be in the local cargo cache; downloads are disabled.'}
    } finally { Pop-Location }
    Copy-Item -LiteralPath (Join-Path $buildDirectory "$Target\release\relay-iroh.exe") -Destination $pending -Force
    Publish-RelayFiles -Files @(@{Source=$pending;Target=$output}) -TransactionDirectory ($pending+'.transaction')
} finally {
    $env:CARGO_TARGET_DIR=$previousTarget
    $env:RUSTFLAGS=$previousFlags
    if([IO.File]::Exists($pending)){[void](Get-RelayDirectPath $pending);[IO.File]::Delete($pending)}
}
Write-Output $output
