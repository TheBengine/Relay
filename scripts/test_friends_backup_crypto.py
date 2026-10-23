"""Final-phase stream/authentication fixtures; no SSH or personal Relay state."""
import hashlib
import io
import os
import unittest
from unittest import mock

import friends_backup_crypto as crypto
from cryptography.exceptions import InvalidTag


class EncryptedBackup(unittest.TestCase):
    def receipt(self, raw):
        return {'filename': 'friends-20261021T050000Z-abcdef123456.tar.gz',
                'server_id': 'owned-fixture', 'bytes': len(raw),
                'sha256': hashlib.sha256(raw).hexdigest()}

    def test_streaming_authentication_rejects_tampering_truncation_and_trailing_bytes(self):
        raw = b'generated opaque history\0' * 100000
        # Unit boundary only; the actual DPAPI round trip is a separate Windows
        # fixture below. GCM still authenticates the complete header and stream.
        with mock.patch.object(crypto, 'dpapi', side_effect=lambda data, protect: data):
            output = io.BytesIO()
            crypto.seal(io.BytesIO(raw), output, self.receipt(raw))
            encrypted = output.getvalue()
            self.assertNotIn(raw[:1024], encrypted)
            self.assertEqual(crypto.verify(io.BytesIO(encrypted))['bytes'], len(raw))
            for changed in [encrypted[:-1], encrypted + b'x',
                            encrypted.replace(b'owned-fixture', b'other-fixture', 1),
                            encrypted[:-17] + bytes([encrypted[-17] ^ 1]) + encrypted[-16:]]:
                with self.assertRaises((ValueError, InvalidTag)):
                    crypto.verify(io.BytesIO(changed))

    def test_a_short_or_changed_ssh_stream_cannot_be_accepted(self):
        raw = b'owned generated archive'
        with mock.patch.object(crypto, 'dpapi', side_effect=lambda data, protect: data):
            for changed in [raw[:-1], raw + b'extra', b'X' + raw[1:]]:
                with self.assertRaises(ValueError):
                    crypto.seal(io.BytesIO(changed), io.BytesIO(), self.receipt(raw))

    @unittest.skipUnless(os.name == 'nt', 'Requires Windows current-user DPAPI')
    def test_actual_dpapi_stream_round_trip_uses_no_plaintext_file(self):
        raw = os.urandom(crypto.BLOCK + 37)
        encrypted = io.BytesIO()
        crypto.seal(io.BytesIO(raw), encrypted, self.receipt(raw))
        recovered = io.BytesIO()
        crypto.verify(encrypted, recovered)
        self.assertEqual(recovered.getvalue(), raw)


if __name__ == '__main__':
    unittest.main()
