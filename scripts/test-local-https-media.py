"""Own loopback TLS/Relay/WebSocket fixture; no devices, live users or external host.

Voice payloads are synthetic opaque packets, not a listening/codec quality test.
The RTC peer is a signalling simulator, not a LiveKit/video-acceptance claim.
"""
import argparse
import base64
import contextlib
import datetime
import hashlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import secrets
import select
import socket
import socketserver
import ssl
import statistics
import struct
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.x509.oid import NameOID
from dev_profile_core import direct


def unused(kind=socket.SOCK_STREAM):
    with socket.socket(socket.AF_INET, kind) as endpoint:
        endpoint.bind(('127.0.0.1', 0))
        return endpoint.getsockname()[1]


class Frames:
    def send(self, opcode, payload):
        mask = os.urandom(4)
        size = len(payload)
        header = bytes([0x80 | opcode, 0x80 | size]) if size < 126 else bytes([0x80 | opcode, 0xfe]) + struct.pack('!H', size)
        self.socket.sendall(header + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def frame(self, seconds=2):
        deadline = time.monotonic() + seconds
        while True:
            if len(self.buffer) >= 2:
                opcode, size, offset = self.buffer[0] & 15, self.buffer[1] & 127, 2
                if size == 126:
                    size = struct.unpack('!H', self.buffer[2:4])[0] if len(self.buffer) >= 4 else None
                    offset = 4
                if size is not None and len(self.buffer) >= offset + size:
                    assert not self.buffer[1] & 128 and size < 65536
                    data = bytes(self.buffer[offset:offset + size]); del self.buffer[:offset + size]
                    if opcode == 9:
                        self.send(10, data)
                        continue
                    return opcode, data
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not self.socket.pending() and not select.select([self.socket], [], [], remaining)[0]:
                return None
            received = self.socket.recv(65536)
            if not received:
                return 8, b''
            self.buffer.extend(received)

    def close(self):
        with contextlib.suppress(OSError):
            self.socket.shutdown(socket.SHUT_RDWR)
        self.socket.close()


class Peer(Frames):
    def __init__(self, port, path, context, headers=None, expected=101):
        plain = socket.create_connection(('127.0.0.1', port), timeout=4)
        plain.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.socket = context.wrap_socket(plain, server_hostname='localhost')
        self.buffer = bytearray()
        key = base64.b64encode(os.urandom(16)).decode()
        extra = ''.join(name + ': ' + value + '\r\n' for name, value in (headers or {}).items())
        self.socket.sendall((f'GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n{extra}\r\n').encode())
        while b'\r\n\r\n' not in self.buffer:
            raw = self.socket.recv(4096); assert raw, 'Handshake closed early'; self.buffer.extend(raw)
        head, remaining = bytes(self.buffer).split(b'\r\n\r\n', 1); self.buffer = bytearray(remaining)
        assert int(head.split()[1]) == expected, f'Fixture WebSocket status {int(head.split()[1])}, expected {expected}'
        if expected != 101:
            self.close()
        else:
            accept = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
            assert accept in head


class Proxy(socketserver.ThreadingTCPServer):
    allow_reuse_address = False
    daemon_threads = True
    def handle_error(self, request, client_address):
        pass  # Expected failed TLS verification contains no application request.


def certificate(root):
    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, 'Relay local QA only')])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (x509.CertificateBuilder().subject_name(name).issuer_name(name).public_key(key.public_key())
            .serial_number(x509.random_serial_number()).not_valid_before(now - datetime.timedelta(minutes=1))
            .not_valid_after(now + datetime.timedelta(hours=2))
            .add_extension(x509.SubjectAlternativeName([x509.DNSName('localhost')]), critical=False)
            .add_extension(x509.BasicConstraints(ca=True, path_length=0), critical=True).sign(key, hashes.SHA256()))
    cert_path, key_path = direct(root / 'qa-cert.pem'), direct(root / 'qa-key.pem')
    cert_path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
    key_path.write_bytes(key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
    return cert_path, key_path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--server', required=True)
    parser.add_argument('--work-root', required=True)
    args = parser.parse_args()
    assert os.name == 'nt', 'This local-path fixture is for Windows'
    binary = direct(args.server, False)
    work = direct(args.work_root, False)
    root = direct(work / ('https-media-' + uuid.uuid4().hex)); root.mkdir()
    port, udp = unused(), unused(socket.SOCK_DGRAM)
    cert, key = certificate(root)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); tls.load_cert_chain(cert, key)
    trust = ssl.create_default_context(cafile=str(cert))
    upstream_requests = []

    class Signal(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'
        def log_message(self, *args):
            pass
        def do_POST(self):
            self.rfile.read(int(self.headers.get('Content-Length', '0')))
            self.send_response(200); self.send_header('Content-Length', '2'); self.end_headers(); self.wfile.write(b'{}')
        def do_GET(self):
            assert self.headers.get('Authorization', '').startswith('Bearer ')
            upstream_requests.append(True)
            value = base64.b64encode(hashlib.sha1((self.headers['Sec-WebSocket-Key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
            self.send_response(101); self.send_header('Upgrade', 'websocket'); self.send_header('Connection', 'Upgrade'); self.send_header('Sec-WebSocket-Accept', value); self.end_headers()
            self.connection.settimeout(6)
            try:
                while True:
                    head = self.rfile.read(2)
                    if len(head) != 2: break
                    opcode, size = head[0] & 15, head[1] & 127
                    if size == 126: size = struct.unpack('!H', self.rfile.read(2))[0]
                    assert size < 65536 and head[1] & 128
                    mask = self.rfile.read(4); data = self.rfile.read(size)
                    payload = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
                    if opcode == 8: break
                    response = 10 if opcode == 9 else opcode
                    frame = bytes([0x80 | response, size]) if size < 126 else bytes([0x80 | response, 126]) + struct.pack('!H', size)
                    self.wfile.write(frame + payload); self.wfile.flush()
            except (OSError, ValueError):
                pass

    class Forward(socketserver.BaseRequestHandler):
        def handle(self):
            try:
                with tls.wrap_socket(self.request, server_side=True) as front, socket.create_connection(('127.0.0.1', port), timeout=3) as back:
                    front.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1); back.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                    while True:
                        ready = [front] if front.pending() else select.select([front, back], [], [], 8)[0]
                        if not ready: break
                        for source in ready:
                            data = source.recv(65536)
                            if not data: return
                            (back if source is front else front).sendall(data)
            except OSError:
                pass

    upstream = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Signal)
    proxy = Proxy(('127.0.0.1', 0), Forward)
    for service in [upstream, proxy]: threading.Thread(target=service.serve_forever, daemon=True).start()
    public_port = proxy.server_address[1]
    base = f'https://localhost:{public_port}'
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=trust))
    checks = []
    peers = []
    report = {'passed': False, 'checks': checks, 'fixture': str(root), 'scope': 'loopback TLS; synthetic packets and RTC signalling simulator; no devices, WAN, codec/listening or LiveKit video acceptance'}

    def api(method, path, token=None, body=None, expected=200, headers=None):
        headers = dict(headers or {})
        if token: headers['Authorization'] = 'Bearer ' + token
        raw = None
        if body is not None: raw = json.dumps(body).encode(); headers['Content-Type'] = 'application/json'
        try:
            response = opener.open(urllib.request.Request(base + path, data=raw, headers=headers, method=method), timeout=5)
        except urllib.error.HTTPError as error: response = error
        with response:
            assert response.status == expected, f'Fixture {method} {path.split("?")[0]} status {response.status}, expected {expected}'
            data = response.read(131072)
            return json.loads(data) if data else None

    environment = os.environ.copy()
    for name in list(environment):
        if name.startswith('RELAY_'): del environment[name]
    environment.update(RELAY_RTC_URL=base.replace('https:', 'wss:'), RELAY_RTC_API_URL='http://127.0.0.1:' + str(upstream.server_address[1]), RELAY_RTC_API_KEY='synthetic-qa-key', RELAY_RTC_API_SECRET=secrets.token_urlsafe(32))
    log = direct(root / 'server.log').open('wb')
    process = subprocess.Popen([str(binary), '--bind', '127.0.0.1', '--udp-bind', '127.0.0.1', '--port', str(port), '--udp-port', str(udp), '--data', str(direct(root / 'data')), '--name', 'Local HTTPS QA'], env=environment, stdout=log, stderr=log, creationflags=subprocess.CREATE_NO_WINDOW)
    try:
        for _ in range(100):
            assert process.poll() is None, 'Owned fixture server exited'
            try:
                if api('GET', '/api/health')['ok']: break
            except (OSError, ValueError, AssertionError): time.sleep(.05)
        else: raise AssertionError('Own HTTPS fixture did not become healthy')
        api('GET', '/api/health', headers={'Host': 'evil.example'}, expected=421)
        default_tls = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=ssl.create_default_context()))
        try: default_tls.open(base + '/api/health', timeout=3)
        except urllib.error.URLError as error:
            assert isinstance(error.reason, ssl.SSLCertVerificationError)
        else: raise AssertionError('Untrusted fixture certificate accepted')
        checks.append('Actual HTTPS health, configured certificate verification and forged Host rejection; no system certificate store change')
        users = []
        for name in ['QaAlice', 'QaBob', 'QaOtherRoom']:
            body = {'username': name, 'password': secrets.token_urlsafe(24)}
            if users: body['invite'] = api('POST', '/api/invites', users[0]['token'], {})['code']
            users.append(api('POST', '/api/register', body=body))
        tokens = [u['token'] for u in users]
        bootstrap = api('GET', '/api/bootstrap', tokens[0])
        room = next(c['id'] for c in bootstrap['channels'] if c['kind'] == 'voice')
        other = api('POST', '/api/channels', tokens[0], {'name': 'isolated-qa', 'kind': 'voice'})['id']
        tickets = [api('POST', '/api/voice/join', token, {'channel_id': other if i == 2 else room}) for i, token in enumerate(tokens)]

        def voice(ticket, expected=101):
            peer = Peer(public_port, '/api/voice/tunnel', trust, {'X-Relay-Voice-Session': str(ticket['session_id']), 'X-Relay-Voice-Key': ticket['key']}, expected)
            if expected == 101: peers.append(peer)
            return peer
        def packet(ticket, sequence, payload):
            header = struct.pack('<QQ', ticket['session_id'], sequence)
            return header + ChaCha20Poly1305(base64.b64decode(ticket['key'])).encrypt(struct.pack('<IQ', 0, sequence), payload, header)
        def clear(ticket, frame):
            assert frame and frame[0] == 2
            header, data = frame[1][:16], frame[1][16:]
            seat, sequence = struct.unpack('<QQ', header); assert seat == ticket['session_id']
            return sequence, ChaCha20Poly1305(base64.b64decode(ticket['key'])).decrypt(struct.pack('<IQ', 1, sequence), data, header)
        sockets = [voice(ticket) for ticket in tickets]
        Peer(public_port, '/api/voice/tunnel', trust, expected=401)
        for peer, ticket in zip(sockets, tickets):
            peer.send(2, packet(ticket, 0, b'\0')); peer.send(9, b'qa-ready'); assert peer.frame() == (10, b'qa-ready')
        durations = []
        for index in range(32):
            payload = b'\x01' + struct.pack('<Q', 0) + b'QA-opaque-media-' + struct.pack('<I', index)
            sent = packet(tickets[0], index + 1, payload)
            began = time.perf_counter(); sockets[0].send(2, sent)
            last_sequence, received = clear(tickets[1], sockets[1].frame())
            durations.append((time.perf_counter() - began) * 1000)
            assert received[1:9] == struct.pack('<Q', tickets[0]['session_id']) and received[9:] == payload[9:]
            time.sleep(.01)
        assert sockets[2].frame(.15) is None
        sockets[0].send(2, sent); assert sockets[1].frame(.15) is None
        api('POST', '/api/voice/state', tokens[0], {'muted': True, 'deafened': False, 'sharing': False, 'camera': False}, expected=204)
        sockets[0].send(2, packet(tickets[0], 33, payload)); assert sockets[1].frame(.15) is None
        api('POST', '/api/voice/state', tokens[0], {'muted': False, 'deafened': False, 'sharing': False, 'camera': False}, expected=204)
        sockets[1].close(); sockets[1] = voice(tickets[1]); sockets[1].send(2, packet(tickets[1], 1, b'\0')); sockets[1].send(9, b'ready'); assert sockets[1].frame() == (10, b'ready')
        sockets[0].send(2, packet(tickets[0], 34, payload)); resumed_sequence, received = clear(tickets[1], sockets[1].frame()); assert resumed_sequence > last_sequence and received[9:] == payload[9:]
        checks.append('32 authenticated synthetic media packets through real WSS; replay, mute and other-room isolation enforced; reconnect preserves nonce progression')
        rtc = api('POST', '/api/rtc/token', tokens[0], {'channel_id': room})
        signal = Peer(public_port, '/rtc?access_token=' + urllib.parse.quote(rtc['token']), trust); peers.append(signal)
        signal.send(2, b'QA synthetic RTC signalling'); assert signal.frame() == (2, b'QA synthetic RTC signalling')
        count = len(upstream_requests)
        Peer(public_port, '/rtc?access_token=invalid', trust, expected=401); assert len(upstream_requests) == count
        api('POST', '/api/logout', tokens[0], expected=204)
        closed = signal.frame(4); assert closed and closed[0] == 8
        checks.append('Real TLS -> Relay RTC proxy -> owned signalling peer; invalid JWT rejected before upstream; logout closes active signalling')
        api('POST', '/api/voice/leave', tokens[1], expected=204); voice(tickets[1], expected=401)
        report.update(passed=True, synthetic_packets=32, local_packet_delivery_median_ms=round(statistics.median(durations), 3), local_packet_delivery_max_ms=round(max(durations), 3), listening_quality_test=False, binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    finally:
        for peer in peers:
            with contextlib.suppress(OSError): peer.close()
        try:
            subprocess.run([str(binary), '--data', str(root / 'data'), '--stop'], env=environment, stdout=log, stderr=log, timeout=10, creationflags=subprocess.CREATE_NO_WINDOW)
            process.wait(timeout=12)
        finally:
            if process.poll() is None: process.terminate(); process.wait(timeout=5)
            log.close()
            for service in [proxy, upstream]: service.shutdown(); service.server_close()
            report['owned_server_stopped'] = process.poll() is not None
            direct(root / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
