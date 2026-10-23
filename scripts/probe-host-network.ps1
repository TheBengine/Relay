param([string]$RouterWanIPv4)

# Read-only hosting diagnostics. Dot-source this file to load the pure helpers
# without reading network interfaces, contacting ipify, or creating a COM object.
function ConvertTo-RelayIPv4 {
    param([AllowNull()][object]$Address)
    if($Address -isnot [string] -or $Address -notmatch '^([0-9]{1,3}\.){3}[0-9]{1,3}$'){return $null}
    $parsed=$null
    if(-not [Net.IPAddress]::TryParse($Address, [ref]$parsed) -or
       $parsed.AddressFamily -ne [Net.Sockets.AddressFamily]::InterNetwork -or
       $parsed.ToString() -cne $Address){return $null}
    return $parsed.ToString()
}

function Test-RelayUsableHostIPv4 {
    param([AllowNull()][object]$Address)
    $valid=ConvertTo-RelayIPv4 $Address
    if(-not $valid){return $false}
    $octets=[Net.IPAddress]::Parse($valid).GetAddressBytes()
    return ($octets[0] -ne 0 -and $octets[0] -ne 127 -and $octets[0] -lt 224 -and
            -not ($octets[0] -eq 169 -and $octets[1] -eq 254))
}

function Test-RelayGlobalIPv4 {
    param([AllowNull()][object]$Address)
    if(-not (Test-RelayUsableHostIPv4 $Address)){return $false}
    $b=[Net.IPAddress]::Parse($Address).GetAddressBytes()
    # Conservatively reject non-global special-use ranges. The two globally
    # reachable 192.0.0/24 anycast exceptions follow the IANA IPv4 registry:
    # https://www.iana.org/assignments/iana-ipv4-special-registry/
    if($b[0] -eq 10 -or
       ($b[0] -eq 100 -and $b[1] -ge 64 -and $b[1] -le 127) -or
       ($b[0] -eq 172 -and $b[1] -ge 16 -and $b[1] -le 31) -or
       ($b[0] -eq 192 -and $b[1] -eq 168) -or
       ($b[0] -eq 192 -and $b[1] -eq 0 -and $b[2] -eq 0 -and $b[3] -notin @(9,10)) -or
       ($b[0] -eq 192 -and $b[1] -eq 0 -and $b[2] -eq 2) -or
       ($b[0] -eq 192 -and $b[1] -eq 88 -and $b[2] -eq 99) -or
       ($b[0] -eq 198 -and $b[1] -in @(18,19)) -or
       ($b[0] -eq 198 -and $b[1] -eq 51 -and $b[2] -eq 100) -or
       ($b[0] -eq 203 -and $b[1] -eq 0 -and $b[2] -eq 113)){return $false}
    return $true
}

function Select-RelayPhysicalNetwork {
    param([AllowEmptyCollection()][object[]]$Candidates)
    $eligible=@($Candidates | Where-Object {
        $_.is_physical -eq $true -and $_.interface_up -eq $true -and
        $_.interface_name -notmatch '(?i)tailscale|wireguard|openvpn|vpn|tunnel|zerotier|hamachi' -and
        (Test-RelayUsableHostIPv4 $_.ipv4_address) -and
        (Test-RelayUsableHostIPv4 $_.default_gateway)
    } | Sort-Object @{Expression={[long]$_.route_metric+[long]$_.interface_metric}},
                         @{Expression={[int]$_.interface_index}},
                         @{Expression={[string]$_.ipv4_address}})
    if($eligible.Count -eq 0){return $null}
    return [pscustomobject]@{
        lan_ipv4=[string]$eligible[0].ipv4_address
        default_gateway=[string]$eligible[0].default_gateway
    }
}

function Get-RelayPhysicalNetwork {
    $physical=@(Get-NetAdapter -Physical -ErrorAction Stop | Where-Object {$_.Status -eq 'Up'})
    $routes=@(Get-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' -ErrorAction Stop)
    $interfaces=@(Get-NetIPInterface -AddressFamily IPv4 -ErrorAction Stop)
    $addresses=@(Get-NetIPAddress -AddressFamily IPv4 -ErrorAction Stop | Where-Object {
        $_.AddressState -eq 'Preferred' -and -not $_.SkipAsSource
    })
    $candidates=@()
    foreach($adapter in $physical){
        $index=[int]$adapter.InterfaceIndex
        $interface=@($interfaces | Where-Object {$_.InterfaceIndex -eq $index} | Select-Object -First 1)
        if($interface.Count -eq 0){continue}
        foreach($route in @($routes | Where-Object {$_.InterfaceIndex -eq $index})){
            foreach($address in @($addresses | Where-Object {$_.InterfaceIndex -eq $index})){
                $candidates += [pscustomobject]@{
                    is_physical=$true
                    interface_up=$true
                    interface_name=[string]$adapter.Name
                    interface_index=$index
                    ipv4_address=[string]$address.IPAddress
                    default_gateway=[string]$route.NextHop
                    route_metric=[long]$route.RouteMetric
                    interface_metric=[long]$interface[0].InterfaceMetric
                }
            }
        }
    }
    return Select-RelayPhysicalNetwork -Candidates $candidates
}

function ConvertFrom-RelayPublicIPv4Response {
    param([AllowNull()][byte[]]$Body)
    if($null -eq $Body -or $Body.Length -eq 0 -or $Body.Length -gt 1024){return $null}
    try {
        $utf8=[Text.UTF8Encoding]::new($false, $true)
        $text=$utf8.GetString($Body).Trim()
        if(-not $text.StartsWith('{') -or -not $text.EndsWith('}')){return $null}
        $payload=$text | ConvertFrom-Json -ErrorAction Stop
        if($payload -isnot [pscustomobject] -or -not (Test-RelayGlobalIPv4 $payload.ip)){return $null}
        return [string]$payload.ip
    } catch {return $null}
}

function Get-RelayPublicIPv4 {
    $client=$null; $handler=$null; $response=$null; $stream=$null; $cancel=$null
    $clock=[Diagnostics.Stopwatch]::StartNew()
    try {
        Add-Type -AssemblyName System.Net.Http -ErrorAction Stop
        $handler=[Net.Http.HttpClientHandler]::new()
        $handler.AllowAutoRedirect=$false
        $client=[Net.Http.HttpClient]::new($handler)
        $client.Timeout=[TimeSpan]::FromSeconds(8)
        $cancel=[Threading.CancellationTokenSource]::new()
        $cancel.CancelAfter(8000)
        $task=$client.GetAsync('https://api.ipify.org?format=json',
            [Net.Http.HttpCompletionOption]::ResponseHeadersRead, $cancel.Token)
        $remaining=8000-[int]$clock.ElapsedMilliseconds
        if($remaining -le 0 -or -not $task.Wait($remaining)){return $null}
        $response=$task.GetAwaiter().GetResult()
        if(-not $response.IsSuccessStatusCode -or
           ($null -ne $response.Content.Headers.ContentLength -and
            $response.Content.Headers.ContentLength -gt 1024)){return $null}
        $streamTask=$response.Content.ReadAsStreamAsync()
        $remaining=8000-[int]$clock.ElapsedMilliseconds
        if($remaining -le 0 -or -not $streamTask.Wait($remaining)){return $null}
        $stream=$streamTask.GetAwaiter().GetResult()
        # An extra byte detects oversized chunked responses without buffering
        # an arbitrary remote response. Every read shares the total deadline.
        $buffer=[byte[]]::new(1025)
        $used=0
        while($used -lt $buffer.Length){
            $remaining=8000-[int]$clock.ElapsedMilliseconds
            if($remaining -le 0){return $null}
            $readTask=$stream.ReadAsync($buffer, $used, $buffer.Length-$used, $cancel.Token)
            if(-not $readTask.Wait($remaining)){return $null}
            $read=$readTask.GetAwaiter().GetResult()
            if($read -eq 0){break}
            $used += $read
        }
        if($used -gt 1024){return $null}
        $body=[byte[]]::new($used)
        [Array]::Copy($buffer, $body, $used)
        return ConvertFrom-RelayPublicIPv4Response -Body $body
    } catch {return $null}
    finally {
        if($cancel){$cancel.Cancel()}
        if($stream){$stream.Dispose()}
        if($response){$response.Dispose()}
        if($client){$client.Dispose()}elseif($handler){$handler.Dispose()}
        if($cancel){$cancel.Dispose()}
        $clock.Stop()
    }
}

function Get-RelayUpnpAvailability {
    # COM discovery can stall on a router. Keep it on a bounded background
    # thread; never enumerate, add, remove, or inspect anyone's port mappings.
    if(-not ('RelayReadOnlyUpnpProbe' -as [type])){
        Add-Type -TypeDefinition @'
using System;
using System.Reflection;
using System.Runtime.InteropServices;
using System.Threading;
public static class RelayReadOnlyUpnpProbe {
    public static bool IsAvailable() {
        bool available = false;
        var worker = new Thread(() => {
            object nat = null, collection = null;
            try {
                var type = Type.GetTypeFromProgID("HNetCfg.NATUPnP");
                if (type == null) return;
                nat = Activator.CreateInstance(type);
                collection = type.InvokeMember("StaticPortMappingCollection",
                    BindingFlags.GetProperty, null, nat, null);
                available = collection != null;
            } catch { }
            finally {
                if (collection != null && Marshal.IsComObject(collection)) {
                    try { Marshal.FinalReleaseComObject(collection); } catch { }
                }
                if (nat != null && Marshal.IsComObject(nat)) {
                    try { Marshal.FinalReleaseComObject(nat); } catch { }
                }
            }
        });
        worker.IsBackground = true;
        worker.SetApartmentState(ApartmentState.STA);
        worker.Start();
        return worker.Join(2000) && available;
    }
}
'@ -ErrorAction Stop
    }
    return [RelayReadOnlyUpnpProbe]::IsAvailable()
}

function New-RelayHostNetworkResult {
    param([AllowNull()][object]$PhysicalNetwork,
          [AllowNull()][object]$PublicIPv4,
          [bool]$UpnpAvailable=$false,
          [AllowNull()][object]$RouterWanIPv4)
    $lan=$null; $gateway=$null; $public=$null; $wan=$null
    $wanStatus='unverified'; $cgnatStatus='unverified'
    if($PhysicalNetwork -and
       (Test-RelayUsableHostIPv4 $PhysicalNetwork.lan_ipv4) -and
       (Test-RelayUsableHostIPv4 $PhysicalNetwork.default_gateway)){
        $lan=[string]$PhysicalNetwork.lan_ipv4
        $gateway=[string]$PhysicalNetwork.default_gateway
    }
    if(Test-RelayGlobalIPv4 $PublicIPv4){$public=[string]$PublicIPv4}
    if(Test-RelayUsableHostIPv4 $RouterWanIPv4){
        $wan=[string]$RouterWanIPv4
        $b=[Net.IPAddress]::Parse($wan).GetAddressBytes()
        if($b[0] -eq 100 -and $b[1] -ge 64 -and $b[1] -le 127){
            # RFC 6598 shared WAN space is evidence of upstream carrier NAT,
            # unlike absence of UPnP or a difference from the LAN address.
            $wanStatus='upstream_nat'; $cgnatStatus='shared_address'
        } elseif($b[0] -eq 10 -or
                 ($b[0] -eq 172 -and $b[1] -ge 16 -and $b[1] -le 31) -or
                 ($b[0] -eq 192 -and $b[1] -eq 168)){
            $wanStatus='double_nat_possible'
        } elseif(Test-RelayGlobalIPv4 $wan){
            if($public -and $wan -ceq $public){$wanStatus='public_address_matches'}
            elseif($public){$wanStatus='mismatch_unverified'}
        }
    }
    return [pscustomobject][ordered]@{
        schema_version=1
        lan_ipv4=$lan
        default_gateway=$gateway
        public_ipv4=$public
        public_ip_status=$(if($public){'available'}else{'unavailable'})
        upnp_available=$UpnpAvailable
        automatic_mapping_unavailable=(-not $UpnpAvailable)
        upnp_external_ipv4=$null
        router_wan_ipv4=$wan
        # ipify observes outbound egress; it cannot prove router WAN state,
        # inbound reachability, CGNAT, or the physical WAN when a VPN is active.
        wan_status=$wanStatus
        cgnat_status=$cgnatStatus
    }
}

function Invoke-RelayHostNetworkProbe {
    param([AllowNull()][object]$RouterWanIPv4)
    $physical=$null; $public=$null; $upnp=$false
    try {$physical=Get-RelayPhysicalNetwork} catch {}
    try {$public=Get-RelayPublicIPv4} catch {}
    try {$upnp=Get-RelayUpnpAvailability} catch {}
    return New-RelayHostNetworkResult -PhysicalNetwork $physical -PublicIPv4 $public -UpnpAvailable $upnp -RouterWanIPv4 $RouterWanIPv4
}

if($MyInvocation.InvocationName -ne '.'){
    Invoke-RelayHostNetworkProbe -RouterWanIPv4 $RouterWanIPv4 | ConvertTo-Json -Depth 3 -Compress
}
