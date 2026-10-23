"""Bounded same-host TCP fixture. Only its own accepted streams can be cut.

No payload interpretation/logging, public listener, DNS change or packet filter.
The controller independently verifies the exact backend SFU PID/interface first.
"""
import ipaddress
import json
import select
import socket
import threading
import time
import uuid

from dev_profile_core import direct, write_json


class OwnedTurnCut:
    def __init__(self, root, run_id, address, front_port, backend_port):
        self.root = direct(root, False)
        self.run_id = str(uuid.UUID(run_id))
        address = ipaddress.IPv4Address(address)
        if not address.is_private or address.is_loopback or address.is_link_local:
            raise ValueError('An independently verified same-host private interface is required')
        if front_port == backend_port or any(not 1024 <= value <= 65535 for value in (front_port, backend_port)):
            raise ValueError('Separate owned TCP ports required')
        self.destination = (str(address), backend_port)
        self.stopped = threading.Event()
        self.lock = threading.Lock()
        self.rows = []
        self.streams = {}
        self.workers = []
        self.cut_applied = False
        self.error = None
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        self.listener.bind((str(address), front_port))
        self.listener.listen(16)
        self.listener.settimeout(.1)
        self.thread = threading.Thread(target=self._accept, name='owned-turn-tcp-cut')
        self.thread.start()

    def _close_streams(self, cause):
        with self.lock:
            streams = list(self.streams.items())
            for serial, _ in streams:
                self.rows[serial]['closed_by_' + cause] = True
        for _, sockets in streams:
            for stream in sockets:
                try:
                    stream.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                stream.close()
        return len(streams)

    def _cut_if_requested(self):
        if self.cut_applied:
            return
        request = self.root / 'turn-cut-request.json'
        try:
            request.lstat()
        except FileNotFoundError:
            return
        raw = direct(request, False).read_bytes()
        if len(raw) > 1024 or json.loads(raw) != {
            'schema': 1, 'run_id': self.run_id, 'operation': 'close_owned_turn_tcp'
        }:
            raise ValueError('Invalid owned TURN cut request')
        with self.lock:
            self.cut_applied = True
            before = [dict(self.rows[serial]) for serial in self.streams]
        count = self._close_streams('cut')
        reply = {'schema': 1, 'run_id': self.run_id, 'operation': 'close_owned_turn_tcp',
            'owned_connections_closed': count, 'connections_before_cut': before,
            'payloads_or_credentials_logged': False, 'applied_monotonic': time.monotonic()}
        output = self.root / 'turn-cut-applied.json'
        if output.exists():
            raise ValueError('Never replace an earlier cut receipt')
        write_json(output, reply)
        print('OWNED_TURN_TCP_CUT_APPLIED: closed owned connections=' + str(count), flush=True)

    def _accept(self):
        try:
            while not self.stopped.is_set():
                self._cut_if_requested()
                try:
                    client, _ = self.listener.accept()
                except socket.timeout:
                    continue
                except OSError:
                    if self.stopped.is_set():
                        return
                    raise
                with self.lock:
                    full = len(self.streams) >= 32 or len(self.rows) >= 128
                if full:
                    client.close()
                    raise ValueError('Owned TCP fixture connection bound exceeded')
                try:
                    upstream = socket.create_connection(self.destination, timeout=2)
                except OSError:
                    client.close()
                    raise
                for stream in (client, upstream):
                    stream.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                    stream.settimeout(2)
                with self.lock:
                    serial = len(self.rows)
                    self.rows.append({'serial': serial, 'created_after_cut': self.cut_applied,
                        'upstream_bytes': 0, 'downstream_bytes': 0, 'active': True})
                    self.streams[serial] = (client, upstream)
                worker = threading.Thread(target=self._forward, args=(serial, client, upstream),
                    name='owned-turn-tcp-stream')
                self.workers.append(worker)
                worker.start()
        except Exception as error:
            self.error = type(error).__name__

    def _forward(self, serial, client, upstream):
        try:
            while not self.stopped.is_set():
                readable, _, _ = select.select([client, upstream], [], [], .1)
                for origin in readable:
                    payload = origin.recv(65536)
                    if not payload:
                        return
                    destination = upstream if origin is client else client
                    destination.sendall(payload)
                    with self.lock:
                        self.rows[serial]['upstream_bytes' if origin is client else 'downstream_bytes'] += len(payload)
        except (OSError, ValueError):
            # Expected after the explicit cut or native connection cleanup.
            pass
        finally:
            for stream in (client, upstream):
                stream.close()
            with self.lock:
                self.rows[serial]['active'] = False
                self.streams.pop(serial, None)

    def snapshot(self):
        with self.lock:
            rows = [dict(row) for row in self.rows]
            active = len(self.streams)
        return {'schema': 1, 'run_id': self.run_id, 'cut_applied': self.cut_applied,
            'error_type': self.error, 'active_connections': active, 'connections': rows,
            'accepted_after_cut': sum(row['created_after_cut'] for row in rows),
            'payloads_or_addresses_exported': False}

    def close(self):
        self.stopped.set()
        self.listener.close()
        self._close_streams('cleanup')
        deadline = time.monotonic() + 5
        self.thread.join(timeout=max(0, deadline - time.monotonic()))
        for worker in self.workers:
            worker.join(timeout=max(0, deadline - time.monotonic()))
        value = self.snapshot()
        value['all_threads_stopped'] = not self.thread.is_alive() and all(not w.is_alive() for w in self.workers)
        return value
