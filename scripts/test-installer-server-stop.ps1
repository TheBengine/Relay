[CmdletBinding()]
param([Parameter(Mandatory)][string]$WorkDirectory)
$ErrorActionPreference='Stop'
. (Join-Path $PSScriptRoot '..\deploy\windows\StopLocalServer.ps1') -FunctionsOnly
$work=Get-SetupDirectPath $WorkDirectory
$root=Join-Path $work ('stop-records-'+[guid]::NewGuid().ToString('N'))
$null=New-Item -ItemType Directory -Path $root
$exe=Join-Path $root 'relay-server.exe'
[IO.File]::WriteAllText($exe,'fixture')
$data=Join-Path $root 'data with spaces'
$null=New-Item -ItemType Directory -Path $data
$process=[pscustomobject]@{ProcessId=1234;ExecutablePath=$exe;CommandLine='server --data "'+$data+'"'}
$record=[pscustomobject]@{pid=1234;exe=$exe;data=$data;server_id=[guid]::NewGuid().ToString();instance_id=[guid]::NewGuid().ToString()}
$checks=0
function Assert-Stop($Value,[string]$Reason){if(-not $Value){throw $Reason};$script:checks++}
function Refused([scriptblock]$Action){try{& $Action | Out-Null;return $false}catch{return $true}}
Assert-Stop ((Get-SetupServerData $process.CommandLine) -eq $data) 'Quoted data path failed.'
Assert-Stop ((Get-SetupServerData ('server --data="'+$data+'"')) -eq $data) 'Equals data path failed.'
Assert-Stop (Refused {Get-SetupServerData ('server --data "'+$data+'" --data "'+$data+'"')}) 'Duplicate data accepted.'
Assert-Stop (Refused {Get-SetupServerData 'server'}) 'Missing data accepted.'
Assert-Stop (Test-SetupServerIdentity $process $record $record $data) 'Valid identity refused.'
foreach($field in @('pid','exe','data','server_id','instance_id')){
    $changed=$record | Select-Object *
    switch($field){
        'pid' {$changed.pid=1235}
        'exe' {$changed.exe=$data}
        'data' {$changed.data=$root}
        'server_id' {$changed.server_id=[guid]::NewGuid().ToString()}
        'instance_id' {$changed.instance_id=[guid]::NewGuid().ToString()}
    }
    Assert-Stop (Refused {Test-SetupServerIdentity $process $record $changed $data}) ('Changed '+$field+' accepted.')
}
foreach($name in @('server-runtime.json','managed-instance.json')){
    $record | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $data $name) -Encoding UTF8
}
$other=[pscustomobject]@{ProcessId=4567;ExecutablePath=(Join-Path $root 'other\relay-server.exe');CommandLine='server --data INVALID'}
$plans=@(Get-SetupServerPlans $root @($process,$other))
Assert-Stop ($plans.Count -eq 1 -and $plans[0].Process.ProcessId -eq 1234) 'Another installation was inspected.'
[IO.File]::WriteAllText((Join-Path $data 'managed-instance.json'),'{invalid')
Assert-Stop (Refused {Get-SetupServerPlans $root @($process)}) 'Malformed identity accepted.'
Write-Output "Installer stop identity checks: $checks passed."
