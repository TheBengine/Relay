"""Pure in-memory publisher regressions; no DPAPI, files, real keys or network.

The deterministic fixture seeds below are PUBLIC TEST MATERIAL, never publisher
keys. The production init command generates new independent keys explicitly.
"""
import copy
import datetime as dt
import importlib.util
from pathlib import Path
import unittest

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

spec = importlib.util.spec_from_file_location('relay_publish_updates', Path(__file__).with_name('publish-updates.py'))
publisher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publisher)


class PublisherTests(unittest.TestCase):
    def setUp(self):
        self.now = dt.datetime(2026, 10, 25, 12, tzinfo=dt.timezone.utc)
        keys = [Ed25519PrivateKey.from_private_bytes(bytes([index]) * 32) for index in range(1, 6)]
        self.roots = keys[:2]
        self.online = dict(zip(publisher.ROLES, keys[2:]))
        self.raw_root = publisher.root_metadata(self.roots, self.online, self.now)
        self.root = publisher.decode(self.raw_root)['signed']
        self.release = {'schema': 1, 'app_id': publisher.APP_ID, 'platform': 'windows-x64',
                        'channel': 'beta', 'version': '0.10.0', 'sequence': '20261025120000',
                        'installer_target': 'windows-x64/beta/Relay-0.10.0-20261025120000-Setup.exe',
                        'installer_sha256': publisher.digest(b'MZ-public-test-bytes'),
                        'installer_bytes': len(b'MZ-public-test-bytes'),
                        'notes': 'Clearer calls.\nExisting accounts stay.', 'handoff': 'in-place-v2'}

    def test_two_root_signatures_and_separate_online_roles(self):
        self.assertEqual(publisher.verify(self.raw_root, self.root, 'root'), self.root)
        ids = list(self.root['roles']['root']['keyids'])
        self.assertEqual(len(set(ids)), 2)
        self.assertEqual(self.root['roles']['root']['threshold'], 2)
        ids += [self.root['roles'][role]['keyids'][0] for role in publisher.ROLES]
        self.assertEqual(len(set(ids)), 5)
        incomplete = publisher.decode(self.raw_root)
        incomplete['signatures'].pop()
        with self.assertRaises(publisher.PublisherError):
            publisher.verify(publisher.encoded(incomplete), self.root, 'root')

    def test_complete_metadata_binds_release_installer_and_versions(self):
        publisher.validate_release(self.release, None)
        release_bytes, metadata = publisher.prepare_metadata(self.root, self.online, self.release, 7, self.now)
        self.assertEqual(publisher.decode(release_bytes)['handoff'], 'in-place-v2')
        values = {role: publisher.verify(metadata[role + '.json'], self.root, role) for role in publisher.ROLES}
        self.assertEqual(values['targets']['targets']['windows-x64/beta/release.json'], publisher.description(release_bytes))
        self.assertEqual(values['targets']['targets'][self.release['installer_target']]['hashes']['sha256'], self.release['installer_sha256'])
        self.assertEqual(values['snapshot']['meta']['targets.json'], publisher.description(metadata['targets.json'], 7))
        self.assertEqual(values['timestamp']['meta']['snapshot.json'], publisher.description(metadata['snapshot.json'], 7))
        self.assertEqual(values['timestamp']['expires'], publisher.expiry(self.now, 30))
        changed = publisher.decode(metadata['targets.json'])
        changed['signed']['targets'][self.release['installer_target']]['hashes']['sha256'] = '0' * 64
        with self.assertRaises(InvalidSignature):
            publisher.verify(publisher.encoded(changed), self.root, 'targets')

    def test_refresh_preserves_release_identity_and_rollback_is_refused(self):
        publisher.validate_release(copy.deepcopy(self.release), self.release)
        for field, value in [('sequence', '20261024120000'), ('version', '0.9.0'),
                             ('installer_sha256', '0' * 64), ('notes', 'Changed'), ('handoff', 'wait-parent-v1')]:
            candidate = {**self.release, field: value}
            with self.subTest(field=field), self.assertRaises(publisher.PublisherError):
                publisher.validate_release(candidate, self.release)
        for value in [True, 0, publisher.MAX_INSTALLER + 1]:
            with self.assertRaises(publisher.PublisherError):
                publisher.validate_release({**self.release, 'installer_bytes': value}, None)

    def test_expiration_and_canonical_subset_fail_closed(self):
        with self.assertRaises(publisher.PublisherError):
            publisher.prepare_metadata(self.root, self.online, self.release, 1, self.now + dt.timedelta(days=300))
        for value in [{'a': 1.5}, {'a': 'line\nbreak'}, {'a': 'non-ASCII é'}]:
            with self.assertRaises(publisher.PublisherError):
                publisher.canonical(value)
        with self.assertRaises(publisher.PublisherError):
            publisher.decode(b'{"version":1,"version":2}')


if __name__ == '__main__':
    unittest.main()
