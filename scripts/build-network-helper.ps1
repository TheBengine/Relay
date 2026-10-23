[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$OutputPath,
    [string]$GoExecutable,
    [string]$GoModuleCache,
    [string]$GoBuildCache
)
$ErrorActionPreference='Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
. ([IO.Path]::Combine($PSScriptRoot, 'build-publication.ps1'))
. ([IO.Path]::Combine($PSScriptRoot, 'go-build-tools.ps1'))
$packageRoot=Get-RelayDirectPath (Join-Path $PSScriptRoot '..')
$output=Get-RelayDirectPath $OutputPath
$goSettings=Resolve-RelayGoBuildSettings -GoExecutable $GoExecutable -GoModuleCache $GoModuleCache -GoBuildCache $GoBuildCache
[void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($output))
$pending=Join-Path ([IO.Path]::GetDirectoryName($output)) ('network-build-'+[Guid]::NewGuid().ToString('N')+'.exe')
$temporary=Get-RelayDirectPath (Join-Path $goSettings.BuildCache ('relay-go-tmp-'+[Guid]::NewGuid().ToString('N')))
$environmentNames=@('GOOS','GOARCH','CGO_ENABLED','GOTOOLCHAIN','GOMODCACHE','GOCACHE','GOCACHEPROG','GOTMPDIR','TEMP','TMP','GOENV','GOFLAGS','GOWORK','GO111MODULE','GOROOT','GOPROXY','GONOPROXY','GOSUMDB','GOVCS','GOTELEMETRY')
$previousEnvironment=@{}
foreach($name in $environmentNames){$previousEnvironment[$name]=[Environment]::GetEnvironmentVariable($name,'Process')}
try {
    $env:GOOS='windows'; $env:GOARCH='amd64'; $env:CGO_ENABLED='0'
    # Use only this compiler and the caller's prepared, guarded local caches.
    # Ignore hidden Go settings/workspaces and inherited flag overrides.
    $env:GOTOOLCHAIN='local'
    $env:GOMODCACHE=$goSettings.ModuleCache; $env:GOCACHE=$goSettings.BuildCache
    [Environment]::SetEnvironmentVariable('GOCACHEPROG',$null,'Process')
    [void][IO.Directory]::CreateDirectory($temporary)
    [void](Get-RelayDirectPath $temporary)
    $env:GOTMPDIR=$temporary; $env:TEMP=$temporary; $env:TMP=$temporary
    $env:GOENV='off'; $env:GOFLAGS=''; $env:GOWORK='off'; $env:GO111MODULE='on'
    [Environment]::SetEnvironmentVariable('GOROOT',$null,'Process')
    # Missing modules/checksums must fail without a toolchain, proxy or VCS fetch.
    $env:GOPROXY='off'; $env:GONOPROXY='none'; $env:GOSUMDB='off'; $env:GOVCS='*:off'
    $env:GOTELEMETRY='off'
    Push-Location (Get-RelayDirectPath (Join-Path $packageRoot 'network-helper'))
    try {
        & $goSettings.Executable build -mod=readonly -trimpath -tags=ts_omit_webclient -ldflags '-s -w' -o $pending .
        if ($LASTEXITCODE -ne 0) { throw 'Private-network build failed. Use the Go version in network-helper/go.mod and prepare the explicit dependency cache; downloads are disabled.' }
    } finally { Pop-Location }
    Publish-RelayFiles -Files @(@{Source=$pending;Target=$output}) -TransactionDirectory ($pending+'.transaction')
} finally {
    foreach($name in $environmentNames){[Environment]::SetEnvironmentVariable($name,$previousEnvironment[$name],'Process')}
    if ([IO.File]::Exists($pending)) { [void](Get-RelayDirectPath $pending); [IO.File]::Delete($pending) }
    # Remove only this attempt's empty temp directory. Preserve nonempty failed
    # work for inspection; never recursively delete an inherited/computed tree.
    if ([IO.Directory]::Exists($temporary)) {
        [void](Get-RelayDirectPath $temporary)
        try { [IO.Directory]::Delete($temporary,$false) } catch [IO.IOException] { }
    }
}
Write-Output $output
