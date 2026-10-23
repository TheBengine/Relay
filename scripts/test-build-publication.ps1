[CmdletBinding()]
param([Parameter(Mandatory=$true)][string]$WorkDirectory)
$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot 'build-publication.ps1')
$work=Get-RelayDirectPath $WorkDirectory
$root=Join-Path $work ('publication-fixture-'+[Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($root)
$source=Join-Path $root 'source'; $target=Join-Path $root 'target'
[void][IO.Directory]::CreateDirectory($source); [void][IO.Directory]::CreateDirectory($target)
function Assert-Equal($Actual,$Expected,$Message){if($Actual -cne $Expected){throw $Message}}
function Put([string]$Path,[string]$Value){[IO.File]::WriteAllText($Path,$Value,[Text.UTF8Encoding]::new($false))}
function Read([string]$Path){return [IO.File]::ReadAllText($Path)}
foreach($name in @('relay.exe','relay-server.exe','manifest.json')){Put (Join-Path $source $name) ('new-'+$name); Put (Join-Path $target $name) ('old-longer-content-'+$name)}
$files=@('relay.exe','relay-server.exe','manifest.json' | ForEach-Object {@{Source=(Join-Path $source $_);Target=(Join-Path $target $_)}})
$held=[IO.File]::Open((Join-Path $target 'relay-server.exe'),[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
try{
    $failed=$false
    try{Publish-RelayFiles -Files $files -TransactionDirectory (Join-Path $root 'busy-second')}catch{$failed=$true;if($_.Exception.Message -notlike '*Close Relay*'){throw}}
    if(-not $failed){throw 'Locked second destination must fail.'}
    foreach($name in @('relay.exe','relay-server.exe','manifest.json')){Assert-Equal (Read (Join-Path $target $name)) ('old-longer-content-'+$name) 'Busy preflight changed an existing output.'}
    $absent=Join-Path $target 'not-created.exe'
    $failed=$false
    try{Publish-RelayFiles -Files @(@{Source=$files[0].Source;Target=$absent},$files[1]) -TransactionDirectory (Join-Path $root 'busy-absent')}catch{$failed=$true}
    if(-not $failed -or [IO.File]::Exists($absent)){throw 'Locked later output created an earlier absent output.'}
}finally{$held.Dispose()}
$held=[IO.File]::Open((Join-Path $target 'manifest.json'),[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
try{
    $failed=$false
    try{Publish-RelayFiles -Files $files -TransactionDirectory (Join-Path $root 'busy-manifest')}catch{$failed=$true}
    if(-not $failed){throw 'A busy manifest must fail the whole preflight.'}
    Assert-Equal (Read $files[0].Target) 'old-longer-content-relay.exe' 'Busy manifest left a newer client.'
}finally{$held.Dispose()}
Publish-RelayFiles -Files $files -TransactionDirectory (Join-Path $root 'successful-commit')
foreach($file in $files){Assert-Equal (Read $file.Target) (Read $file.Source) 'Successful commit must replace and truncate every output.'}
if([IO.Directory]::Exists((Join-Path $root 'successful-commit'))){throw 'Successful publication retained backups.'}
$failed=$false
try{Publish-RelayFiles -Files @($files[0],$files[0]) -TransactionDirectory (Join-Path $root 'duplicate')}catch{$failed=$true}
if(-not $failed){throw 'Duplicate destination must be refused.'}
Assert-Equal (Read $files[0].Target) (Read $files[0].Source) 'Rejected duplicate changed the destination.'
foreach($flags in @('-Ctarget-feature=+crt-static','-C target-feature=+crt-static',' -Ctarget-feature=+sse2,+crt-static ')){
    Assert-Equal (Get-RelayStaticRuntimeFlags $flags) $flags 'An existing equivalent static runtime flag changed.'
}
Assert-Equal (Get-RelayStaticRuntimeFlags '') '-Ctarget-feature=+crt-static' 'Missing required static runtime flag.'
$failed=$false;try{Get-RelayStaticRuntimeFlags '-Ctarget-feature=-crt-static'|Out-Null}catch{$failed=$true};if(-not $failed){throw 'Conflicting static runtime flag must fail.'}
# These inputs are rejected as strings, before touching prohibited storage.
foreach($forbidden in @('E:\do-not-open','\\server\share','\\?\UNC\server\share')){
    $failed=$false;try{Get-RelayDirectPath $forbidden|Out-Null}catch{$failed=$true};if(-not $failed){throw 'A prohibited path was accepted.'}
}
Write-Host 'PASS: locked second/manifest leaves all bytes unchanged; absent output stays absent; successful commit truncates correctly; duplicate/prohibited paths rejected; equivalent RUSTFLAGS preserved. No compiler, program, profile or network used.'
