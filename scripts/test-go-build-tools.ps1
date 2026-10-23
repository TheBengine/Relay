[CmdletBinding()]
param(
    [string]$GoExecutable='C:\Important\Projects\2026-10-03\can\work\embedded-network-20261013\tools\go\bin\go.exe',
    [string]$CMakeExecutable='C:\Important\Projects\2026-10-03\can\work\tools\cmake-4.4.4-windows-x86_64\bin\cmake.exe',
    [string]$ReceiptPath,
    [string]$SupportRoot
)
$ErrorActionPreference='Stop'
$stageRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$canonical=if($SupportRoot){[IO.Path]::GetFullPath($SupportRoot)}else{$stageRoot}
. (Join-Path $canonical 'scripts\shell-runtime.ps1')
. (Join-Path $canonical 'scripts\build-publication.ps1')
if(-not $ReceiptPath){$ReceiptPath=Join-Path $stageRoot 'tool-inputs-verification.json'}
$ReceiptPath=Get-RelayDirectPath $ReceiptPath
if([IO.File]::Exists($ReceiptPath) -or [IO.Directory]::Exists($ReceiptPath)){throw 'Prior verification receipt preserved; choose a new ReceiptPath'}
$fixture=Get-RelayDirectPath (Join-Path ([IO.Path]::GetDirectoryName($ReceiptPath)) ('r11-tools-fixture-'+[Guid]::NewGuid().ToString('N')))
function Assert-True([bool]$Value,[string]$Message){if(-not $Value){throw $Message}}
function Write-TestFile([string]$Path,[string]$Content){[void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($Path));[IO.File]::WriteAllText($Path,$Content,[Text.UTF8Encoding]::new($false))}
$cmakeEnvironmentNames=@('CMAKE','CMAKE_x86_64-pc-windows-msvc','CMAKE_x86_64_pc_windows_msvc','HOST_CMAKE','TARGET_CMAKE')
$environmentNames=@('PATH','CARGO_TARGET_DIR','CMAKE_POLICY_VERSION_MINIMUM','RUSTFLAGS','LK_CUSTOM_WEBRTC','GOOS','GOARCH','CGO_ENABLED','GOTOOLCHAIN','GOMODCACHE','GOCACHE','GOCACHEPROG','GOTMPDIR','TEMP','TMP','GOENV','GOFLAGS','GOWORK','GO111MODULE','GOROOT','GOPROXY','GONOPROXY','GOSUMDB','GOVCS','GOTELEMETRY','RELAY_STUB_GO_LOG','RELAY_STUB_CARGO_LOG','RELAY_STUB_EXPECT_GO','RELAY_STUB_FAIL')+$cmakeEnvironmentNames
$original=@{}
foreach($name in $environmentNames){$original[$name]=[Environment]::GetEnvironmentVariable($name,'Process')}
function Snapshot-Environment {
    $result=@{}
    foreach($name in $environmentNames){$result[$name]=[Environment]::GetEnvironmentVariable($name,'Process')}
    return $result
}
function Assert-Environment($Before){foreach($name in $environmentNames){Assert-True ($Before[$name] -ceq [Environment]::GetEnvironmentVariable($name,'Process')) ('Caller environment changed: '+$name)}}
$checks=[Collections.Generic.List[string]]::new()
try {
    [void][IO.Directory]::CreateDirectory($fixture)
    $fixtureScripts=Join-Path $fixture 'scripts'
    [void][IO.Directory]::CreateDirectory($fixtureScripts)
    foreach($name in @('build.ps1','build-network-helper.ps1','go-build-tools.ps1')){Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination (Join-Path $fixtureScripts $name)}
    foreach($name in @('shell-runtime.ps1','build-publication.ps1')){Copy-Item -LiteralPath (Join-Path $canonical ('scripts\'+$name)) -Destination (Join-Path $fixtureScripts $name)}
    [void][IO.Directory]::CreateDirectory((Join-Path $fixture 'network-helper'))
    Write-TestFile (Join-Path $fixture 'network-helper\go.mod') "module fixture.invalid/network`n`ngo 1.27.1`n"
    Write-TestFile (Join-Path $fixture 'network-helper\go.sum') "fixture sentinel; stub never resolves modules`n"
    $moduleHash=(Get-FileHash -LiteralPath (Join-Path $fixture 'network-helper\go.mod') -Algorithm SHA256).Hash
    $sumHash=(Get-FileHash -LiteralPath (Join-Path $fixture 'network-helper\go.sum') -Algorithm SHA256).Hash
    Write-TestFile (Join-Path $fixtureScripts 'build-iroh-helper.ps1') @'
param([string]$OutputPath,[string]$BuildDirectory)
# This fixture covers tool selection/publication, not another Rust compilation.
[IO.File]::WriteAllBytes($OutputPath,[byte[]](77,90,6))
'@
    Write-TestFile (Join-Path $fixtureScripts 'fetch-sfu.ps1') @'
param([string]$OutputDirectory,[string]$CacheDirectory)
if($CacheDirectory){[IO.File]::WriteAllText((Join-Path $CacheDirectory 'received.txt'),$CacheDirectory)}
[void][IO.Directory]::CreateDirectory((Join-Path $OutputDirectory 'dist'))
[void][IO.Directory]::CreateDirectory((Join-Path $OutputDirectory 'docs'))
[IO.File]::WriteAllBytes((Join-Path $OutputDirectory 'dist\livekit-server.exe'),[byte[]](77,90,4))
[IO.File]::WriteAllText((Join-Path $OutputDirectory 'docs\LICENSE-livekit.txt'),'owned fixture license')
'@
    Write-TestFile (Join-Path $fixtureScripts 'write-binary-manifest.ps1') @'
param([string]$PackageRoot,[string]$SourceRoot,[string[]]$BinaryNames)
[IO.File]::WriteAllText((Join-Path $PackageRoot 'dist\manifest.json'),'{"fixture":true}')
'@
    Write-TestFile (Join-Path $fixtureScripts 'build-launcher.ps1') @'
param([string]$OutputPath)
[IO.File]::WriteAllBytes($OutputPath,[byte[]](77,90,5))
'@
    $toolDirectory=Join-Path $fixture 'tools'
    [void][IO.Directory]::CreateDirectory($toolDirectory)
    $stubSource=Join-Path $fixture 'GoStub.cs'
    Write-TestFile $stubSource @'
using System;
using System.IO;
using System.Collections.Generic;
using System.Diagnostics;
using System.Web.Script.Serialization;
public class GoStub {
  public static int Main(string[] args) {
    string exe=Process.GetCurrentProcess().MainModule.FileName;
    var environment=new Dictionary<string,string>();
    foreach(string name in new string[]{"GOOS","GOARCH","CGO_ENABLED","GOTOOLCHAIN","GOMODCACHE","GOCACHE","GOCACHEPROG","GOTMPDIR","TEMP","TMP","GOENV","GOFLAGS","GOWORK","GO111MODULE","GOROOT","GOPROXY","GONOPROXY","GOSUMDB","GOVCS","GOTELEMETRY","CMAKE","CMAKE_x86_64-pc-windows-msvc","CMAKE_x86_64_pc_windows_msvc","HOST_CMAKE","TARGET_CMAKE"})
      environment[name]=Environment.GetEnvironmentVariable(name);
    bool cargo=String.Equals(Path.GetFileName(exe),"cargo.exe",StringComparison.OrdinalIgnoreCase);
    var record=new Dictionary<string,object>{{"executable",exe},{"args",args},{"cwd",Directory.GetCurrentDirectory()},{"environment",environment},{"go_temporary_directory_existed",cargo?false:Directory.Exists(Environment.GetEnvironmentVariable("GOTMPDIR"))}};
    string log=Environment.GetEnvironmentVariable(cargo?"RELAY_STUB_CARGO_LOG":"RELAY_STUB_GO_LOG");
    File.WriteAllText(log,new JavaScriptSerializer().Serialize(record));
    if(cargo) {
      string output=Path.Combine(Environment.GetEnvironmentVariable("CARGO_TARGET_DIR"),"x86_64-pc-windows-msvc","release");
      Directory.CreateDirectory(output);
      File.WriteAllBytes(Path.Combine(output,"relay.exe"),new byte[]{77,90,1});
      File.WriteAllBytes(Path.Combine(output,"relay-server.exe"),new byte[]{77,90,2});
      return 0;
    }
    if(!String.Equals(exe,Environment.GetEnvironmentVariable("RELAY_STUB_EXPECT_GO"),StringComparison.OrdinalIgnoreCase)) return 71;
    int index=Array.IndexOf(args,"-o");
    if(index<0 || index+1>=args.Length) return 72;
    File.WriteAllBytes(args[index+1],new byte[]{77,90,3});
    return Environment.GetEnvironmentVariable("RELAY_STUB_FAIL")=="1"?7:0;
  }
}
'@
    $compiler=Get-RelayDirectPath 'C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe'
    Assert-True ([IO.File]::Exists($compiler)) 'The local stub compiler is missing'
    $explicitGo=Join-Path $toolDirectory 'configured-go.exe'
    & $compiler /nologo /target:exe /reference:System.Web.Extensions.dll ('/out:'+$explicitGo) $stubSource
    Assert-True ($LASTEXITCODE -eq 0) 'Owned headless Go stub compilation failed'
    Copy-Item -LiteralPath $explicitGo -Destination (Join-Path $toolDirectory 'cargo.exe')
    Copy-Item -LiteralPath $explicitGo -Destination (Join-Path $toolDirectory 'go.exe')
    [IO.File]::WriteAllBytes((Join-Path $toolDirectory 'cmake.exe'),[byte[]](77,90,0))
    $modules=Join-Path $fixture 'explicit-modules';[void][IO.Directory]::CreateDirectory($modules)
    $sfuCache=Join-Path $fixture 'explicit-sfu';[void][IO.Directory]::CreateDirectory($sfuCache)
    $cache=Join-Path $fixture 'explicit-cache'
    $sdk=Join-Path $fixture 'sdk';[void][IO.Directory]::CreateDirectory($sdk)
    Write-TestFile (Join-Path $sdk 'LICENSE.md') 'owned SDK fixture license'
    $env:PATH=$toolDirectory+';'+$original['PATH']
    foreach($name in $cmakeEnvironmentNames){[Environment]::SetEnvironmentVariable($name,('\\blocked.example\unopened\'+$name+'\cmake.exe'),'Process')}
    $env:RUSTFLAGS='-Cdebuginfo=0'
    $env:CARGO_TARGET_DIR='caller-target-sentinel';$env:CMAKE_POLICY_VERSION_MINIMUM='caller-policy-sentinel'
    $env:LK_CUSTOM_WEBRTC='caller-sdk-sentinel'
    foreach($name in @('GOOS','GOARCH','CGO_ENABLED','GOTOOLCHAIN','GOENV','GOFLAGS','GOWORK','GO111MODULE','GOROOT','GOPROXY','GONOPROXY','GOSUMDB','GOVCS','GOTELEMETRY')){[Environment]::SetEnvironmentVariable($name,('caller-'+$name),'Process')}
    $env:GOMODCACHE=Join-Path $fixture 'caller-modules';[void][IO.Directory]::CreateDirectory($env:GOMODCACHE)
    $env:GOCACHE=Join-Path $fixture 'caller-cache'
    $env:GOCACHEPROG='\\blocked.example\unopened\cache-helper.exe'
    $env:GOTMPDIR='\\blocked.example\unopened\go-temp'
    $poisonedTemporary=Join-Path $fixture 'caller-temp-is-a-file'
    Write-TestFile $poisonedTemporary 'must never be used as a temporary directory'
    $env:TEMP=$poisonedTemporary;$env:TMP=$poisonedTemporary
    $env:GOFLAGS='-modfile=\\blocked.example\unopened\go.mod -overlay=\\blocked.example\unopened\overlay.json -toolexec=\\blocked.example\unopened\tool.exe'
    $env:RELAY_STUB_GO_LOG=Join-Path $fixture 'go-success.json'
    $env:RELAY_STUB_CARGO_LOG=Join-Path $fixture 'cargo-success.json'
    $env:RELAY_STUB_EXPECT_GO=$explicitGo;$env:RELAY_STUB_FAIL='0'
    $before=Snapshot-Environment
    $build=Join-Path $fixture 'owned-cargo-target'
    $output=Join-Path $fixture 'owned-output'
    & (Join-Path $fixtureScripts 'build.ps1') -BuildDirectory $build -OutputDirectory $output -CMakeDirectory $toolDirectory -NativeSdkDirectory $sdk -SfuCacheDirectory $sfuCache -GoExecutable $explicitGo -GoModuleCache $modules -GoBuildCache $cache
    Assert-True ([IO.File]::ReadAllText((Join-Path $sfuCache 'received.txt')) -ceq $sfuCache) 'Explicit SFU cache was not forwarded to preparation'
    Assert-Environment $before
    $go=Get-Content -LiteralPath $env:RELAY_STUB_GO_LOG -Raw | ConvertFrom-Json
    $cargo=Get-Content -LiteralPath $env:RELAY_STUB_CARGO_LOG -Raw | ConvertFrom-Json
    Assert-True ($go.executable -ieq $explicitGo) 'Explicit Go must bypass the different go.exe on PATH'
    Assert-True ($go.environment.GOMODCACHE -ceq $modules -and $go.environment.GOCACHE -ceq $cache) 'Explicit caches were not forwarded'
    foreach($pair in @(@('GOOS','windows'),@('GOARCH','amd64'),@('CGO_ENABLED','0'),@('GOTOOLCHAIN','local'),@('GOENV','off'),@('GOWORK','off'),@('GO111MODULE','on'),@('GOPROXY','off'),@('GONOPROXY','none'),@('GOSUMDB','off'),@('GOVCS','*:off'),@('GOTELEMETRY','off'))){Assert-True ($go.environment.($pair[0]) -ceq $pair[1]) ('Incorrect child Go environment: '+$pair[0])}
    Assert-True ([string]::IsNullOrEmpty($go.environment.GOFLAGS) -and [string]::IsNullOrEmpty($go.environment.GOROOT)) 'Inherited Go config/flags were not isolated'
    Assert-True ([string]::IsNullOrEmpty($go.environment.GOCACHEPROG)) 'Inherited external cache program was not cleared'
    $selectedTemporary=Get-RelayDirectPath $go.environment.GOTMPDIR
    Assert-True ($selectedTemporary.StartsWith($cache+'\',[StringComparison]::OrdinalIgnoreCase) -and $go.go_temporary_directory_existed -and $go.environment.TEMP -ceq $selectedTemporary -and $go.environment.TMP -ceq $selectedTemporary) 'Go temporary paths did not use a validated directory inside the chosen cache'
    Assert-True (-not [IO.Directory]::Exists($selectedTemporary)) 'The successful empty owned Go temporary directory was not removed'
    Assert-True ($go.args -contains '-mod=readonly' -and $go.args -contains '-trimpath' -and $go.args -contains '-tags=ts_omit_webclient') 'Go release flags changed'
    Assert-True ($go.cwd -ieq (Join-Path $fixture 'network-helper')) 'Go ran outside the owned module'
    foreach($name in $cmakeEnvironmentNames){Assert-True ($cargo.environment.($name) -ieq (Join-Path $toolDirectory 'cmake.exe')) ('Explicit CMake did not override higher-priority setting: '+$name)}
    foreach($name in @('Relay.exe','dist\relay.exe','dist\relay-server.exe','dist\relay-network.exe','dist\livekit-server.exe','dist\manifest.json')){Assert-True ([IO.File]::Exists((Join-Path $output $name))) ('Expected fixture publication missing: '+$name)}
    $checks.Add('top-level-explicit-tool-and-cache-forwarding-without-real-Cargo-or-Go-build')
    $checks.Add('absolute-CMAKE-and-local-offline-readonly-Go-environment')
    $checks.Add('poisoned-target-HOST-TARGET-CMake-overrides-bound-to-explicit-executable')
    $checks.Add('poisoned-GOCACHEPROG-GOFLAGS-cleared-and-GOTMPDIR-TEMP-TMP-isolated')
    $checks.Add('caller-environment-restored-after-success')

    $previousOutput=(Get-FileHash -LiteralPath (Join-Path $output 'dist\relay-network.exe') -Algorithm SHA256).Hash
    $env:RELAY_STUB_FAIL='1';$env:RELAY_STUB_GO_LOG=Join-Path $fixture 'go-failure.json'
    $before=Snapshot-Environment;$failed=$false
    try { & (Join-Path $fixtureScripts 'build-network-helper.ps1') -OutputPath (Join-Path $output 'dist\relay-network.exe') -GoExecutable $explicitGo -GoModuleCache $modules -GoBuildCache $cache | Out-Null } catch {$failed=$true}
    Assert-True $failed 'A nonzero Go exit must fail the helper'
    Assert-Environment $before
    $failedGo=Get-Content -LiteralPath $env:RELAY_STUB_GO_LOG -Raw | ConvertFrom-Json
    Assert-True ([string]::IsNullOrEmpty($failedGo.environment.GOCACHEPROG) -and [string]::IsNullOrEmpty($failedGo.environment.GOFLAGS) -and $failedGo.environment.GOTMPDIR.StartsWith($cache+'\',[StringComparison]::OrdinalIgnoreCase)) 'Failed Go invocation received poisoned external settings'
    Assert-True ((Get-FileHash -LiteralPath (Join-Path $output 'dist\relay-network.exe') -Algorithm SHA256).Hash -ceq $previousOutput) 'Failed Go build changed the published helper'
    Assert-True (@(Get-ChildItem -LiteralPath (Join-Path $output 'dist') -Filter 'network-build-*.exe').Count -eq 0) 'Failed Go pending file was retained'
    $checks.Add('nonzero-Go-preserves-existing-output-cleans-owned-pending-and-restores-environment')
    $before=Snapshot-Environment;$failed=$false
    try { & (Join-Path $fixtureScripts 'build.ps1') -BuildDirectory (Join-Path $fixture 'failure-target') -OutputDirectory $output -CMakeDirectory $toolDirectory -NativeSdkDirectory $sdk -GoExecutable $explicitGo -GoModuleCache $modules -GoBuildCache $cache } catch {$failed=$true}
    Assert-True $failed 'A Go failure must fail the top-level build'
    Assert-Environment $before
    Assert-True ((Get-FileHash -LiteralPath (Join-Path $output 'dist\relay-network.exe') -Algorithm SHA256).Hash -ceq $previousOutput) 'Failed top-level build changed the prior published helper'
    $checks.Add('top-level-Go-failure-restores-CMake-PATH-and-caller-build-environment')

    $env:RELAY_STUB_FAIL='0';$env:RELAY_STUB_GO_LOG=Join-Path $fixture 'go-configured-caches.json'
    & (Join-Path $fixtureScripts 'build-network-helper.ps1') -OutputPath (Join-Path $output 'dist\configured-caches.exe') -GoExecutable $explicitGo | Out-Null
    $configured=Get-Content -LiteralPath $env:RELAY_STUB_GO_LOG -Raw | ConvertFrom-Json
    Assert-True ($configured.environment.GOMODCACHE -ceq $env:GOMODCACHE -and $configured.environment.GOCACHE -ceq $env:GOCACHE) 'Explicitly configured cache fallback changed'
    $checks.Add('caller-configured-local-cache-fallback')
    . (Join-Path $fixtureScripts 'go-build-tools.ps1')
    $savedPath=$env:PATH
    try {
        $env:PATH='\\blocked.example\share;Z:\blocked;'+$toolDirectory
        $settings=Resolve-RelayGoBuildSettings -GoModuleCache $modules -GoBuildCache $cache
        Assert-True ($settings.Executable -ieq (Join-Path $toolDirectory 'go.exe')) 'Safe default PATH lookup did not select the local configured Go executable'
    } finally {$env:PATH=$savedPath}
    $checks.Add('default-tool-lookup-skips-network-and-other-drive-PATH-text')

    $env:RELAY_STUB_GO_LOG=Join-Path $fixture 'must-not-run.json'
    foreach($case in @(
        @{GoExecutable=$explicitGo;GoModuleCache='\\blocked.example\share\modules';GoBuildCache=$cache},
        @{GoExecutable=$explicitGo;GoModuleCache=(Join-Path $fixture 'missing-module-cache');GoBuildCache=$cache},
        @{GoExecutable=(Join-Path $fixture 'missing-go.exe');GoModuleCache=$modules;GoBuildCache=$cache}
    )){
        $before=Snapshot-Environment;$failed=$false
        try { & (Join-Path $fixtureScripts 'build.ps1') -BuildDirectory (Join-Path $fixture 'must-not-build') -OutputDirectory (Join-Path $fixture 'must-not-publish') -CMakeDirectory $toolDirectory -NativeSdkDirectory $sdk @case } catch {$failed=$true}
        Assert-True $failed 'Invalid tool/cache configuration must fail before a build'
        Assert-Environment $before
        Assert-True (-not [IO.Directory]::Exists((Join-Path $fixture 'must-not-build')) -and -not [IO.File]::Exists($env:RELAY_STUB_GO_LOG)) 'Invalid configuration reached a build or helper'
    }
    $checks.Add('missing-and-network-tool-cache-inputs-fail-before-Cargo-or-Go')
    Assert-True ((Get-FileHash -LiteralPath (Join-Path $fixture 'network-helper\go.mod') -Algorithm SHA256).Hash -ceq $moduleHash -and (Get-FileHash -LiteralPath (Join-Path $fixture 'network-helper\go.sum') -Algorithm SHA256).Hash -ceq $sumHash) 'Fixture module records changed'

    # Actual tools are queried for metadata only: no build, install or cache fill.
    $actualGo=Get-RelayDirectPath $GoExecutable
    $actualCMake=Get-RelayDirectPath $CMakeExecutable
    $metadataTemporary=Join-Path $fixture 'metadata-temp';[void][IO.Directory]::CreateDirectory($metadataTemporary)
    $env:GOENV='off';$env:GOTOOLCHAIN='local';$env:GOTELEMETRY='off';$env:GOFLAGS='';$env:GOTMPDIR=$metadataTemporary;$env:TEMP=$metadataTemporary;$env:TMP=$metadataTemporary
    [Environment]::SetEnvironmentVariable('GOROOT',$null,'Process');[Environment]::SetEnvironmentVariable('GOCACHEPROG',$null,'Process')
    $goVersion=(& $actualGo version | Out-String).Trim();Assert-True ($LASTEXITCODE -eq 0) 'Actual Go version query failed'
    $cmakeLines=@(& $actualCMake --version);Assert-True ($LASTEXITCODE -eq 0 -and $cmakeLines.Count -gt 0) 'Actual CMake version query failed'
    $cmakeVersion=$cmakeLines[0]
    $checks.Add('actual-local-Go-and-CMake-version-metadata-only')
    $receipt=[ordered]@{schema=1;passed=$true;checks=@($checks);fixture=$fixture;actual_go_version=$goVersion;actual_cmake_version=$cmakeVersion;actual_go_sha256=(Get-FileHash -LiteralPath $actualGo -Algorithm SHA256).Hash.ToLowerInvariant();actual_cmake_sha256=(Get-FileHash -LiteralPath $actualCMake -Algorithm SHA256).Hash.ToLowerInvariant();full_application_build_run=$false;actual_Go_build_run=$false;canonical_modified=$false;gui_or_network_actions=$false}
    $receipt.tested_script_sha256=[ordered]@{}
    foreach($name in @('build.ps1','build-network-helper.ps1','go-build-tools.ps1','test-go-build-tools.ps1')){$receipt.tested_script_sha256[$name]=(Get-FileHash -LiteralPath (Join-Path $PSScriptRoot $name) -Algorithm SHA256).Hash.ToLowerInvariant()}
    [IO.File]::WriteAllText($ReceiptPath,($receipt | ConvertTo-Json -Depth 5)+[Environment]::NewLine,[Text.UTF8Encoding]::new($false))
    Write-Output ($receipt | ConvertTo-Json -Depth 5)
} finally {
    foreach($name in $environmentNames){[Environment]::SetEnvironmentVariable($name,$original[$name],'Process')}
}
