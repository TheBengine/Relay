[CmdletBinding()]
param(
    [string]$DataDirectory,
    [ValidateRange(1,65535)][int]$Port=4580,
    [ValidateRange(1,65535)][int]$UdpPort=4581,
    [ValidateRange(1,65535)][int]$SfuPort=7880,
    [ValidateRange(1,65535)][int]$SfuTcpPort=7881,
    [ValidateRange(1,65535)][int]$SfuUdpPort=7882,
    [string]$ServerName='Relay House',
    [string]$ServerBinary,
    [string]$SfuBinary,
    [string]$PublicIPv4,
    [string]$LocalIPv4,
    [string]$RouterWanIPv4,
    [string]$CaddyBinary,
    [switch]$AcceptedCertificateTerms,
    [string]$TailscaleIPv4,
    [string]$EmbeddedIPv4,
    [switch]$LanDiscovery,
    [ValidateRange(1,65535)][int]$NetworkPeerPort=4582
)
$ErrorActionPreference='Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
. ([IO.Path]::Combine($PSScriptRoot, 'public-host-config.ps1'))
# Every public preflight check is read-only and happens before directories,
# configuration, processes, ACME accounts or certificates can be created.
$publicMode=@('PublicIPv4','LocalIPv4','RouterWanIPv4','CaddyBinary','AcceptedCertificateTerms' | Where-Object {$PSBoundParameters.ContainsKey($_)}).Count -gt 0
$tailscaleMode=$PSBoundParameters.ContainsKey('TailscaleIPv4')
$embeddedMode=$PSBoundParameters.ContainsKey('EmbeddedIPv4')
if($embeddedMode -and ($publicMode -or $tailscaleMode)){throw 'Choose one hosting connection at a time.'}
if($LanDiscovery -and ($publicMode -or $tailscaleMode)){throw 'Local code discovery needs a local or built-in private host. Use the existing HTTPS or Tailscale invitation for this connection.'}
if($PSBoundParameters.ContainsKey('NetworkPeerPort') -and -not $embeddedMode){throw 'NetworkPeerPort requires built-in private hosting.'}
if($embeddedMode){
    $embeddedOctets=ConvertTo-RelayStrictIPv4 -Address $EmbeddedIPv4
    if($embeddedOctets[0] -ne 100 -or $embeddedOctets[1] -lt 64 -or $embeddedOctets[1] -gt 127){throw 'The built-in private connection needs its own private IPv4 address.'}
    if($NetworkPeerPort -in @($Port,$SfuPort,$SfuTcpPort)){throw 'The private ingress needs a separate loopback port.'}
}
if($publicMode -and $tailscaleMode){throw 'Choose either public hosting or private Tailscale hosting, not both.'}
function New-RelayTailscaleHostingPlan {
    param([Parameter(Mandatory=$true)][string]$Address,[Parameter(Mandatory=$true)][object]$Status,[ValidateRange(1,65535)][int]$Port=4580)
    $octets=ConvertTo-RelayStrictIPv4 -Address $Address
    if($octets[0] -ne 100 -or $octets[1] -lt 64 -or $octets[1] -gt 127){throw 'Use this PC''s Tailscale IPv4 address in 100.64.0.0/10.'}
    if($Status.BackendState -cne 'Running'){throw 'Tailscale is not connected. Sign in and connect Tailscale before hosting.'}
    # Only the local node's top-level addresses count. Never accept a peer's
    # address or trust an address merely because it lies within 100.64/10.
    if(@($Status.TailscaleIPs | Where-Object {$_ -ceq $Address}).Count -ne 1){throw 'The selected Tailscale address does not belong to this connected PC.'}
    return [pscustomobject]@{
        Address=$Address
        RelayUrl="http://${Address}:$Port"
        RtcUrl="ws://${Address}:$Port"
        MediaIpIncludes=@("$Address/32",'127.0.0.0/8')
        RequiresCertificate=$false
        RequiresRouterForwarding=$false
    }
}
$tailscalePlan=$null
if($tailscaleMode){
    # Reject invalid input before looking for or invoking any local program.
    $tailscaleOctets=ConvertTo-RelayStrictIPv4 -Address $TailscaleIPv4
    if($tailscaleOctets[0] -ne 100 -or $tailscaleOctets[1] -lt 64 -or $tailscaleOctets[1] -gt 127){throw 'Use this PC''s Tailscale IPv4 address in 100.64.0.0/10.'}
}
$publicPlan=$null
if($publicMode){
    if(-not $PublicIPv4 -or -not $LocalIPv4 -or -not $RouterWanIPv4){throw 'Public hosting requires PublicIPv4, LocalIPv4 and the router''s actual RouterWanIPv4.'}
    $publicPlan=New-RelayPublicHostingPlan -PublicIPv4 $PublicIPv4 -LocalIPv4 $LocalIPv4 -AcceptedCertificateTerms:$AcceptedCertificateTerms
    # A discovery service can report the carrier's public address while the
    # router itself is behind CGNAT. Both addresses must be globally routable
    # and equal; never attempt certificate issuance for the carrier gateway.
    $wanPlan=New-RelayPublicHostingPlan -PublicIPv4 $RouterWanIPv4 -LocalIPv4 $LocalIPv4
    if($wanPlan.PublicAddress -cne $publicPlan.PublicAddress){throw 'Router WAN IPv4 differs from the public IPv4. Ask the ISP for a directly reachable public IPv4 before enabling public hosting.'}
    Assert-RelayPublicCertificateTerms -AcceptedCertificateTerms:$AcceptedCertificateTerms
    if($Port -ne 4580 -or $UdpPort -ne 4581 -or $SfuPort -ne 7880 -or $SfuTcpPort -ne 7881 -or $SfuUdpPort -ne 7882){throw 'Public hosting currently requires the fixed documented Relay/SFU ports. Custom ports remain available for local hosting.'}
}
function Assert-RelayHostingLocalPath([string]$Path){
    $full=[IO.Path]::GetFullPath($Path)
    if($full -notmatch '^[CDcd]:\\' -or $full.StartsWith('\\')){throw 'Hosting files must stay on the local C: or D: drive.'}
    if(([IO.DriveInfo]::new([IO.Path]::GetPathRoot($full))).DriveType -eq [IO.DriveType]::Network){throw 'Hosting files must stay on local storage.'}
    $ancestors=[Collections.Generic.List[string]]::new()
    $inspect=$full
    while($inspect){
        $ancestors.Add($inspect)
        $parent=[IO.Path]::GetDirectoryName($inspect.TrimEnd('\'))
        if(-not $parent -or $parent -eq $inspect){break}
        $inspect=$parent
    }
    $ancestors.Reverse()
    foreach($ancestor in $ancestors){
        $item=Get-Item -LiteralPath $ancestor -Force -ErrorAction SilentlyContinue
        if($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0){throw 'Hosting paths cannot traverse a redirecting folder or file.'}
    }
    return $full
}
function Read-RelayBoundedProcessOutput {
    param([Parameter(Mandatory=$true)][Diagnostics.Process]$Process)
    # Pump both pipes concurrently with fixed buffers. Neither output can grow
    # beyond 65 KiB, and a full stderr pipe cannot block stdout or vice versa.
    $limit=65*1024
    $deadline=[DateTime]::UtcNow.AddSeconds(10)
    $readers=@(
        @{Stream=$Process.StandardOutput.BaseStream;Buffer=[byte[]]::new(4096);Bytes=[IO.MemoryStream]::new();Pending=$null;Ended=$false},
        @{Stream=$Process.StandardError.BaseStream;Buffer=[byte[]]::new(4096);Bytes=[IO.MemoryStream]::new();Pending=$null;Ended=$false}
    )
    try{
        foreach($reader in $readers){$reader.Pending=$reader.Stream.ReadAsync($reader.Buffer,0,$reader.Buffer.Length)}
        while($true){
            foreach($reader in $readers){
                if(-not $reader.Ended -and $reader.Pending.IsCompleted){
                    $count=$reader.Pending.GetAwaiter().GetResult()
                    if($count -eq 0){$reader.Ended=$true;$reader.Pending=$null}
                    else{
                        if($reader.Bytes.Length+$count -gt $limit){throw 'Tailscale status exceeded its 65 KiB output limit. Open Tailscale and try again.'}
                        $reader.Bytes.Write($reader.Buffer,0,$count)
                        $reader.Pending=$reader.Stream.ReadAsync($reader.Buffer,0,$reader.Buffer.Length)
                    }
                }
            }
            if($Process.HasExited -and $readers[0].Ended -and $readers[1].Ended){break}
            if([DateTime]::UtcNow -ge $deadline){throw 'Tailscale status did not respond within 10 seconds. Open Tailscale and try again.'}
            Start-Sleep -Milliseconds 10
        }
        return [pscustomobject]@{
            Stdout=[Text.Encoding]::UTF8.GetString($readers[0].Bytes.ToArray())
            Stderr=[Text.Encoding]::UTF8.GetString($readers[1].Bytes.ToArray())
        }
    }finally{foreach($reader in $readers){$reader.Bytes.Dispose()}}
}
function Read-RelayTailscaleHostingStatus {
    # Deliberately fixed installation path: no PATH lookup, portable executable,
    # login command, settings change, firewall command or account creation.
    $binary='C:\Program Files\Tailscale\tailscale.exe'
    $null=Assert-RelayHostingLocalPath $binary
    if(-not(Test-Path -LiteralPath $binary -PathType Leaf)){throw 'Install Tailscale on this PC and connect it before using private hosting.'}
    $start=[Diagnostics.ProcessStartInfo]::new()
    $start.FileName=$binary
    $start.Arguments='status --json --peers=false'
    $start.UseShellExecute=$false
    $start.CreateNoWindow=$true
    $start.RedirectStandardOutput=$true
    $start.RedirectStandardError=$true
    $query=[Diagnostics.Process]::new()
    $query.StartInfo=$start
    $queryStarted=$false
    try{
        if(-not $query.Start()){throw 'Could not read Tailscale connection status.'}
        $queryStarted=$true
        $output=Read-RelayBoundedProcessOutput -Process $query
        if($query.ExitCode -ne 0){throw 'Tailscale is unavailable. Open Tailscale, sign in and connect it before hosting.'}
        return $output.Stdout | ConvertFrom-Json
    }finally{
        # This exact Process object is the read-only child just created. Stop it
        # on timeout/output-limit failure without touching the Tailscale daemon.
        try{if($queryStarted -and -not $query.HasExited){$query.Kill();$null=$query.WaitForExit(2000)}}finally{$query.Dispose()}
    }
}
if($tailscaleMode){
    $tailscaleStatus=Read-RelayTailscaleHostingStatus
    $tailscalePlan=New-RelayTailscaleHostingPlan -Address $TailscaleIPv4 -Status $tailscaleStatus -Port $Port
}
$packageRoot=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
. (Join-Path $PSScriptRoot 'hosting-paths.ps1')
if($DataDirectory){$null=Assert-RelayHostingLocalPath (ConvertTo-RelayShellPath -Path $DataDirectory)}
elseif($env:RELAY_SERVER_DATA_DIR){$null=Assert-RelayHostingLocalPath (ConvertTo-RelayShellPath -Path $env:RELAY_SERVER_DATA_DIR)}
$dataRoot=Resolve-RelayDataDirectory -DataDirectory $DataDirectory -PackageRoot $packageRoot -CheckLegacy
if(-not $ServerBinary){$ServerBinary=Join-Path $packageRoot 'dist\relay-server.exe'}
if(-not $SfuBinary){$SfuBinary=Join-Path $packageRoot 'dist\livekit-server.exe'}
$ServerBinary=[IO.Path]::GetFullPath($ServerBinary)
$SfuBinary=[IO.Path]::GetFullPath($SfuBinary)
$null=Assert-RelayHostingLocalPath $dataRoot
$protectedRoots=@([IO.Path]::GetPathRoot($dataRoot),$env:LOCALAPPDATA,$env:USERPROFILE,$env:SystemRoot)
if($dataRoot.TrimEnd('\') -eq [IO.Path]::GetPathRoot($dataRoot).TrimEnd('\') -or $protectedRoots -contains $dataRoot.TrimEnd('\')){throw 'Choose a dedicated server data directory.'}
foreach($relative in @('relay.sqlite3','managed-instance.json','server-runtime.json','stack-instance.json','stack-supervisor.json','stack-launch.lock','sfu-instance.json','caddy-instance.json','livekit.yaml','logs','logs\livekit.stdout.log','logs\livekit.stderr.log','logs\server.stdout.log','logs\server.stderr.log','logs\caddy.stdout.log','logs\caddy.stderr.log','logs\stack.stdout.log','logs\stack.stderr.log')){
    $null=Assert-RelayHostingLocalPath (Join-Path $dataRoot $relative)
}
$null=Assert-RelayHostingLocalPath $ServerBinary
$null=Assert-RelayHostingLocalPath $SfuBinary
foreach($binary in @($ServerBinary,$SfuBinary)){if(-not(Test-Path -LiteralPath $binary -PathType Leaf)){throw "Bundled server is missing: $binary"}}
if($dataRoot -match '["\x00-\x1F]' -or $ServerName -match '["\x00-\x1F]'){throw 'Names and paths cannot contain quotes or control characters.'}
if($publicMode){
    if(-not $CaddyBinary){$CaddyBinary=Join-Path $packageRoot 'dist\caddy.exe'}
    $CaddyBinary=Assert-RelayHostingLocalPath $CaddyBinary
    if(-not(Test-Path -LiteralPath $CaddyBinary -PathType Leaf)){throw 'Public HTTPS helper is missing. Install the verified bundled Caddy binary first.'}
    if(-not(Test-Path -LiteralPath (Join-Path $dataRoot 'relay.sqlite3') -PathType Leaf)){throw 'Create the server and its owner locally before enabling public hosting.'}
}
if(($tailscaleMode -or $embeddedMode) -and -not(Test-Path -LiteralPath (Join-Path $dataRoot 'relay.sqlite3') -PathType Leaf)){throw 'Create the server and its owner locally before enabling private hosting.'}
$ports=@($Port,$UdpPort,$SfuPort,$SfuTcpPort,$SfuUdpPort)
if(@($ports | Sort-Object -Unique).Count -ne $ports.Count){throw 'Choose separate Relay and SFU ports.'}
$record=Join-Path $dataRoot 'managed-instance.json'
if(Test-Path -LiteralPath $record){
    $existingHealthy=$false
    try {
        $previous=Get-Content -LiteralPath $record -Raw | ConvertFrom-Json
        $previousAddress=[Uri]$previous.url
        if($previousAddress.Scheme -ne 'http' -or -not([Net.IPAddress]::IsLoopback([Net.IPAddress]::Parse($previousAddress.Host)))){throw 'Managed health address is not loopback.'}
        $health=Invoke-RestMethod -Uri ($previous.url+'/api/health') -TimeoutSec 1
        if($health.instance_id -eq $previous.instance_id -and $health.server_id -eq $previous.server_id -and $health.pid -eq $previous.pid){
            $existingHealthy=$true
        }
    }catch{}
    if($existingHealthy){
        if($publicMode){throw 'A Relay stack is already running in this folder. Stop it explicitly before changing to public hosting; the local stack was left running.'}
        if($tailscaleMode -or $embeddedMode){throw 'A Relay stack is already running in this folder. Stop it explicitly before changing its connection.'}
        if($LanDiscovery){throw 'A Relay stack is already running in this folder. Stop it explicitly before changing who can find it on the network.'}
        if(-not $health.rtc_enabled){throw 'A chat-only Relay instance is already running in this folder. Stop it with stop-server.ps1, then start the complete stack.'}
        Write-Host "Relay already running: $($previous.url)";exit 0
    }
}
if($publicMode -or $tailscaleMode -or $embeddedMode -or $LanDiscovery){
    # Only inspect a stopped/checkpointed community. This CLI never migrates,
    # creates directories, binds listeners or initializes an account.
    $ownerCheck=& $ServerBinary --public-hosting-status --data $dataRoot
    if($LASTEXITCODE -ne 0 -and (($ownerCheck -join "`n") | ConvertFrom-Json).error_code -eq 'community_busy'){
        # A crash or forced stop leaves a write-ahead log. The ordinary local open
        # recovers it under the exclusive data lock (refused while a server runs),
        # then closes cleanly; only then is the read-only preflight repeated.
        & $ServerBinary --migrate-only --data $dataRoot | Out-Null
        if($LASTEXITCODE -eq 0){$ownerCheck=& $ServerBinary --public-hosting-status --data $dataRoot}
    }
    if($LASTEXITCODE -ne 0){throw 'Network hosting preflight failed. Initialize the owner locally, stop that local host normally, and use a server supporting read-only hosting preflight.'}
    $ownerStatus=($ownerCheck -join "`n") | ConvertFrom-Json
    if(-not $ownerStatus.ready -or -not $ownerStatus.owner_initialized -or -not $ownerStatus.server_id){throw 'The local server owner has not been initialized.'}
}
New-Item -ItemType Directory -Path $dataRoot,(Join-Path $dataRoot 'logs') -Force | Out-Null
$directory=[IO.DirectoryInfo]::new($dataRoot)
$acl=if($PSVersionTable.PSVersion.Major -ge 6){[IO.FileSystemAclExtensions]::GetAccessControl($directory)}else{$directory.GetAccessControl()};$acl.SetAccessRuleProtection($true,$false)
foreach($identity in @([Security.Principal.WindowsIdentity]::GetCurrent().User,[Security.Principal.SecurityIdentifier]::new('S-1-5-18'))){$acl.SetAccessRule([Security.AccessControl.FileSystemAccessRule]::new($identity,'FullControl','ContainerInherit,ObjectInherit','None','Allow'))}
if($PSVersionTable.PSVersion.Major -ge 6){[IO.FileSystemAclExtensions]::SetAccessControl($directory,$acl)}else{$directory.SetAccessControl($acl)}
$startupLock=[IO.File]::Open((Join-Path $dataRoot 'stack-launch.lock'),[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
try {
function Stop-OwnedChild([Diagnostics.Process]$Process,[string]$Executable,[DateTime]$Started){
    if(-not $Process){return};$Process.Refresh();if($Process.HasExited){return}
    if($Process.Path -ne $Executable -or $Process.StartTime.ToUniversalTime() -ne $Started){throw 'Process ownership changed; left it running.'}
    Stop-Process -Id $Process.Id
    if(-not $Process.WaitForExit(5000)){throw 'Owned child stop did not complete.'}
}
# Repeated launch after a Relay crash may leave the tracked private SFU running.
# Reuse/cleanup only the exact executable, creation time, and private config instance.
$sfuRecord=Join-Path $dataRoot 'sfu-instance.json'
$caddyRecord=Join-Path $dataRoot 'caddy-instance.json'
if(Test-Path -LiteralPath $sfuRecord){
    $old=Get-Content -LiteralPath $sfuRecord -Raw | ConvertFrom-Json
    $oldProcess=Get-Process -Id $old.pid -ErrorAction SilentlyContinue
    if($oldProcess){Stop-OwnedChild $oldProcess $old.exe ([DateTime]::Parse($old.started_at).ToUniversalTime())}
}
if(Test-Path -LiteralPath $caddyRecord){
    $old=Get-Content -LiteralPath $caddyRecord -Raw | ConvertFrom-Json
    $oldProcess=Get-Process -Id $old.pid -ErrorAction SilentlyContinue
    if($oldProcess){Stop-OwnedChild $oldProcess $old.exe ([DateTime]::Parse($old.started_at).ToUniversalTime())}
}
$stackId=[Guid]::NewGuid().ToString('N')
$key='relay'+[Guid]::NewGuid().ToString('N')
$secret=[Guid]::NewGuid().ToString('N')+[Guid]::NewGuid().ToString('N')
$configPath=Join-Path $dataRoot 'livekit.yaml'
$nodeIp=if($publicMode){$publicPlan.PublicAddress}elseif($tailscaleMode){$tailscalePlan.Address}elseif($embeddedMode){$EmbeddedIPv4}else{'127.0.0.1'}
$mediaFilter=if($tailscaleMode){"  enable_loopback_candidate: true`n  ips:`n    includes:`n      - $($tailscalePlan.Address)/32`n      - 127.0.0.0/8`n"}elseif($embeddedMode){"  enable_loopback_candidate: true`n  use_ice_lite: true`n  ips:`n    includes:`n      - 127.0.0.0/8`n"}else{''}
$config=@"
port: $SfuPort
bind_addresses: ["127.0.0.1"]
rtc:
  tcp_port: $SfuTcpPort
  udp_port: $SfuUdpPort
  use_external_ip: false
  node_ip: $nodeIp
${mediaFilter}keys:
  ${key}: $secret
logging:
  level: error
"@
[IO.File]::WriteAllText($configPath,$config,[Text.UTF8Encoding]::new($false))
$sfu=$null;$relay=$null;$caddy=$null;$supervisor=$null
$sfuStarted=$null;$relayStarted=$null;$caddyStarted=$null;$supervisorStarted=$null
$caddyConfigPath=$null
if($publicMode){
    # Isolate certificate/account material inside the already protected per-user
    # root. Do not install a local CA, alter system trust, or use shared storage.
    $publicRoot=Join-Path $dataRoot 'public-hosting'
    $caddyData=Join-Path $publicRoot 'caddy-data'
    $null=Assert-RelayHostingLocalPath $publicRoot
    $null=Assert-RelayHostingLocalPath $caddyData
    New-Item -ItemType Directory -Path $publicRoot,$caddyData -Force | Out-Null
    $caddyConfigPath=Join-Path $publicRoot 'Caddyfile'
    $null=Assert-RelayHostingLocalPath $caddyConfigPath
    $storage="admin off`n    storage file_system {`n        root `"$($caddyData.Replace('\','/'))`"`n    }"
    $caddyConfig=$publicPlan.Caddyfile.Replace('admin off',$storage)
    [IO.File]::WriteAllText($caddyConfigPath,$caddyConfig,[Text.UTF8Encoding]::new($false))
}
$oldEnv=@{};foreach($name in @('RELAY_RTC_URL','RELAY_RTC_API_URL','RELAY_RTC_API_KEY','RELAY_RTC_API_SECRET')){$oldEnv[$name]=[Environment]::GetEnvironmentVariable($name,'Process')}
try {
    $sfu=Start-Process -FilePath $SfuBinary -ArgumentList ('--config "'+$configPath+'"') -WorkingDirectory $packageRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $dataRoot 'logs\livekit.stdout.log') -RedirectStandardError (Join-Path $dataRoot 'logs\livekit.stderr.log')
    $sfuStarted=$sfu.StartTime.ToUniversalTime()
    $sfuReady=$false
    for($attempt=0;$attempt -lt 50;$attempt++){$sfu.Refresh();if($sfu.HasExited){throw 'Private video server exited; inspect livekit.stderr.log'};try{Invoke-WebRequest -Uri "http://127.0.0.1:$SfuPort/" -UseBasicParsing -TimeoutSec 1 | Out-Null;$sfuReady=$true;break}catch{};Start-Sleep -Milliseconds 200}
    if(-not $sfuReady){throw 'Private video server readiness failed.'}
    [ordered]@{pid=$sfu.Id;exe=$SfuBinary;started_at=$sfuStarted.ToString('o');config=$configPath;port=$SfuPort;stack_id=$stackId} | ConvertTo-Json | Set-Content -LiteralPath $sfuRecord -Encoding UTF8
    $env:RELAY_RTC_URL=if($publicMode){$publicPlan.RtcUrl}elseif($tailscaleMode){$tailscalePlan.RtcUrl}elseif($embeddedMode){"ws://${EmbeddedIPv4}:$Port"}else{"ws://127.0.0.1:$Port"}
    $env:RELAY_RTC_API_URL="http://127.0.0.1:$SfuPort"
    $env:RELAY_RTC_API_KEY=$key
    $env:RELAY_RTC_API_SECRET=$secret
    # LAN opt-in opens only the sealed-code listener. Ordinary API and voice stay local.
    $bindAddress='127.0.0.1'
    $args='--bind {4} --port {0} --udp-port {1} --data "{2}" --name "{3}"' -f $Port,$UdpPort,$dataRoot,$ServerName,$bindAddress
    if($LanDiscovery){$args+=' --lan-discovery --lan-pairing-only'}
    if($publicMode){$args+=' --udp-bind 0.0.0.0 --trusted-proxy 127.0.0.1'}
    if($tailscaleMode){$args+=' --tailscale-bind '+$tailscalePlan.Address}
    if($embeddedMode){$args+=' --network-peer-port '+$NetworkPeerPort}
    $relay=Start-Process -FilePath $ServerBinary -ArgumentList $args -WorkingDirectory $packageRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $dataRoot 'logs\server.stdout.log') -RedirectStandardError (Join-Path $dataRoot 'logs\server.stderr.log')
    $relayStarted=$relay.StartTime.ToUniversalTime();$ready=$false
    for($attempt=0;$attempt -lt 50;$attempt++){$relay.Refresh();if($relay.HasExited){throw 'Relay exited; inspect server.stderr.log'};try{$health=Invoke-RestMethod -Uri "http://127.0.0.1:$Port/api/health" -TimeoutSec 1;if($health.pid -eq $relay.Id -and $health.managed_instance -and $health.instance_id){$ready=$true;break}}catch{};Start-Sleep -Milliseconds 200}
    if(-not $ready){throw 'Relay readiness failed.'}
    if($publicMode){
        $caddy=Start-Process -FilePath $CaddyBinary -ArgumentList ('run --config "'+$caddyConfigPath+'" --adapter caddyfile') -WorkingDirectory $packageRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $dataRoot 'logs\caddy.stdout.log') -RedirectStandardError (Join-Path $dataRoot 'logs\caddy.stderr.log')
        $caddyStarted=$caddy.StartTime.ToUniversalTime()
        Start-Sleep -Milliseconds 500
        $caddy.Refresh();if($caddy.HasExited){throw 'Public HTTPS helper exited; inspect caddy.stderr.log. Public access was not confirmed.'}
        [ordered]@{pid=$caddy.Id;exe=$CaddyBinary;started_at=$caddyStarted.ToString('o');config=$caddyConfigPath;stack_id=$stackId} | ConvertTo-Json | Set-Content -LiteralPath $caddyRecord -Encoding UTF8
    }
    $hostingMode=if($embeddedMode){'embedded'}elseif($tailscaleMode){'tailscale'}elseif($publicMode){'public'}else{'local'}
    $advertisedUrl=if($embeddedMode){"http://${EmbeddedIPv4}:$Port"}elseif($tailscaleMode){$tailscalePlan.RelayUrl}elseif($publicMode){$publicPlan.RelayUrl}else{"http://127.0.0.1:$Port"}
    $lanActual=$null;$lanPairingPort=$null
    try {
        $runtime=Get-Content -LiteralPath (Join-Path $dataRoot 'server-runtime.json') -Raw | ConvertFrom-Json
        if($runtime.server_id -ceq $health.server_id -and $runtime.instance_id -ceq $health.instance_id -and $runtime.pid -eq $relay.Id -and $runtime.lan_discovery -is [bool]){
            $lanActual=$runtime.lan_discovery
            if($lanActual -and ($runtime.lan_pairing_port -is [long] -or $runtime.lan_pairing_port -is [int]) -and $runtime.lan_pairing_port -ge 1024 -and $runtime.lan_pairing_port -le 65535){$lanPairingPort=$runtime.lan_pairing_port}
        }
    }catch{}
    $stackRecord=[ordered]@{stack_id=$stackId;server_id=$health.server_id;instance_id=$health.instance_id;relay_pid=$relay.Id;relay_exe=$ServerBinary;relay_started_at=$relayStarted.ToString('o');sfu_pid=$sfu.Id;sfu_exe=$SfuBinary;sfu_started_at=$sfuStarted.ToString('o');public_mode=$publicMode;tailscale_mode=$tailscaleMode;hosting_mode=$hostingMode;advertised_url=$advertisedUrl;lan_discovery=$lanActual;lan_pairing_port=$lanPairingPort}
    if($publicMode){$stackRecord.caddy_pid=$caddy.Id;$stackRecord.caddy_exe=$CaddyBinary;$stackRecord.caddy_started_at=$caddyStarted.ToString('o');$stackRecord.public_url=$publicPlan.RelayUrl;$stackRecord.public_https_status='certificate_and_external_check_pending'}
    if($tailscaleMode){$stackRecord.tailscale_ipv4=$tailscalePlan.Address;$stackRecord.tailscale_url=$tailscalePlan.RelayUrl}
    if($embeddedMode){$stackRecord.embedded_ipv4=$EmbeddedIPv4;$stackRecord.network_peer_port=$NetworkPeerPort}
    $stackRecord | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $dataRoot 'stack-instance.json') -Encoding UTF8
    $supervisorScript=Join-Path $PSScriptRoot 'supervise-stack.ps1'
    $supervisorArgs='-NoProfile -ExecutionPolicy Bypass -File "{0}" -DataDirectory "{1}"' -f $supervisorScript,$dataRoot
    $supervisor=Start-Process -FilePath 'powershell.exe' -ArgumentList $supervisorArgs -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $dataRoot 'logs\stack.stdout.log') -RedirectStandardError (Join-Path $dataRoot 'logs\stack.stderr.log')
    $supervisorStarted=$supervisor.StartTime.ToUniversalTime()
    [ordered]@{pid=$supervisor.Id;started_at=$supervisorStarted.ToString('o');relay_pid=$relay.Id;stack_id=$stackId} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $dataRoot 'stack-supervisor.json') -Encoding UTF8
    Write-Host "Relay ready with native video at http://127.0.0.1:$Port"
    if($publicMode){Write-Host "Public HTTPS helper started for $($publicPlan.RelayUrl). Certificate and external reachability still need verification; router/firewall settings were not changed."}
    if($lanActual -eq $true){Write-Host 'Local code discovery started. Login, administration and video keep their existing private connection. No router or firewall settings were changed.'}
    elseif($LanDiscovery){Write-Warning 'The community is running, but local code discovery could not be confirmed. Use its secure invitation.'}
    if($tailscaleMode){Write-Host "Private Tailscale host started at $($tailscalePlan.RelayUrl). Friends need Tailscale access to this PC; no router or firewall settings were changed."}
}catch{
    $failure=$_
    # Try all exact owned children even if one ownership check fails. Never kill
    # a reused PID or an untracked process to make rollback appear successful.
    foreach($owned in @(
        @{Process=$supervisor;Executable=if($supervisor){$supervisor.Path}else{''};Started=$supervisorStarted},
        @{Process=$caddy;Executable=$CaddyBinary;Started=$caddyStarted},
        @{Process=$relay;Executable=$ServerBinary;Started=$relayStarted},
        @{Process=$sfu;Executable=$SfuBinary;Started=$sfuStarted}
    )){try{Stop-OwnedChild $owned.Process $owned.Executable $owned.Started}catch{Write-Warning 'A child ownership check prevented cleanup; inspect the tracked process record.'}}
    throw $failure
}finally{
    foreach($name in $oldEnv.Keys){[Environment]::SetEnvironmentVariable($name,$oldEnv[$name],'Process')}
}

} finally { $startupLock.Dispose() }
