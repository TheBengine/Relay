"""Streaming AES-256-GCM backups with a Windows current-user DPAPI key wrapper.

No plaintext file is created during capture or verification. Recovery still
requires this Windows user's DPAPI profile; this is not password-only recovery.
"""
import base64
import ctypes
from ctypes import wintypes
import hashlib
import json
import secrets
import struct

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

MAGIC = b'RelayFriendsBackup\0v1\n'
MAX_BYTES = 1024 ** 3 + 16 * 1024 ** 2
BLOCK = 1024 * 1024


class Blob(ctypes.Structure):
    _fields_ = [('size', wintypes.DWORD), ('data', ctypes.POINTER(ctypes.c_ubyte))]


def dpapi(raw, protect):
    memory = ctypes.create_string_buffer(raw)
    source = Blob(len(raw), ctypes.cast(memory, ctypes.POINTER(ctypes.c_ubyte)))
    output = Blob()
    library = ctypes.WinDLL('crypt32', use_last_error=True)
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    function = library.CryptProtectData if protect else library.CryptUnprotectData
    function.argtypes = [ctypes.POINTER(Blob), ctypes.c_void_p, ctypes.c_void_p,
                         ctypes.c_void_p, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(Blob)]
    function.restype = wintypes.BOOL
    kernel.LocalFree.argtypes = [ctypes.c_void_p]
    kernel.LocalFree.restype = ctypes.c_void_p
    # CRYPTPROTECT_UI_FORBIDDEN: a background transfer must never open a prompt.
    if not function(ctypes.byref(source), None, None, None, None, 1, ctypes.byref(output)):
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        return ctypes.string_at(output.data, output.size)
    finally:
        kernel.LocalFree(output.data)


def seal(source, destination, receipt):
    size = receipt['bytes']
    if type(size) is not int or not 0 < size <= MAX_BYTES:
        raise ValueError('Archive size outside the supported bound')
    key, nonce = secrets.token_bytes(32), secrets.token_bytes(12)
    header = {'format': 'relay-friends-aes256gcm-dpapi-v1',
              'source': receipt['filename'], 'server_id': receipt['server_id'],
              'bytes': size, 'plaintext_sha256': receipt['sha256'],
              'nonce': base64.b64encode(nonce).decode('ascii'),
              'wrapped_key': base64.b64encode(dpapi(key, True)).decode('ascii')}
    encoded = json.dumps(header, sort_keys=True, separators=(',', ':')).encode()
    destination.write(MAGIC + struct.pack('>I', len(encoded)) + encoded)
    encryptor = Cipher(algorithms.AES(key), modes.GCM(nonce)).encryptor()
    encryptor.authenticate_additional_data(MAGIC + encoded)
    hashed, remaining = hashlib.sha256(), size
    while remaining:
        block = source.read(min(BLOCK, remaining))
        if not block:
            raise ValueError('Backup stream ended early')
        remaining -= len(block)
        hashed.update(block)
        destination.write(encryptor.update(block))
    if source.read(1) or hashed.hexdigest() != receipt['sha256']:
        raise ValueError('Export changed or archive hash did not match')
    destination.write(encryptor.finalize())
    destination.write(encryptor.tag)


def verify(source, plaintext=None):
    """Authenticate all bytes, optionally into an unpublished recovery staging file."""
    source.seek(0, 2)
    length = source.tell()
    source.seek(0)
    if source.read(len(MAGIC)) != MAGIC:
        raise ValueError('Unrecognized encrypted backup')
    encoded_size = source.read(4)
    if len(encoded_size) != 4:
        raise ValueError('Truncated backup header')
    header_size = struct.unpack('>I', encoded_size)[0]
    if not 0 < header_size <= 16384:
        raise ValueError('Invalid backup header size')
    encoded = source.read(header_size)
    header = json.loads(encoded)
    size = header.get('bytes')
    if (header.get('format') != 'relay-friends-aes256gcm-dpapi-v1'
            or type(size) is not int or not 0 < size <= MAX_BYTES
            or length != len(MAGIC) + 4 + header_size + size + 16):
        raise ValueError('Invalid encrypted backup length or format')
    nonce = base64.b64decode(header['nonce'], validate=True)
    wrapped = base64.b64decode(header['wrapped_key'], validate=True)
    if len(nonce) != 12 or not 0 < len(wrapped) <= 8192:
        raise ValueError('Invalid backup key metadata')
    key = dpapi(wrapped, False)
    if len(key) != 32:
        raise ValueError('Invalid unwrapped backup key')
    source.seek(-16, 2)
    tag = source.read(16)
    source.seek(len(MAGIC) + 4 + header_size)
    decryptor = Cipher(algorithms.AES(key), modes.GCM(nonce, tag)).decryptor()
    decryptor.authenticate_additional_data(MAGIC + encoded)
    hashed, remaining = hashlib.sha256(), size
    while remaining:
        block = source.read(min(BLOCK, remaining))
        if not block:
            raise ValueError('Truncated ciphertext')
        remaining -= len(block)
        decoded = decryptor.update(block)
        hashed.update(decoded)
        if plaintext is not None:
            plaintext.write(decoded)
    last = decryptor.finalize()  # raises InvalidTag; staging must not be published
    hashed.update(last)
    if plaintext is not None:
        plaintext.write(last)
    if hashed.hexdigest() != header['plaintext_sha256']:
        raise ValueError('Decrypted archive hash mismatch')
    return header
