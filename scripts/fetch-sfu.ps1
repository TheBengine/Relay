[CmdletBinding()]
param([string]$CacheDirectory,[string]$OutputDirectory)
$ErrorActionPreference='Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
. ([IO.Path]::Combine($PSScriptRoot, 'build-publication.ps1'))
$packageRoot=Get-RelayDirectPath ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..')))
if (-not $OutputDirectory) { $OutputDirectory=$packageRoot }
$OutputDirectory=Get-RelayDirectPath $OutputDirectory
$expected='f71a9403935689428aca13e4fa3c2cc0ecb930f75e579ea4e46e772444347b78'
$licenseHash='cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30'
$binary=Get-RelayDirectPath (Join-Path $OutputDirectory 'dist\livekit-server.exe')
$license=Get-RelayDirectPath (Join-Path $OutputDirectory 'docs\LICENSE-livekit.txt')
if ((Test-Path -LiteralPath $binary) -and (Test-Path -LiteralPath $license) -and
    (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash -eq $expected -and
    (Get-FileHash -LiteralPath $license -Algorithm SHA256).Hash -eq $licenseHash) {
    Write-Host 'Pinned Relay security SFU and license already verified.'
    return
}
if (-not $CacheDirectory) {
    $workspaceRoot=[IO.Path]::GetFullPath((Join-Path $packageRoot '..\..'))
    if ((Split-Path (Split-Path $packageRoot -Parent) -Leaf) -eq 'outputs') {
        $CacheDirectory=Join-Path $workspaceRoot 'work\livekit-sfu-security-20261021'
    } else {
        $CacheDirectory=Join-Path $packageRoot 'work\livekit-sfu-security-20261021'
    }
}
$cache=Get-RelayDirectPath $CacheDirectory
$preparedBinary=Get-RelayDirectPath (Join-Path $cache 'livekit-server.exe')
$preparedLicense=Get-RelayDirectPath (Join-Path $cache 'LICENSE')
if (-not (Test-Path -LiteralPath $preparedBinary) -or -not (Test-Path -LiteralPath $preparedLicense)) {
    throw 'Prepare the reviewed SFU in CacheDirectory first; see deploy/sfu-security/README.md. The older official binary is not a security fallback. No files were published.'
}
if ((Get-FileHash -LiteralPath $preparedBinary -Algorithm SHA256).Hash -ne $expected -or
    (Get-FileHash -LiteralPath $preparedLicense -Algorithm SHA256).Hash -ne $licenseHash) {
    throw 'Prepared SFU or license does not match the reviewed security build. No files were published.'
}
# Check both inputs before publishing either; retain old output on failure.
# This prepares build output only; it never stops or installs an application.
[void][IO.Directory]::CreateDirectory((Join-Path $OutputDirectory 'dist'))
[void][IO.Directory]::CreateDirectory((Join-Path $OutputDirectory 'docs'))
Publish-RelayFiles -Files @(@{Source=$preparedBinary;Target=$binary},@{Source=$preparedLicense;Target=$license}) -TransactionDirectory (Join-Path $cache ('publish-'+[Guid]::NewGuid().ToString('N')))
Write-Host 'Pinned LiveKit 1.13.10 Relay security build and license verified.'
