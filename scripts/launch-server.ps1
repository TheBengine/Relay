[CmdletBinding()]
param(
    [string]$BindAddress = '127.0.0.1',
    [ValidateRange(1,65535)][int]$Port = 4580,
    [ValidateRange(1,65535)][int]$UdpPort = 4581,
    [string]$ServerName = 'Relay House',
    [string]$DataDirectory,
    [switch]$PlainChatOnly
)
$ErrorActionPreference='Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
$packageRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$serverExe=Join-Path $packageRoot 'dist\relay-server.exe'
. (Join-Path $PSScriptRoot 'hosting-paths.ps1')
$dataRoot=Resolve-RelayDataDirectory -DataDirectory $DataDirectory -PackageRoot $packageRoot -CheckLegacy
if(-not $PlainChatOnly){
    if($BindAddress -ne '127.0.0.1'){throw 'For LAN/public native video, use the configured SFU recipe in docs/hosting.md. Plain chat/UDP audio is available with -PlainChatOnly.'}
    & (Join-Path $PSScriptRoot 'launch-stack.ps1') -DataDirectory $dataRoot -Port $Port -UdpPort $UdpPort -ServerName $ServerName
    exit
}
$logRoot=Join-Path $dataRoot 'logs'
$record=Join-Path $dataRoot 'managed-instance.json'
if(-not(Test-Path -LiteralPath $serverExe -PathType Leaf)){throw 'Server binary is missing. Build the package first.'}
[void][Net.IPAddress]::Parse($BindAddress)
if($ServerName.Contains('"') -or $dataRoot.Contains('"')){throw 'Names and paths cannot contain a double quote.'}
if(Test-Path -LiteralPath $record){
    try {
        $previous=Get-Content -LiteralPath $record -Raw | ConvertFrom-Json
        $health=Invoke-RestMethod -Uri ($previous.url+'/api/health') -TimeoutSec 2
        if($health.instance_id -eq $previous.instance_id -and $health.server_id -eq $previous.server_id -and $health.pid -eq $previous.pid){Write-Host "Relay is already running: $($previous.url)";exit 0}
    } catch {}
}
New-Item -ItemType Directory -Path $dataRoot,$logRoot -Force | Out-Null
# The runtime management token stays in a per-user directory. Preserve SYSTEM and
# the current user's full control, and remove inherited grants to other accounts.
$directory=[IO.DirectoryInfo]::new($dataRoot)
$acl=if($PSVersionTable.PSVersion.Major -ge 6){[IO.FileSystemAclExtensions]::GetAccessControl($directory)}else{$directory.GetAccessControl()}
$acl.SetAccessRuleProtection($true,$false)
foreach($identity in @([Security.Principal.WindowsIdentity]::GetCurrent().User, [Security.Principal.SecurityIdentifier]::new('S-1-5-18'))){
    $rule=[Security.AccessControl.FileSystemAccessRule]::new($identity,'FullControl','ContainerInherit,ObjectInherit','None','Allow')
    $acl.SetAccessRule($rule)
}
if($PSVersionTable.PSVersion.Major -ge 6){[IO.FileSystemAclExtensions]::SetAccessControl($directory,$acl)}else{$directory.SetAccessControl($acl)}
$arguments='--bind {0} --port {1} --udp-port {2} --data "{3}" --name "{4}"' -f $BindAddress,$Port,$UdpPort,$dataRoot,$ServerName
$process=Start-Process -FilePath $serverExe -ArgumentList $arguments -WorkingDirectory $packageRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $logRoot 'server.stdout.log') -RedirectStandardError (Join-Path $logRoot 'server.stderr.log')
$healthAddress=if($BindAddress -eq '0.0.0.0'){'127.0.0.1'}elseif($BindAddress -eq '::'){'[::1]'}elseif($BindAddress.Contains(':')){"[$BindAddress]"}else{$BindAddress}
$url="http://${healthAddress}:$Port"
$ready=$false
for($attempt=0;$attempt -lt 50;$attempt++){
    $process.Refresh()
    if($process.HasExited){throw ('Relay exited: '+(Get-Content -LiteralPath (Join-Path $logRoot 'server.stderr.log') -Raw))}
    try {
        $health=Invoke-RestMethod -Uri "$url/api/health" -TimeoutSec 1
        if($health.ok -and $health.pid -eq $process.Id -and $health.instance_id -and $health.server_id){$ready=$true;break}
    }catch{}
    Start-Sleep -Milliseconds 200
}
if(-not $ready){throw "Readiness failed; check $logRoot"}
Write-Host "Relay ready at $url (instance $($health.instance_id))."
Write-Host "Data: $dataRoot"
Write-Host 'Stop with scripts\stop-server.ps1 using the same -DataDirectory.'
