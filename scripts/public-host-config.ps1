# Pure public-hosting plan. Dot-sourcing and calling these helpers never opens
# ports, writes configuration, launches processes or accepts certificate terms.

function ConvertTo-RelayStrictIPv4 {
    param([Parameter(Mandatory=$true)][string]$Address)
    # IPAddress.Parse accepts legacy forms such as 127.1 and octal octets. Only
    # canonical dotted decimal is safe to interpolate into addresses/config.
    if($Address -cnotmatch '\A(0|[1-9][0-9]{0,2})\.(0|[1-9][0-9]{0,2})\.(0|[1-9][0-9]{0,2})\.(0|[1-9][0-9]{0,2})\z'){
        throw 'Enter an IPv4 address in dotted decimal, without a URL, port or whitespace.'
    }
    $octets=@($Address.Split('.') | ForEach-Object { [int]$_ })
    if(@($octets | Where-Object {$_ -gt 255}).Count -ne 0){
        throw 'Each IPv4 address part must be between 0 and 255.'
    }
    return ,$octets
}

function Test-RelayPrivateIPv4 {
    param([Parameter(Mandatory=$true)][int[]]$Octets)
    return ($Octets[0] -eq 10 -or
        ($Octets[0] -eq 172 -and $Octets[1] -ge 16 -and $Octets[1] -le 31) -or
        ($Octets[0] -eq 192 -and $Octets[1] -eq 168))
}

function Test-RelayPublicIPv4 {
    param([Parameter(Mandatory=$true)][int[]]$Octets)
    # Exclude special-purpose, documentation, benchmark, shared (CGNAT),
    # loopback, link-local, multicast and future-use ranges. Conservatively
    # reject the entire 192.0.0/24 and retired 192.88.99/24 special ranges.
    return -not ((Test-RelayPrivateIPv4 -Octets $Octets) -or
        $Octets[0] -eq 0 -or $Octets[0] -eq 127 -or $Octets[0] -ge 224 -or
        ($Octets[0] -eq 100 -and $Octets[1] -ge 64 -and $Octets[1] -le 127) -or
        ($Octets[0] -eq 169 -and $Octets[1] -eq 254) -or
        ($Octets[0] -eq 192 -and $Octets[1] -eq 0 -and $Octets[2] -in @(0,2)) -or
        ($Octets[0] -eq 192 -and $Octets[1] -eq 88 -and $Octets[2] -eq 99) -or
        ($Octets[0] -eq 198 -and $Octets[1] -in @(18,19)) -or
        ($Octets[0] -eq 198 -and $Octets[1] -eq 51 -and $Octets[2] -eq 100) -or
        ($Octets[0] -eq 203 -and $Octets[1] -eq 0 -and $Octets[2] -eq 113))
}

function Assert-RelayPublicCertificateTerms {
    param([switch]$AcceptedCertificateTerms)
    if(-not $AcceptedCertificateTerms){
        throw 'Public HTTPS requires explicit acceptance of the certificate issuer terms. Pass -AcceptedCertificateTerms only after the host has accepted them.'
    }
}

function New-RelayPublicHostingPlan {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory=$true)][string]$PublicIPv4,
        [Parameter(Mandatory=$true)][string]$LocalIPv4,
        [switch]$AcceptedCertificateTerms,
        [switch]$EnableHttpChallenge
    )
    $publicOctets=ConvertTo-RelayStrictIPv4 -Address $PublicIPv4
    $localOctets=ConvertTo-RelayStrictIPv4 -Address $LocalIPv4
    if(-not(Test-RelayPublicIPv4 -Octets $publicOctets)){
        throw 'Public hosting needs a globally routable IPv4 address. Private, CGNAT, reserved and loopback addresses cannot be used.'
    }
    if(-not(Test-RelayPrivateIPv4 -Octets $localOctets)){
        throw 'LocalIPv4 must be this PC''s private LAN address (10.x, 172.16-31.x or 192.168.x).'
    }

    # Literal template: only previously validated ASCII IPv4 is substituted.
    # A route block keeps all rejection handlers ahead of the reverse proxy.
    # Caddy path_regexp matches decoded paths; reject remaining %xx to close
    # double-decoding ambiguity, backslashes and dot-segment ambiguity too.
    $template=@'
{
    admin off
    auto_https disable_redirects
    servers {
        protocols h1 h2
    }
}

https://__PUBLIC_IPV4__ {
    tls {
        issuer acme {
            dir https://acme-v02.api.letsencrypt.org/directory
            profile shortlived
__HTTP_CHALLENGE_POLICY__
        }
    }
    route {
        @ambiguousPath path_regexp relayAmbiguous "(?i)(%[0-9a-f]{2}|\\\\|(^|/)[.]{1,2}(/|$))"
        respond @ambiguousPath "Not available" 403
        @hostOnly path_regexp relayHostOnly (?i)^/+(api/+(local|local-management|host-secret)(/+|$)|local-management(/+|$)|host-secret(/+|$))
        respond @hostOnly "Not available" 403
        reverse_proxy 127.0.0.1:4580 {
            header_up -X-Relay-Local-Control
            header_up -X-Relay-Instance-Id
            header_up -X-Relay-Server-Id
            header_up -Forwarded
            header_up X-Forwarded-For {remote_host}
            flush_interval -1
        }
    }
}
'@
    $challengePolicy=if($EnableHttpChallenge){'            # TCP 80 explicitly enabled for HTTP certificate validation.'}else{'            disable_http_challenge'}
    $caddyfile=$template.Replace('__PUBLIC_IPV4__',$PublicIPv4).Replace('__HTTP_CHALLENGE_POLICY__',$challengePolicy)+"`n"
    $required=@(
        [pscustomobject]@{Protocol='TCP';ExternalPort=443;InternalPort=443;LocalAddress=$LocalIPv4;Required=$true;Purpose='HTTPS and voice/video signaling'},
        [pscustomobject]@{Protocol='UDP';ExternalPort=4581;InternalPort=4581;LocalAddress=$LocalIPv4;Required=$true;Purpose='Low-latency encrypted voice'},
        [pscustomobject]@{Protocol='TCP';ExternalPort=7881;InternalPort=7881;LocalAddress=$LocalIPv4;Required=$true;Purpose='WebRTC TCP fallback'},
        [pscustomobject]@{Protocol='UDP';ExternalPort=7882;InternalPort=7882;LocalAddress=$LocalIPv4;Required=$true;Purpose='WebRTC camera and screen media'}
    )
    return [pscustomobject]@{
        Version=1
        PublicAddress=$PublicIPv4
        LocalAddress=$LocalIPv4
        RelayUrl="https://$PublicIPv4"
        RtcUrl="wss://$PublicIPv4"
        BackendAddress='127.0.0.1:4580'
        PrivateRtcApiAddress='127.0.0.1:7880'
        RequiredPortMappings=$required
        OptionalPortMappings=@([pscustomobject]@{Protocol='TCP';ExternalPort=80;InternalPort=80;LocalAddress=$LocalIPv4;Required=$false;Enabled=[bool]$EnableHttpChallenge;Purpose='Optional HTTP certificate validation'})
        NeverForwardPorts=@(4580,7880)
        CertificateIssuer='https://acme-v02.api.letsencrypt.org/directory'
        CertificateProfile='shortlived'
        CertificateTermsUrl='https://letsencrypt.org/repository/'
        AcceptedCertificateTerms=[bool]$AcceptedCertificateTerms
        CertificateOperationsAllowed=[bool]$AcceptedCertificateTerms
        Caddyfile=$caddyfile
    }
}
