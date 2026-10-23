[CmdletBinding()]
param([Parameter(Mandatory)][string]$DataDirectory)
$ErrorActionPreference='Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
. ([IO.Path]::Combine($PSScriptRoot, 'hosting-paths.ps1'))
$dataRoot=ConvertTo-RelayShellPath -Path $DataDirectory
if($dataRoot -notmatch '^[CDcd]:\\'){throw 'Hosting files must stay on the local C: or D: drive.'}
function Assert-NoHostingRedirect([string]$Path){
    $inspect=[IO.Path]::GetFullPath($Path)
    if($inspect -notmatch '^[CDcd]:\\'){throw 'Hosting files must stay on the local C: or D: drive.'}
    if(([IO.DriveInfo]::new([IO.Path]::GetPathRoot($inspect))).DriveType -eq [IO.DriveType]::Network){throw 'Hosting files must stay on local storage.'}
    $ancestors=[Collections.Generic.List[string]]::new()
    while($inspect){
        $ancestors.Add($inspect)
        $parent=[IO.Path]::GetDirectoryName($inspect.TrimEnd('\'))
        if(-not $parent -or $parent -eq $inspect){break}
        $inspect=$parent
    }
    $ancestors.Reverse()
    foreach($ancestor in $ancestors){
        $item=Get-Item -LiteralPath $ancestor -Force -ErrorAction SilentlyContinue
        if($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0){throw 'Hosting paths cannot traverse redirects.'}
    }
}
$recordPath=Join-Path $dataRoot 'stack-instance.json'
Assert-NoHostingRedirect $recordPath
$record=Get-Content -LiteralPath $recordPath -Raw | ConvertFrom-Json
function Get-OwnedStackProcess([int]$ProcessId,[string]$Executable,[string]$StartedAt){
    if(-not $ProcessId -or -not $Executable -or -not $StartedAt){return $null}
    $process=Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if(-not $process){return $null}
    try{
        if($process.Path -eq $Executable -and $process.StartTime.ToUniversalTime() -eq ([DateTime]::Parse($StartedAt).ToUniversalTime())){return $process}
    }catch{}
    return $null
}
function Stop-OwnedStackProcess([int]$ProcessId,[string]$Executable,[string]$StartedAt){
    $owned=Get-OwnedStackProcess $ProcessId $Executable $StartedAt
    if($owned){Stop-Process -Id $owned.Id -ErrorAction Stop}
}
Assert-NoHostingRedirect $record.relay_exe
Assert-NoHostingRedirect $record.sfu_exe
if($record.public_mode){Assert-NoHostingRedirect $record.caddy_exe}
$shutdownRequested=$false
$shutdownDeadline=$null
while($true){
    $relay=Get-OwnedStackProcess $record.relay_pid $record.relay_exe $record.relay_started_at
    if(-not $relay){break}
    $video=Get-OwnedStackProcess $record.sfu_pid $record.sfu_exe $record.sfu_started_at
    $gateway=if($record.public_mode){Get-OwnedStackProcess $record.caddy_pid $record.caddy_exe $record.caddy_started_at}else{$true}
    if((-not $video -or -not $gateway) -and -not $shutdownRequested){
        # If media or public HTTPS fails, stop the complete stack. Graceful
        # control verifies its private instance token. Fallback also checks
        # exact executable and creation time before stopping the process.
        $shutdownRequested=$true
        $shutdownDeadline=[DateTime]::UtcNow.AddSeconds(15)
        try{
            & $record.relay_exe --data $dataRoot --stop
            if($LASTEXITCODE -ne 0){throw 'Owned child failed and graceful Relay shutdown did not finish.'}
        }catch{
            Write-Warning 'Graceful Relay stop failed; stopping only its exact tracked process.'
            Stop-OwnedStackProcess $record.relay_pid $record.relay_exe $record.relay_started_at
        }
    }
    if($shutdownRequested -and [DateTime]::UtcNow -gt $shutdownDeadline){
        Stop-OwnedStackProcess $record.relay_pid $record.relay_exe $record.relay_started_at
    }
    Start-Sleep -Seconds 1
}
# Normal stop, media failure and HTTPS failure all remove only this stack's
# exact children. A reused PID or different executable is left alone.
foreach($child in @(
    @{ProcessId=$record.sfu_pid;Executable=$record.sfu_exe;StartedAt=$record.sfu_started_at},
    @{ProcessId=$record.caddy_pid;Executable=$record.caddy_exe;StartedAt=$record.caddy_started_at}
)){
    try{Stop-OwnedStackProcess $child.ProcessId $child.Executable $child.StartedAt}catch{Write-Warning 'Could not stop an owned stack child; its tracking records are preserved.'}
}
$videoRemaining=Get-OwnedStackProcess $record.sfu_pid $record.sfu_exe $record.sfu_started_at
$gatewayRemaining=Get-OwnedStackProcess $record.caddy_pid $record.caddy_exe $record.caddy_started_at
if($videoRemaining -or $gatewayRemaining){throw 'Owned child shutdown was incomplete; process records were kept for recovery.'}
# A new launcher may already have written a new stack; never remove its records.
foreach($path in @($recordPath,(Join-Path $dataRoot 'sfu-instance.json'),(Join-Path $dataRoot 'caddy-instance.json'),(Join-Path $dataRoot 'stack-supervisor.json'))){
    Assert-NoHostingRedirect $path
    if(Test-Path -LiteralPath $path){
        $current=Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
        $sameStack=if($record.stack_id){$current.stack_id -eq $record.stack_id}else{($current.relay_pid -eq $record.relay_pid) -or ($current.pid -eq $record.sfu_pid)}
        if($sameStack){Remove-Item -LiteralPath $path}
    }
}
