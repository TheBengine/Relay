"""Test the pure PowerShell public-host plan; optionally exercise Caddy offline.

No public ports, router rules, certificate accounts or certificate requests are
created. --caddy uses only `adapt` and an isolated, TLS-disabled loopback proxy.
"""
from __future__ import annotations

import argparse
import base64
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import stat
import subprocess
import tempfile
import threading
import time


PROJECT = Path(__file__).absolute().parents[1]
POWERSHELL = Path(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")


def require_local_path(path: Path) -> Path:
    """Reject redirecting ancestors before reading an optional executable."""
    path = path.absolute()
    if path.drive.upper() not in ("C:", "D:") or str(path).startswith("\\\\"):
        raise AssertionError("Only a local C: or D: path is permitted")
    for current in [*reversed(path.parents), path]:
        metadata = current.lstat()
        if stat.S_ISLNK(metadata.st_mode) or getattr(metadata, "st_file_attributes", 0) & 0x400:
            raise AssertionError(f"Redirecting path is not permitted: {current}")
    return path


def powershell_checks() -> dict:
    helper = str(require_local_path(PROJECT / "scripts" / "public-host-config.ps1")).replace("'", "''")
    vectors = {
        "valid_public": ["1.1.1.1", "8.8.4.4", "9.9.9.9", "100.63.255.254", "100.128.0.1", "172.15.255.254", "172.32.0.1", "192.0.3.1", "198.17.255.254", "198.20.0.1", "223.255.255.254"],
        "invalid_public": ["0.0.0.0", "0.1.2.3", "10.0.0.1", "100.64.0.1", "100.127.255.255", "127.0.0.1", "127.255.255.255", "169.254.0.1", "172.16.0.1", "172.31.255.255", "192.0.0.1", "192.0.0.9", "192.0.2.1", "192.88.99.1", "192.168.1.1", "198.18.0.1", "198.19.255.255", "198.51.100.1", "203.0.113.1", "224.0.0.1", "239.255.255.255", "240.0.0.1", "255.255.255.255"],
        "valid_local": ["10.0.0.2", "10.255.255.254", "172.16.0.2", "172.31.255.254", "192.168.0.2", "192.168.255.254"],
        "invalid_local": ["1.1.1.1", "100.64.0.2", "127.0.0.1", "169.254.1.2", "172.15.0.2", "172.32.0.2", "192.0.2.2", "224.0.0.2"],
        "malformed": ["", "127.1", "0127.0.0.1", "01.1.1.1", "0x08080808", "134744072", "8.8.8.256", "-8.8.8.8", "+8.8.8.8", "8.8.8.8.", "8.8.8", "8.8.8.8:443", "https://8.8.8.8", "8.8.8.8/32", "8.8.8.8/anything", "8.8.8.8?evil=1", "8.8.8.8#evil", "8.8.8.8\\evil", "8.8.8.8\n{admin :2019}", "8.8.8.8\r\nheader X-Relay-Local-Control evil", "8.8.8.8; start-process calc", "8.8.8.8\"", " 8.8.8.8", "8.8.8.8 ", "8.8.8.8\n", "８.８.８.８", "2001:4860:4860::8888", "::ffff:8.8.8.8"],
    }
    encoded_vectors = base64.b64encode(json.dumps(vectors, ensure_ascii=False).encode()).decode()
    code = r"""
$ErrorActionPreference='Stop'
. '__HELPER__'
if($PSVersionTable.PSVersion.Major -ne 5){throw 'Test must run in Windows PowerShell 5.1'}
$vectors=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__VECTORS__')) | ConvertFrom-Json
$checks=0
function Assert-Rejected {
    param([string]$Public,[string]$Local)
    $rejected=$false
    try { $null=New-RelayPublicHostingPlan -PublicIPv4 $Public -LocalIPv4 $Local } catch { $rejected=$true }
    if(-not $rejected){throw "Invalid IP was accepted: public=$Public local=$Local"}
}
foreach($value in $vectors.valid_public){
    $p=New-RelayPublicHostingPlan -PublicIPv4 $value -LocalIPv4 '192.168.1.20'
    if($p.PublicAddress -cne $value -or $p.RelayUrl -cne "https://$value" -or $p.RtcUrl -cne "wss://$value"){throw 'Public address did not round trip'}
    $checks++
}
foreach($value in $vectors.invalid_public){Assert-Rejected -Public $value -Local '192.168.1.20';$checks++}
foreach($value in $vectors.valid_local){
    $p=New-RelayPublicHostingPlan -PublicIPv4 '8.8.4.4' -LocalIPv4 $value
    if($p.LocalAddress -cne $value){throw 'LAN address did not round trip'}
    $checks++
}
foreach($value in $vectors.invalid_local){Assert-Rejected -Public '8.8.4.4' -Local $value;$checks++}
foreach($value in $vectors.malformed){
    Assert-Rejected -Public $value -Local '192.168.1.20'
    Assert-Rejected -Public '8.8.4.4' -Local $value
    $checks+=2
}
$guardRejected=$false
try {Assert-RelayPublicCertificateTerms} catch {$guardRejected=$true}
if(-not $guardRejected){throw 'Certificate action did not require explicit consent'}
$guardRejected=$false
try {Assert-RelayPublicCertificateTerms -AcceptedCertificateTerms:$false} catch {$guardRejected=$true}
if(-not $guardRejected){throw 'False consent allowed certificate action'}
Assert-RelayPublicCertificateTerms -AcceptedCertificateTerms
$plan=New-RelayPublicHostingPlan -PublicIPv4 '8.8.4.4' -LocalIPv4 '192.168.1.20'
$consented=New-RelayPublicHostingPlan -PublicIPv4 '8.8.4.4' -LocalIPv4 '192.168.1.20' -AcceptedCertificateTerms
$http=New-RelayPublicHostingPlan -PublicIPv4 '8.8.4.4' -LocalIPv4 '192.168.1.20' -EnableHttpChallenge
[pscustomobject]@{IPChecks=$checks;Plan=$plan;Consented=$consented;Http=$http} | ConvertTo-Json -Depth 8 -Compress
""".replace("__HELPER__", helper).replace("__VECTORS__", encoded_vectors)
    encoded = base64.b64encode(code.encode("utf-16-le")).decode()
    result = subprocess.run([str(POWERSHELL), "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", encoded], capture_output=True, text=True, encoding="utf-8", timeout=30)
    if result.returncode:
        raise AssertionError(result.stdout + result.stderr)
    output = json.loads(result.stdout)
    plan = output["Plan"]
    assert plan["AcceptedCertificateTerms"] is False
    assert plan["CertificateOperationsAllowed"] is False
    assert output["Consented"]["AcceptedCertificateTerms"] is True
    assert output["Consented"]["CertificateOperationsAllowed"] is True
    assert plan["BackendAddress"] == "127.0.0.1:4580"
    assert plan["PrivateRtcApiAddress"] == "127.0.0.1:7880"
    mappings = plan["RequiredPortMappings"]
    assert [(p["Protocol"], p["ExternalPort"], p["InternalPort"]) for p in mappings] == [("TCP", 443, 443), ("UDP", 4581, 4581), ("TCP", 7881, 7881), ("UDP", 7882, 7882)]
    assert all(p["LocalAddress"] == "192.168.1.20" and p["Required"] for p in mappings)
    assert plan["NeverForwardPorts"] == [4580, 7880]
    assert [(p["Protocol"], p["ExternalPort"], p["Enabled"], p["Required"]) for p in plan["OptionalPortMappings"]] == [("TCP", 80, False, False)]
    assert output["Http"]["OptionalPortMappings"][0]["Enabled"] is True
    config = plan["Caddyfile"]
    for expected in ("admin off", "auto_https disable_redirects", "protocols h1 h2", "https://8.8.4.4", "dir https://acme-v02.api.letsencrypt.org/directory", "profile shortlived", "disable_http_challenge", "reverse_proxy 127.0.0.1:4580", "flush_interval -1"):
        assert expected in config, expected
    assert "disable_http_challenge" not in output["Http"]["Caddyfile"]
    assert "tls internal" not in config and "trusted_proxies" not in config
    assert config.index("respond @hostOnly") < config.index("reverse_proxy")
    assert config.index("respond @ambiguousPath") < config.index("reverse_proxy")
    for name in ("X-Relay-Local-Control", "X-Relay-Instance-Id", "X-Relay-Server-Id", "Forwarded"):
        assert f"header_up -{name}" in config
    return output


def exercise_caddy(caddy: Path, plan: dict) -> dict:
    caddy = require_local_path(caddy)
    work = require_local_path(PROJECT.parents[1] / "work")
    with tempfile.TemporaryDirectory(prefix="relay-public-host-config-", dir=work) as temporary:
        scratch = Path(temporary)
        require_local_path(scratch)
        assert scratch.parent == work, "Temporary cleanup target escaped the intended work folder"
        config_path = scratch / "Caddyfile"
        config_path.write_text(plan["Caddyfile"], encoding="utf-8")
        # adapt does not provision the TLS app; never use `validate` here.
        adapted = subprocess.run([str(caddy), "adapt", "--config", str(config_path), "--adapter", "caddyfile"], capture_output=True, text=True, timeout=20)
        if adapted.returncode:
            raise AssertionError(adapted.stdout + adapted.stderr)
        config = json.loads(adapted.stdout)
        assert config["admin"]["disabled"] is True
        policy = config["apps"]["tls"]["automation"]["policies"][0]
        assert policy["subjects"] == [plan["PublicAddress"]]
        issuer = policy["issuers"][0]
        assert issuer["module"] == "acme"
        assert issuer["ca"] == plan["CertificateIssuer"]
        assert issuer["profile"] == "shortlived"
        assert issuer["challenges"]["http"]["disabled"] is True

        requests_seen: list[dict] = []

        class Echo(BaseHTTPRequestHandler):
            def do_GET(self):
                received = {"path": self.path, "headers": {name.lower(): value for name, value in self.headers.items()}}
                requests_seen.append(received)
                body = json.dumps(received).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            do_POST = do_GET

            def log_message(self, *_):
                pass

        backend = ThreadingHTTPServer(("127.0.0.1", 0), Echo)
        thread = threading.Thread(target=backend.serve_forever, daemon=True)
        thread.start()
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            proxy_port = reservation.getsockname()[1]
        # Exercise precisely the adapted guards/headers on loopback with TLS
        # entirely removed, so no issuance/account/acceptance can take place.
        del config["apps"]["tls"]
        for server in config["apps"]["http"]["servers"].values():
            server["listen"] = [f"127.0.0.1:{proxy_port}"]
            server.pop("tls_connection_policies", None)
            server["automatic_https"] = {"disable": True}
            server["protocols"] = ["h1"]

        def replace_upstreams(value):
            if isinstance(value, dict):
                if value.get("handler") == "reverse_proxy":
                    value["upstreams"] = [{"dial": f"127.0.0.1:{backend.server_port}"}]
                for child in value.values():
                    replace_upstreams(child)
            elif isinstance(value, list):
                for child in value:
                    replace_upstreams(child)

        replace_upstreams(config)
        loopback_path = scratch / "loopback.json"
        loopback_path.write_text(json.dumps(config), encoding="utf-8")
        env = os.environ.copy()
        env["XDG_DATA_HOME"] = str(scratch / "data")
        env["XDG_CONFIG_HOME"] = str(scratch / "config")
        env["APPDATA"] = str(scratch / "appdata")
        env["LOCALAPPDATA"] = str(scratch / "localappdata")
        log = (scratch / "caddy.log").open("wb")
        process = subprocess.Popen([str(caddy), "run", "--config", str(loopback_path)], stdout=log, stderr=subprocess.STDOUT, env=env, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))

        def request(path: str, headers: dict | None = None, method="GET"):
            connection = http.client.HTTPConnection("127.0.0.1", proxy_port, timeout=2)
            try:
                connection.request(method, path, headers={"Host": plan["PublicAddress"], **(headers or {})})
                response = connection.getresponse()
                return response.status, response.read()
            finally:
                connection.close()

        try:
            deadline = time.monotonic() + 10
            while True:
                try:
                    assert request("/api/messages")[0] == 200
                    break
                except (OSError, AssertionError):
                    if process.poll() is not None or time.monotonic() > deadline:
                        log.flush()
                        raise AssertionError((scratch / "caddy.log").read_text(errors="replace"))
                    time.sleep(0.1)
            paths = ["/api/local", "/api/local/", "/api/local/shutdown", "/api/local/discord-credentials", "/api/local/anything?x=1", "/api/local-management", "/host-secret", "/local-management", "/API/LOCAL/shutdown", "//api//local//shutdown", "/api/%6cocal/shutdown", "/api%2flocal%2fshutdown", "/%61pi/local/shutdown", "/api%252flocal%252fshutdown", "/foo/../api/local/shutdown", "/api\\local\\shutdown", "/api%5clocal%5cshutdown"]
            baseline = len(requests_seen)
            for path in paths:
                status, _ = request(path, {"X-Relay-Local-Control": "fake-test-proof", "Authorization": "Bearer fake-test-proof"}, method="POST")
                assert status == 403, (path, status)
            assert len(requests_seen) == baseline, "Private management request reached the backend"
            status, body = request("/api/messages", {"X-Relay-Local-Control": "fake-test-proof", "X-Relay-Instance-Id": "fake-instance", "X-Relay-Server-Id": "fake-server", "Forwarded": "for=127.0.0.1", "X-Forwarded-For": "8.8.8.8", "Authorization": "Bearer ordinary-test-session"})
            assert status == 200
            headers = json.loads(body)["headers"]
            for name in ("x-relay-local-control", "x-relay-instance-id", "x-relay-server-id", "forwarded"):
                assert name not in headers, name
            assert headers["x-forwarded-for"] == "127.0.0.1"
            assert headers["authorization"] == "Bearer ordinary-test-session"
            return {"offline_adapt": True, "loopback_private_paths_blocked": len(paths), "loopback_local_proof_headers_removed": True, "certificate_operations": False}
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            log.close()
            backend.shutdown()
            backend.server_close()
            thread.join(timeout=3)


def launch_preflight_checks(caddy: Path | None) -> dict:
    """All cases must fail before launch-stack can create any data or child."""
    work = require_local_path(PROJECT.parents[1] / "work")
    with tempfile.TemporaryDirectory(prefix="relay-public-preflight-", dir=work) as temporary:
        scratch = require_local_path(Path(temporary))
        assert scratch.parent == work
        data = scratch / "must-not-be-created"
        script = require_local_path(PROJECT / "scripts" / "launch-stack.ps1")
        common = [str(POWERSHELL), "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(script), "-DataDirectory", str(data)]
        public = ["-PublicIPv4", "8.8.4.4", "-LocalIPv4", "192.168.1.20", "-RouterWanIPv4", "8.8.4.4"]
        cases = [
            (public, "explicit acceptance"),
            (["-PublicIPv4", "8.8.4.4", "-LocalIPv4", "192.168.1.20", "-RouterWanIPv4", "100.127.25.100", "-AcceptedCertificateTerms"], "globally routable"),
            (["-PublicIPv4", "8.8.4.4", "-LocalIPv4", "192.168.1.20", "-RouterWanIPv4", "1.1.1.1", "-AcceptedCertificateTerms"], "differs"),
            (["-PublicIPv4", "192.168.1.20", "-LocalIPv4", "192.168.1.20", "-RouterWanIPv4", "192.168.1.20", "-AcceptedCertificateTerms"], "globally routable"),
            (public + ["-AcceptedCertificateTerms", "-Port", "4582"], "fixed documented"),
            (["-PublicIPv4", "8.8.4.4"], "requires PublicIPv4"),
            (public + ["-AcceptedCertificateTerms", "-CaddyBinary", str(scratch / "no-caddy.exe")], "HTTPS helper is missing"),
            (["-TailscaleIPv4", "8.8.8.8"], "Tailscale IPv4 address"),
            (["-TailscaleIPv4", "100.63.255.254"], "Tailscale IPv4 address"),
            (["-TailscaleIPv4", "100.128.0.1"], "Tailscale IPv4 address"),
            (["-TailscaleIPv4", "100.100.5.6\n"], "without a URL"),
            (public + ["-TailscaleIPv4", "100.100.5.6"], "either public hosting or private Tailscale"),
        ]
        if caddy:
            cases.append((public + ["-AcceptedCertificateTerms", "-CaddyBinary", str(require_local_path(caddy))], "owner locally"))
        for parameters, expected in cases:
            result = subprocess.run(common + parameters, capture_output=True, text=True, timeout=15)
            assert result.returncode != 0, "Invalid public hosting preflight succeeded"
            assert expected in result.stdout + result.stderr, (expected, result.stdout, result.stderr)
            assert not data.exists(), "Failed public preflight created the server data folder"
        return {"public_launch_preflight_cases": len(cases), "cgnat_launch_rejected_before_side_effects": True, "preflight_created_data_or_children": False}


def tailscale_pure_checks() -> dict:
    """Evaluate real pure helper/config source without reading the user's VPN."""
    helper = str(require_local_path(PROJECT / "scripts" / "public-host-config.ps1")).replace("'", "''")
    launcher = str(require_local_path(PROJECT / "scripts" / "launch-stack.ps1")).replace("'", "''")
    code = r"""
$ErrorActionPreference='Stop'
. '__HELPER__'
$errors=$null;$tokens=$null
$ast=[Management.Automation.Language.Parser]::ParseFile('__LAUNCHER__',[ref]$tokens,[ref]$errors)
if($errors){throw 'Production launcher syntax is invalid'}
foreach($name in @('New-RelayTailscaleHostingPlan','Read-RelayBoundedProcessOutput','Read-RelayTailscaleHostingStatus')){
    $fn=$ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name},$true)
    if(-not $fn){throw 'Tailscale preflight helper is missing'}
    Invoke-Expression $fn.Extent.Text
}
$connected=[pscustomobject]@{BackendState='Running';TailscaleIPs=@('100.100.5.6','fd7a:115c:a1e0::1')}
$plan=New-RelayTailscaleHostingPlan -Address '100.100.5.6' -Status $connected -Port 49123
if($plan.RelayUrl -cne 'http://100.100.5.6:49123' -or $plan.RtcUrl -cne 'ws://100.100.5.6:49123'){throw 'Private invitation/RTC address changed'}
if($plan.RequiresCertificate -or $plan.RequiresRouterForwarding){throw 'Private Tailscale mode acquired public requirements'}
$checks=0
foreach($status in @(
    [pscustomobject]@{BackendState='NeedsLogin';TailscaleIPs=@('100.100.5.6')},
    [pscustomobject]@{BackendState='Stopped';TailscaleIPs=@('100.100.5.6')},
    [pscustomobject]@{BackendState='Running';TailscaleIPs=@('100.100.5.7')},
    [pscustomobject]@{BackendState='Running';TailscaleIPs=@();Peer=@{Someone=[pscustomobject]@{TailscaleIPs=@('100.100.5.6')}}},
    [pscustomobject]@{BackendState='Running';Self=[pscustomobject]@{TailscaleIPs=@('100.100.5.6')}},
    [pscustomobject]@{BackendState='Running';TailscaleIPs=@('100.100.5.6','100.100.5.6')}
)){
    $rejected=$false
    try{$null=New-RelayTailscaleHostingPlan -Address '100.100.5.6' -Status $status}catch{$rejected=$true}
    if(-not $rejected){throw 'Missing/login/mismatched Tailscale status was accepted'}
    $checks++
}
foreach($address in @('100.63.255.254','100.128.0.1','127.0.0.1','192.168.1.20','8.8.8.8','100.100.5.6:443','100.100.5.6/32',"100.100.5.6`n")){
    $rejected=$false
    try{$null=New-RelayTailscaleHostingPlan -Address $address -Status $connected}catch{$rejected=$true}
    if(-not $rejected){throw 'Invalid private Tailscale IP was accepted'}
    $checks++
}
# Missing-installation branch uses mocks in this isolated shell only; it never
# looks up, executes or alters the user's real Tailscale installation.
function Assert-RelayHostingLocalPath([string]$Path){return $Path}
function Test-Path {param([string]$LiteralPath,[string]$PathType);return $false}
$missingRejected=$false
try{$null=Read-RelayTailscaleHostingStatus}catch{$missingRejected=$_.Exception.Message -like 'Install Tailscale*'}
if(-not $missingRejected){throw 'Missing Tailscale installation was accepted'}
Remove-Item Function:\Test-Path
$tailscaleMode=$true;$tailscalePlan=$plan;$publicMode=$false
$SfuPort=49124;$SfuTcpPort=49125;$SfuUdpPort=49126;$key='fixture';$secret='fixture-no-real-secret'
foreach($name in @('nodeIp','mediaFilter','config')){
    $assignment=$ast.Find({param($node) $node -is [Management.Automation.Language.AssignmentStatementAst] -and $node.Left -is [Management.Automation.Language.VariableExpressionAst] -and $node.Left.VariablePath.UserPath -ceq $name},$true)
    if(-not $assignment){throw 'Production media configuration assignment missing'}
    Invoke-Expression $assignment.Extent.Text
}
$stackAssignment=$ast.Find({param($node) $node -is [Management.Automation.Language.AssignmentStatementAst] -and $node.Left -is [Management.Automation.Language.VariableExpressionAst] -and $node.Left.VariablePath.UserPath -ceq 'stackRecord'},$true)
if(-not $stackAssignment -or $stackAssignment.Extent.Text -notmatch 'server_id=\$health.server_id' -or $stackAssignment.Extent.Text -notmatch 'instance_id=\$health.instance_id'){throw 'Stack record is not bound to server and instance identity'}
# Real, owned pipe fixtures exercise the production reader. No Tailscale CLI,
# user host or external process is queried. Both pipes are filled together.
$boundedChecks=0
foreach($case in @(
    @{OutBytes=10;ErrBytes=10;Reject=$false},
    @{OutBytes=66560;ErrBytes=66560;Reject=$false},
    @{OutBytes=66561;ErrBytes=10;Reject=$true},
    @{OutBytes=10;ErrBytes=66561;Reject=$true}
)){
    $childCode='$o=[byte[]]::new('+ $case.OutBytes +');$e=[byte[]]::new('+ $case.ErrBytes +');[Console]::OpenStandardOutput().Write($o,0,$o.Length);[Console]::OpenStandardError().Write($e,0,$e.Length)'
    $encodedChild=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($childCode))
    $start=[Diagnostics.ProcessStartInfo]::new()
    $start.FileName='C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe'
    $start.Arguments='-NoLogo -NoProfile -NonInteractive -EncodedCommand '+$encodedChild
    $start.UseShellExecute=$false;$start.CreateNoWindow=$true
    $start.RedirectStandardOutput=$true;$start.RedirectStandardError=$true
    $fixture=[Diagnostics.Process]::new();$fixture.StartInfo=$start
    $rejected=$false
    try{
        if(-not $fixture.Start()){throw 'Owned pipe fixture did not start'}
        try{$captured=Read-RelayBoundedProcessOutput -Process $fixture}catch{
            if($_.Exception.Message -notlike '*65 KiB output limit*'){throw}
            $rejected=$true
        }
        if($rejected -ne $case.Reject){throw 'Byte limit fixture did not enforce the exact65KiB bound'}
        if(-not $rejected){
            if([Text.Encoding]::UTF8.GetByteCount($captured.Stdout) -ne $case.OutBytes -or [Text.Encoding]::UTF8.GetByteCount($captured.Stderr) -ne $case.ErrBytes){throw 'Bounded reader lost pipe data'}
        }
        $boundedChecks++
    }finally{
        if(-not $fixture.HasExited){$fixture.Kill();$null=$fixture.WaitForExit(2000)}
        $fixture.Dispose()
    }
}
[pscustomobject]@{RejectedStatusesAndIPs=$checks;MissingInstallation=$missingRejected;Plan=$plan;LivekitConfig=$config;BoundedPipeCases=$boundedChecks;StackIdentityBinding=$true} | ConvertTo-Json -Depth 5 -Compress
""".replace("__HELPER__", helper).replace("__LAUNCHER__", launcher)
    encoded = base64.b64encode(code.encode("utf-16-le")).decode()
    result = subprocess.run([str(POWERSHELL), "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", encoded], capture_output=True, text=True, encoding="utf-8", timeout=20)
    if result.returncode:
        raise AssertionError(result.stdout + result.stderr)
    output = json.loads(result.stdout)
    assert output["MissingInstallation"]
    assert output["Plan"]["MediaIpIncludes"] == ["100.100.5.6/32", "127.0.0.0/8"]
    config = output["LivekitConfig"]
    assert 'bind_addresses: ["127.0.0.1"]' in config
    assert "node_ip: 100.100.5.6" in config
    assert "enable_loopback_candidate: true" in config
    assert "ips:\n    includes:\n      - 100.100.5.6/32\n      - 127.0.0.0/8\nkeys:" in config.replace("\r\n", "\n")
    assert "0.0.0.0/0" not in config and "192.168." not in config
    assert output["StackIdentityBinding"]
    return {"tailscale_pure_rejection_cases": output["RejectedStatusesAndIPs"] + 1, "tailscale_own_node_match": True, "tailscale_no_public_certificate_requirements": True, "tailscale_media_ip_filter_preserves_loopback": True, "bounded_pipe_cases": output["BoundedPipeCases"], "stack_server_instance_identity_bound": True, "user_tailscale_queried_or_changed": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--caddy", type=Path, help="Optional already-installed local Caddy executable (offline/loopback tests only)")
    args = parser.parse_args()
    output = powershell_checks()
    result = {"windows_powershell_51": True, "ip_validation_cases": output["IPChecks"], "certificate_terms_guard": True, "exact_port_plan": True, "config_template": True}
    result.update(launch_preflight_checks(args.caddy))
    result.update(tailscale_pure_checks())
    if args.caddy:
        result.update(exercise_caddy(args.caddy, output["Plan"]))
    else:
        result["caddy_runtime_checks"] = "not run; pass --caddy for offline adapt and isolated loopback checks"
    print(json.dumps(result))


if __name__ == "__main__":
    main()
