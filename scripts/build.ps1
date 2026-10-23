[CmdletBinding()]
param(
    [string]$BuildDirectory,
    [string]$CMakeDirectory,
    [string]$NativeSdkDirectory,
    [string]$SfuCacheDirectory,
    [string]$OutputDirectory,
    [string]$GoExecutable,
    [string]$GoModuleCache,
    [string]$GoBuildCache,
    [ValidateSet('x86_64-pc-windows-msvc')]
    [string]$Target = 'x86_64-pc-windows-msvc',
    [switch]$DebugBuild,
    [switch]$WithoutNativeRtc,
    # Leaves out the account-free connection (relay-iroh.exe) and its route.
    [switch]$WithoutIroh
)
$ErrorActionPreference = 'Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
. ([IO.Path]::Combine($PSScriptRoot, 'build-publication.ps1'))
. ([IO.Path]::Combine($PSScriptRoot, 'go-build-tools.ps1'))
$packageRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$workspaceRoot = [IO.Path]::GetFullPath((Join-Path $packageRoot '..\..'))
if (-not $BuildDirectory) {
    if ((Split-Path (Split-Path $packageRoot -Parent) -Leaf) -eq 'outputs') {
        $BuildDirectory = Join-Path $workspaceRoot 'work\relay-build-rtc'
    } else {
        $BuildDirectory = Join-Path $packageRoot 'work\relay-build-rtc'
    }
}
$BuildDirectory = Get-RelayDirectPath $BuildDirectory
if (-not $OutputDirectory) { $OutputDirectory=$packageRoot }
$OutputDirectory=Get-RelayDirectPath $OutputDirectory
if ($NativeSdkDirectory) { $NativeSdkDirectory=Get-RelayDirectPath $NativeSdkDirectory }
if ($SfuCacheDirectory) { $SfuCacheDirectory=Get-RelayDirectPath $SfuCacheDirectory }
$publicationStage=Join-Path $BuildDirectory ('publish-'+[Guid]::NewGuid().ToString('N'))
if ($CMakeDirectory) {
    $CMakeDirectory = Get-RelayDirectPath $CMakeDirectory
    $cmakeExecutable = Get-RelayDirectPath (Join-Path $CMakeDirectory 'cmake.exe')
    if (-not [IO.File]::Exists($cmakeExecutable)) {
        throw 'CMakeDirectory must contain cmake.exe. Alternatively prepare CMake on PATH.'
    }
}
# Without an explicit CMakeDirectory, use the caller's configured PATH. Do not
# recursively discover tools: a work folder may contain redirected subtrees.
$goSettings = Resolve-RelayGoBuildSettings -GoExecutable $GoExecutable -GoModuleCache $GoModuleCache -GoBuildCache $GoBuildCache
$previousPath = $env:PATH
$cmakeEnvironmentNames=@('CMAKE',('CMAKE_'+$Target),('CMAKE_'+$Target.Replace('-','_')),'HOST_CMAKE','TARGET_CMAKE')
$previousCMakeEnvironment=@{}
foreach($name in $cmakeEnvironmentNames){$previousCMakeEnvironment[$name]=[Environment]::GetEnvironmentVariable($name,'Process')}
$previousTarget = $env:CARGO_TARGET_DIR
$previousCMakeMinimum = $env:CMAKE_POLICY_VERSION_MINIMUM
$previousRustFlags = $env:RUSTFLAGS
$previousNativeSdk = $env:LK_CUSTOM_WEBRTC
try {
    if ($CMakeDirectory) {
        $env:PATH = "$CMakeDirectory;$env:PATH"
        # cmake 0.1.58 checks target-specific and HOST_/TARGET_ overrides before
        # CMAKE. Bind every applicable spelling to the chosen executable.
        foreach($name in $cmakeEnvironmentNames){[Environment]::SetEnvironmentVariable($name,$cmakeExecutable,'Process')}
    }
    # Bundled Opus uses an older CMake minimum; CMake 4 accepts it with this policy floor.
    $env:CMAKE_POLICY_VERSION_MINIMUM = '3.5'
    $env:CARGO_TARGET_DIR = $BuildDirectory
    $env:RUSTFLAGS = Get-RelayStaticRuntimeFlags $previousRustFlags
    if(-not $WithoutNativeRtc){
        # Refuse missing/unreviewed SFU input before the expensive Rust/SDK build.
        $sfuArguments=@{OutputDirectory=$publicationStage}
        if ($SfuCacheDirectory) { $sfuArguments.CacheDirectory=$SfuCacheDirectory }
        & (Join-Path $PSScriptRoot 'fetch-sfu.ps1') @sfuArguments
        if ($NativeSdkDirectory) {
            # build.rs still checks the pinned patch, recipe, archive and every
            # SDK file. Reusing a reviewed SDK does not skip native validation.
            $env:LK_CUSTOM_WEBRTC=$NativeSdkDirectory
        } else {
            $env:LK_CUSTOM_WEBRTC=& (Join-Path $PSScriptRoot 'prepare-webrtc.ps1') -PassThru
        }
    }
    Push-Location $packageRoot
    try {
        $cargoArguments=@('build','--workspace','--locked','--target',$Target)
        if($WithoutNativeRtc){$cargoArguments+='--no-default-features'}else{$cargoArguments+=@('--features','relay-client/native-rtc')}
        if(-not $DebugBuild){$cargoArguments+='--release'}
        & cargo @cargoArguments
        if ($LASTEXITCODE -ne 0) { throw "Cargo build failed with exit code $LASTEXITCODE." }
    } finally { Pop-Location }
    $profile = if ($DebugBuild) { 'debug' } else { 'release' }
    $dist = Join-Path $publicationStage 'dist'
    New-Item -ItemType Directory -Path $dist,(Join-Path $publicationStage 'docs') -Force | Out-Null
    foreach ($binary in @('relay.exe', 'relay-server.exe')) {
        Copy-Item -LiteralPath (Join-Path $BuildDirectory "$Target\$profile\$binary") -Destination $dist -Force
    }
    & (Join-Path $PSScriptRoot 'build-network-helper.ps1') -OutputPath (Join-Path $dist 'relay-network.exe') -GoExecutable $goSettings.Executable -GoModuleCache $goSettings.ModuleCache -GoBuildCache $goSettings.BuildCache | Out-Null
    if(-not $WithoutNativeRtc){
        Copy-Item -LiteralPath (Join-Path $env:LK_CUSTOM_WEBRTC 'LICENSE.md') -Destination (Join-Path $publicationStage 'docs\LICENSE-libwebrtc.md') -Force
    }
    $manifestNames = @('relay.exe', 'relay-server.exe', 'relay-network.exe')
    if(-not $WithoutIroh){
        & (Join-Path $PSScriptRoot 'build-iroh-helper.ps1') -OutputPath (Join-Path $dist 'relay-iroh.exe') -BuildDirectory (Join-Path $BuildDirectory 'iroh-helper') | Out-Null
        $manifestNames += 'relay-iroh.exe'
    }
    if (Test-Path -LiteralPath (Join-Path $dist 'livekit-server.exe')) { $manifestNames += 'livekit-server.exe' }
    & (Join-Path $PSScriptRoot 'write-binary-manifest.ps1') -PackageRoot $publicationStage -SourceRoot $packageRoot -BinaryNames $manifestNames
    & (Join-Path $PSScriptRoot 'build-launcher.ps1') -OutputPath (Join-Path $publicationStage 'Relay.exe') | Out-Null
    $publicationNames=@('Relay.exe','dist\manifest.json')+@($manifestNames | ForEach-Object {'dist\'+$_})
    if(-not $WithoutNativeRtc){$publicationNames+=@('docs\LICENSE-libwebrtc.md','docs\LICENSE-livekit.txt')}
    $publication = @($publicationNames | ForEach-Object {@{Source=(Join-Path $publicationStage $_);Target=(Join-Path $OutputDirectory $_)}})
    Publish-RelayFiles -Files $publication -TransactionDirectory (Join-Path $publicationStage 'publication-rollback')
    # Successful staging has an exact inventory. No recursive cleanup is used.
    foreach($name in $publicationNames){[IO.File]::Delete((Get-RelayDirectPath (Join-Path $publicationStage $name)))}
    [IO.Directory]::Delete((Join-Path $publicationStage 'dist'),$false)
    [IO.Directory]::Delete((Join-Path $publicationStage 'docs'),$false)
    [IO.Directory]::Delete($publicationStage,$false)
    Write-Host "Built Relay, server, private network and launcher; verified output: $OutputDirectory"
} finally {
    $env:PATH = $previousPath
    foreach($name in $cmakeEnvironmentNames){[Environment]::SetEnvironmentVariable($name,$previousCMakeEnvironment[$name],'Process')}
    $env:CARGO_TARGET_DIR = $previousTarget
    $env:CMAKE_POLICY_VERSION_MINIMUM = $previousCMakeMinimum
    $env:RUSTFLAGS = $previousRustFlags
    $env:LK_CUSTOM_WEBRTC = $previousNativeSdk
}
