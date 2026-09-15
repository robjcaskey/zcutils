"""Independent-worker comparison must inspect artifacts, not only receipts."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('compare', Path(__file__).with_name('fips-compare-builds.py'))
compare = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compare)


class IndependentBuildTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for mode, instance in (('a', 'i-first'), ('b', 'i-second')):
            root = self.root / mode
            (root / 'bin').mkdir(parents=True)
            for name in compare.repro.BINARIES:
                (root / 'bin' / name).write_bytes(name.encode())
            (root / 'provider/lib').mkdir(parents=True)
            for name in ('bcm.o', 'libcrypto.a'):
                (root / 'provider/lib' / name).write_bytes(name.encode())
            compare.repro.write_json(root / 'provider/share/zcutils/fips/provider-receipt.json', {
                'network_interfaces': ['lo'], 'source': 'same', 'tools': 'same', 'commands': 'same',
                'artifacts': {name: {'sha256': compare.repro.sha256(root / 'provider/lib' / name)}
                              for name in ('bcm.o', 'libcrypto.a')},
            })
            compare.repro.write_json(root / 'evidence/offline-build.json', {
                'mode': 'offline', 'clean_target': True, 'network_interfaces': ['lo'],
                'artifacts': compare.repro.manifest(root / 'bin'), 'inputs': {'source_sha256': 'same'},
                'cargo_lock_sha256': 'same',
                'provider_libcrypto_sha256': compare.repro.sha256(root / 'provider/lib/libcrypto.a'),
            })
            compare.repro.write_json(root / 'launch.json', {'instance_id': instance})
        self.args = (self.root / 'a', self.root / 'b', self.root / 'a/launch.json',
                     self.root / 'b/launch.json', self.root / 'result.json')

    def test_matching_distinct_workers(self):
        compare.compare_builds(*self.args)
        self.assertEqual(json.loads((self.root / 'result.json').read_text())['status'], 'identical')

    def test_same_worker_is_not_independent(self):
        compare.repro.write_json(self.root / 'b/launch.json', {'instance_id': 'i-first'})
        with self.assertRaisesRegex(ValueError, 'distinct'):
            compare.compare_builds(*self.args)

    def test_modified_executable_fails_and_removes_stale_success(self):
        compare.compare_builds(*self.args)
        (self.root / 'b/bin/zcblock-csi').write_bytes(b'different')
        with self.assertRaisesRegex(ValueError, 'differs from offline'):
            compare.compare_builds(*self.args)
        self.assertFalse((self.root / 'result.json').exists())

    def test_modified_native_module_fails(self):
        (self.root / 'b/provider/lib/bcm.o').write_bytes(b'different')
        with self.assertRaisesRegex(ValueError, 'differs from its build receipt'):
            compare.compare_builds(*self.args)


if __name__ == '__main__':
    unittest.main()
