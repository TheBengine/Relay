"""Test read-only host-network classification with injected data, never real NICs."""
from __future__ import annotations

import json
from pathlib import Path
import subprocess


def main():
    project = Path(__file__).resolve().parents[1]
    shell = Path(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
    probe = str(project / "scripts" / "probe-host-network.ps1").replace("'", "''")
    code = r"""
$ErrorActionPreference='Stop'
# Dot-sourcing must not run any probe or touch user network interfaces.
function Get-NetAdapter {throw 'A test must not enumerate real adapters'}
function Get-NetRoute {throw 'A test must not enumerate real routes'}
function Get-NetIPInterface {throw 'A test must not enumerate real interfaces'}
function Get-NetIPAddress {throw 'A test must not enumerate real addresses'}
$loaded=@(. '__PROBE__')
if($loaded.Count -ne 0){throw 'Dot-source produced probe output'}

foreach($good in @('1.1.1.1','8.8.8.8','100.63.255.255','100.128.0.0','192.0.0.9','192.0.0.10')){
    if(-not (Test-RelayGlobalIPv4 $good)){throw "Rejected global address $good"}
}
foreach($bad in @($null,'','127.0.0.1','0.0.0.0','0.1.2.3','10.2.3.4','100.64.0.0',
    '100.127.255.255','169.254.1.2','172.16.0.1','172.31.255.255','192.168.2.1',
    '192.0.0.8','192.0.0.11','192.0.2.2','192.88.99.2','198.18.0.1','198.19.255.255',
    '198.51.100.2','203.0.113.1','224.0.0.1','255.255.255.255','::1',
    '1.1.1.1:80','001.1.1.1','8.8.8.999','134744072','1.1.1.1 ')){
    if(Test-RelayGlobalIPv4 $bad){throw "Accepted invalid/non-global address $bad"}
}
function Candidate($name,$physical,$up,$ip,$gateway,$route,$metric,$index){
    [pscustomobject]@{interface_name=$name;is_physical=$physical;interface_up=$up;
        ipv4_address=$ip;default_gateway=$gateway;route_metric=$route;
        interface_metric=$metric;interface_index=$index}
}
$candidates=@(
    (Candidate 'Tailscale' $false $true '100.80.1.2' '100.80.0.1' 0 0 1),
    (Candidate 'My VPN' $true $true '10.0.0.2' '10.0.0.1' 0 0 2),
    (Candidate 'Ethernet unplugged' $true $false '192.168.0.2' '192.168.0.1' 0 0 3),
    (Candidate 'No gateway' $true $true '192.168.3.2' '0.0.0.0' 0 0 4),
    (Candidate 'Link local' $true $true '169.254.1.2' '192.168.1.1' 0 0 5),
    (Candidate 'Wi-Fi' $true $true '192.168.1.2' '192.168.1.1' 10 60 8),
    (Candidate 'Ethernet' $true $true '192.168.2.2' '192.168.2.1' 25 10 9)
)
$selected=Select-RelayPhysicalNetwork -Candidates $candidates
if($selected.lan_ipv4 -cne '192.168.2.2' -or $selected.default_gateway -cne '192.168.2.1'){
    throw 'Preferred physical default did not use combined metrics or ignored exclusions'
}
if($null -ne (Select-RelayPhysicalNetwork -Candidates @())){throw 'Empty network selected'}
if($null -ne (Select-RelayPhysicalNetwork -Candidates $candidates[0..4])){throw 'Ineligible network selected'}
$ties=@((Candidate 'Second' $true $true '10.3.0.2' '10.3.0.1' 10 10 20),
        (Candidate 'First' $true $true '10.2.0.2' '10.2.0.1' 15 5 10))
if((Select-RelayPhysicalNetwork -Candidates $ties).lan_ipv4 -cne '10.2.0.2'){
    throw 'Equal-metric networks were not selected deterministically'
}
function Bytes($text){return [Text.Encoding]::UTF8.GetBytes($text)}
if((ConvertFrom-RelayPublicIPv4Response -Body (Bytes '{"ip":"8.8.8.8"}')) -cne '8.8.8.8'){
    throw 'Valid bounded IP response failed'
}
foreach($payload in @('{"ip":"100.70.1.2"}','{"ip":"127.0.0.1"}',
    '{"ip":"::1"}','{"ip":123}','{"ip":["8.8.8.8"]}',
    '[{"ip":"8.8.8.8"}]','{}','{"ip":"8.8.8.8"','"8.8.8.8"')){
    if($null -ne (ConvertFrom-RelayPublicIPv4Response -Body (Bytes $payload))){throw "Invalid response accepted: $payload"}
}
if($null -ne (ConvertFrom-RelayPublicIPv4Response -Body ([byte[]]::new(1025)))){throw 'Oversized body accepted'}
if($null -ne (ConvertFrom-RelayPublicIPv4Response -Body ([byte[]]@(255)))){throw 'Invalid UTF-8 accepted'}

foreach($available in @($false,$true)){
    $report=New-RelayHostNetworkResult -PhysicalNetwork $selected -PublicIPv4 '8.8.8.8' -UpnpAvailable $available
    if($report.upnp_available -ne $available -or $report.automatic_mapping_unavailable -eq $available){
        throw 'UPnP availability classification wrong'
    }
    if($report.wan_status -cne 'unverified' -or $report.cgnat_status -cne 'unverified' -or
       $null -ne $report.upnp_external_ipv4){throw 'Probe invented WAN/CGNAT evidence'}
}
$empty=New-RelayHostNetworkResult -PhysicalNetwork $null -PublicIPv4 '100.80.0.1'
if($null -ne $empty.public_ipv4 -or $empty.public_ip_status -cne 'unavailable' -or
   $null -ne $empty.lan_ipv4 -or $null -ne $empty.default_gateway){throw 'Unavailable states wrong'}
foreach($wan in @('100.64.0.1','100.127.1.2')){
    $shared=New-RelayHostNetworkResult -RouterWanIPv4 $wan -PublicIPv4 '8.8.8.8'
    if($shared.wan_status -cne 'upstream_nat' -or $shared.cgnat_status -cne 'shared_address'){
        throw 'Shared router WAN did not classify as upstream carrier NAT'
    }
}
foreach($wan in @('10.1.2.3','172.16.1.2','192.168.3.2')){
    $private=New-RelayHostNetworkResult -RouterWanIPv4 $wan -PublicIPv4 '8.8.8.8'
    if($private.wan_status -cne 'double_nat_possible' -or $private.cgnat_status -cne 'unverified'){
        throw 'Private WAN incorrectly proved CGNAT'
    }
}
$mismatch=New-RelayHostNetworkResult -RouterWanIPv4 '1.1.1.1' -PublicIPv4 '8.8.8.8'
if($mismatch.wan_status -cne 'mismatch_unverified' -or $mismatch.cgnat_status -cne 'unverified'){
    throw 'Different public WAN addresses incorrectly proved CGNAT'
}
$same=New-RelayHostNetworkResult -RouterWanIPv4 '8.8.8.8' -PublicIPv4 '8.8.8.8'
if($same.wan_status -cne 'public_address_matches' -or $same.cgnat_status -cne 'unverified'){
    throw 'Matching public WAN incorrectly claimed full inbound reachability'
}
foreach($wan in @('127.0.0.1','0.0.0.0','::1','not-an-ip')){
    $badWan=New-RelayHostNetworkResult -RouterWanIPv4 $wan -PublicIPv4 '8.8.8.8'
    if($badWan.wan_status -cne 'unverified' -or $null -ne $badWan.router_wan_ipv4){
        throw 'Invalid router WAN classified or emitted'
    }
}
# Test the complete worker contract using only injected read functions.
function Get-RelayPhysicalNetwork {return $selected}
function Get-RelayPublicIPv4 {return '1.1.1.1'}
function Get-RelayUpnpAvailability {return $false}
$json=Invoke-RelayHostNetworkProbe | ConvertTo-Json -Depth 3 -Compress
$actual=$json | ConvertFrom-Json
if($actual.schema_version -ne 1 -or $actual.public_ipv4 -cne '1.1.1.1' -or
   $actual.lan_ipv4 -cne '192.168.2.2' -or -not $actual.automatic_mapping_unavailable -or
   $actual.cgnat_status -cne 'unverified'){throw 'Worker contract mismatch'}
function Get-RelayPhysicalNetwork {throw 'Injected local network failure'}
function Get-RelayPublicIPv4 {throw 'Injected lookup failure'}
function Get-RelayUpnpAvailability {throw 'Injected COM failure'}
$failed=Invoke-RelayHostNetworkProbe
if($null -ne $failed.lan_ipv4 -or $null -ne $failed.public_ipv4 -or $failed.upnp_available -or
   $failed.wan_status -cne 'unverified'){throw 'Read failures leaked or fabricated data'}
$json
""".replace("__PROBE__", probe)
    result = subprocess.run(
        [str(shell), "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", code],
        capture_output=True, text=True, timeout=20,
    )
    if result.returncode:
        raise AssertionError(result.stdout + result.stderr)
    report = json.loads(result.stdout.strip())
    assert set(report) == {
        "schema_version", "lan_ipv4", "default_gateway", "public_ipv4", "public_ip_status",
        "upnp_available", "automatic_mapping_unavailable", "upnp_external_ipv4", "router_wan_ipv4", "wan_status", "cgnat_status",
    }
    print(json.dumps({"windows_powershell": True, "pure_network_classification": True,
                      "public_response_validation": True, "router_wan_classification": True,
                      "injected_worker_contract": True,
                      "real_network_access": False}))


if __name__ == "__main__":
    main()
