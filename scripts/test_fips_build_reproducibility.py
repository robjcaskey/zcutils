"""The release gate must reject mismatches, stale records, and missing binaries."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location('repro', Path(__file__).with_name('fips-build-reproducibility.py'))
repro = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(repro)


class ReproducibilityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for mode in ('offline', 'online'):
            root = self.root / mode
            (root / 'bin').mkdir(parents=True)
            for name in repro.BINARIES:
                (root / 'bin' / name).write_bytes(('binary:' + name).encode())
            repro.write_json(root / 'build.json', {
                'mode': mode, 'clean_target': True, 'inputs': {'source_sha256': 'c' * 64},
                'network_interfaces': ['lo'] if mode == 'offline' else ['lo', 'eth0'],
                'artifacts': repro.manifest(root / 'bin'),
                'cargo_lock_sha256': 'a' * 64, 'provider_libcrypto_sha256': 'b' * 64,
            })

    def compare(self):
        repro.compare(self.root / 'offline', self.root / 'online', self.root / 'result.json')

    def test_matching_complete_outputs_select_offline(self):
        self.compare()
        result = json.loads((self.root / 'result.json').read_text())
        self.assertEqual(result['selected_artifacts'], 'offline')
        self.assertEqual(set(result['builds']['offline']['artifacts']), set(repro.BINARIES))

    def test_one_changed_binary_fails_even_with_updated_record(self):
        root = self.root / 'online'
        (root / 'bin/zcblock-csi').write_bytes(b'different')
        record = json.loads((root / 'build.json').read_text())
        record['artifacts'] = repro.manifest(root / 'bin')
        repro.write_json(root / 'build.json', record)
        with self.assertRaisesRegex(ValueError, 'mismatch'):
            self.compare()
        self.assertFalse((self.root / 'result.json').exists())

    def test_tampered_binary_fails_without_updating_record(self):
        (self.root / 'offline/bin/zcblock-csi').write_bytes(b'changed')
        with self.assertRaisesRegex(ValueError, 'record does not match'):
            self.compare()

    def test_missing_or_extra_binary_fails(self):
        (self.root / 'offline/bin/zcblock-csi').unlink()
        with self.assertRaisesRegex(ValueError, 'missing or unexpected'):
            self.compare()

    def test_networked_offline_build_fails(self):
        path = self.root / 'offline/build.json'
        record = json.loads(path.read_text())
        record['network_interfaces'].append('eth0')
        repro.write_json(path, record)
        with self.assertRaisesRegex(ValueError, 'offline build had networking'):
            self.compare()
        with mock.patch.object(repro, 'network_interfaces', return_value=['lo', 'eth0']):
            with self.assertRaisesRegex(ValueError, 'non-loopback'):
                repro.require_offline()

    def test_build_rejects_existing_compiler_outputs(self):
        with mock.patch.object(repro, 'require_offline', return_value=['lo']):
            with self.assertRaisesRegex(ValueError, 'empty target'):
                repro.build('offline', self.root / 'offline', 1)

    def test_symlinked_artifact_fails(self):
        path = self.root / 'offline/bin/zcblock-csi'
        path.unlink()
        path.symlink_to(self.root / 'online/bin/zcblock-csi')
        with self.assertRaisesRegex(ValueError, 'symlink'):
            self.compare()

    def test_offline_publication_rejects_changed_payload(self):
        repro.verify(self.root / 'offline/bin', self.root / 'offline/build.json')
        (self.root / 'offline/bin/zcblock-csi').write_bytes(b'tampered')
        with self.assertRaisesRegex(ValueError, 'differs from offline'):
            repro.verify(self.root / 'offline/bin', self.root / 'offline/build.json')

    def test_runtime_depends_on_comparison_and_offline_outputs(self):
        text = (Path(__file__).resolve().parents[1] / 'zccusan/deploy/zcblock-csi/Dockerfile.fips').read_text()
        self.assertIn('FROM offline-build AS reproducibility-check', text)
        self.assertIn('ARG FIPS_COMPILED_STAGE=offline-build', text)
        self.assertIn('FROM ${FIPS_COMPILED_STAGE} AS builder', text)
        self.assertIn('COPY --from=builder /out/bin/ /usr/local/bin/', text)
        self.assertIn('RUN --network=none test -n "$FIPS_REBUILD_NONCE"', text)
        self.assertNotIn('COPY --from=online-reference /out/bin/ /usr/local/bin/', text)


if __name__ == '__main__':
    unittest.main()
