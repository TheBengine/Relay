"""Windows metrics and private fixture helpers for benchmark-release.py."""
from __future__ import annotations

import argparse
import csv
import ctypes
from ctypes import wintypes
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import socket
import sqlite3
import statistics
import subprocess
import time
from urllib.request import Request, urlopen
import zipfile


MIB = 1024 * 1024


def utc() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(MIB), b""):
            digest.update(chunk)
    return digest.hexdigest()


class MemoryCounters(ctypes.Structure):
    _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD)] + [
        (name, ctypes.c_size_t) for name in (
            "PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage",
            "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage",
            "QuotaNonPagedPoolUsage", "PagefileUsage", "PeakPagefileUsage", "PrivateUsage",
        )
    ]


class WindowsMetrics:
    """Read only one exact owned PID through a retained process handle."""
    def __init__(self, pid: int):
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.psapi = ctypes.WinDLL("psapi", use_last_error=True)
        self.kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        self.kernel.OpenProcess.restype = wintypes.HANDLE
        self.kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        self.kernel.CloseHandle.restype = wintypes.BOOL
        self.kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
        self.kernel.GetProcessTimes.restype = wintypes.BOOL
        self.psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(MemoryCounters), wintypes.DWORD]
        self.psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
        self.kernel.QueryFullProcessImageNameW.argtypes = [wintypes.HANDLE, wintypes.DWORD, wintypes.LPWSTR, ctypes.POINTER(wintypes.DWORD)]
        self.kernel.QueryFullProcessImageNameW.restype = wintypes.BOOL
        self.kernel.GetActiveProcessorCount.argtypes = [wintypes.WORD]
        self.kernel.GetActiveProcessorCount.restype = wintypes.DWORD
        self.logical_processors = int(self.kernel.GetActiveProcessorCount(0xFFFF)) or (os.cpu_count() or 1)
        self.handle = self.kernel.OpenProcess(0x0400 | 0x0010, False, pid)
        if not self.handle:
            raise ctypes.WinError(ctypes.get_last_error())
        self.pid = pid
        name = ctypes.create_unicode_buffer(32768)
        length = wintypes.DWORD(len(name))
        if not self.kernel.QueryFullProcessImageNameW(self.handle, 0, name, ctypes.byref(length)):
            self.close()
            raise ctypes.WinError(ctypes.get_last_error())
        self.image_path = Path(name.value)

    def snapshot(self) -> dict:
        memory = MemoryCounters()
        memory.cb = ctypes.sizeof(memory)
        if not self.psapi.GetProcessMemoryInfo(self.handle, ctypes.byref(memory), memory.cb):
            raise ctypes.WinError(ctypes.get_last_error())
        creation, exit_time, kernel_time, user_time = (wintypes.FILETIME() for _ in range(4))
        if not self.kernel.GetProcessTimes(self.handle, *map(ctypes.byref, (creation, exit_time, kernel_time, user_time))):
            raise ctypes.WinError(ctypes.get_last_error())
        ticks = sum((value.dwHighDateTime << 32) | value.dwLowDateTime for value in (kernel_time, user_time))
        return {"working_set_bytes": int(memory.WorkingSetSize), "private_bytes": int(memory.PrivateUsage), "cpu_seconds": ticks / 10_000_000, "clock": time.perf_counter()}

    def close(self):
        if self.handle:
            self.kernel.CloseHandle(self.handle)
            self.handle = None


def owned_window(pid: int) -> dict | None:
    """Inspect titles only after confirming that the window belongs to our PID."""
    user = ctypes.WinDLL("user32", use_last_error=True)
    callback_type = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    user.EnumWindows.argtypes = [callback_type, wintypes.LPARAM]
    user.EnumWindows.restype = wintypes.BOOL
    user.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
    user.GetWindowThreadProcessId.restype = wintypes.DWORD
    user.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
    user.GetWindowTextW.restype = ctypes.c_int
    user.IsWindowVisible.argtypes = [wintypes.HWND]
    user.IsWindowVisible.restype = wintypes.BOOL
    found = []

    @callback_type
    def visit(hwnd, _):
        window_pid = wintypes.DWORD()
        user.GetWindowThreadProcessId(hwnd, ctypes.byref(window_pid))
        if window_pid.value == pid:
            title = ctypes.create_unicode_buffer(512)
            user.GetWindowTextW(hwnd, title, len(title))
            if title.value == "Relay":
                found.append({"title": title.value, "visible": bool(user.IsWindowVisible(hwnd))})
                return False
        return True

    user.EnumWindows(visit, 0)
    return found[0] if found else None


def api(base: str, method: str, path: str, payload=None, token=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    data = json.dumps(payload).encode() if payload is not None else None
    with urlopen(Request(base + path, data=data, headers=headers, method=method), timeout=8) as response:
        value = response.read()
        return json.loads(value) if value else None


def fixture(base: str) -> dict:
    password = "Isolated_Backend_QA_1234!"
    alex = api(base, "POST", "/api/register", {"username": "Alex", "password": password})
    users = [alex]
    for name in ("Mira", "Jonas"):
        invite = api(base, "POST", "/api/invites", {}, alex["token"])
        users.append(api(base, "POST", "/api/register", {"username": name, "password": password, "invite": invite["code"]}))
    bootstrap = api(base, "GET", "/api/bootstrap", token=alex["token"])
    channel = next(c for c in bootstrap["channels"] if c["name"] == "general")
    api(base, "POST", "/api/channels", {"name": "game-night", "kind": "text", "topic": "Make plans. Find your squad."}, alex["token"])
    api(base, "POST", "/api/channels", {"name": "Squad", "kind": "voice", "topic": "Bring your team."}, alex["token"])
    messages = [
        (0, "Welcome to The Commons. This is our little place to hang out."),
        (1, "Game night at eight? I have a couple of maps we could try."),
        (2, "I am in. I will jump into the Lounge when I get home."),
        (0, "Sounds good. Text channels and voice rooms are ready."),
        (1, "A little game-night energy."),
        (1, "I like having the server on our own PC."),
        (0, "We can keep plans in #game-night and use the Lounge for voice."),
        (0, "I will send an invite to the rest of the group."),
        (1, "Nice. I am bringing the usual squad.\nCan we start with a warm-up round?"),
        (1, "This conversation uses seeded test data for the Relay preview."),
    ]
    # Prepare fixture content within the production account burst limit. Keep
    # expected errors visible; this is deliberate pacing, not a 429 retry loop.
    sent = {index: [] for index in range(len(users))}
    def seed_message(sender, body):
        timestamps = sent[sender]
        if len(timestamps) >= 5:
            time.sleep(max(0, timestamps[-5] + 5.1 - time.monotonic()))
        message = api(base, "POST", "/api/messages", body, users[sender]["token"])
        timestamps.append(time.monotonic())
        return message
    for index, (sender, content) in enumerate(messages):
        message = seed_message(sender, {"channel_id": channel["id"], "content": content})
        if index == 2:
            api(base, "POST", f"/api/messages/{message['id']}/reactions", {"emoji": "+1"}, alex["token"])
    api(base, "PATCH", f"/api/messages/{message['id']}", {"content": messages[-1][1]}, users[1]["token"])
    for sender, recipient, content in [(0, 1, "Hey Mira, want to join us for game night?"), (1, 0, "Definitely. Send me an invite and I will be there at eight.")]:
        seed_message(sender, {"recipient_id": users[recipient]["user"]["id"], "content": content})
    channel_count = len(api(base, "GET", f"/api/messages?channel_id={channel['id']}", token=alex["token"]))
    direct_count = len(api(base, "GET", f"/api/messages?peer_id={users[1]['user']['id']}", token=alex["token"]))
    assert channel_count == 10 and direct_count == 2
    return {"password": password, "alex_id": alex["user"]["id"], "alex_token": alex["token"], "channel_id": channel["id"], "accounts": 3, "channel_messages": channel_count, "direct_messages": direct_count}


def session_count(database: Path, user_id: str) -> int:
    with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=1) as connection:
        return connection.execute("SELECT COUNT(*) FROM sessions WHERE user_id=?", (user_id,)).fetchone()[0]


def stop_owned(process: subprocess.Popen | None):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(8)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(8)


def wait_until(deadline: float, process: subprocess.Popen, run_dir: Path):
    while time.perf_counter() < deadline:
        if (run_dir / "stop").exists():
            raise InterruptedError("Owned benchmark stop file detected")
        if process.poll() is not None:
            raise RuntimeError(f"Owned client PID {process.pid} exited: {process.returncode}")
        time.sleep(min(.1, max(0, deadline - time.perf_counter())))

