"""Check rejection and selection in the native offline comparison."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('native_repro', Path(__file__).with_name('fips-reproducible-provider.py'))
native = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(native)


class NativeComparisonTests(unittest.TestCase):
    def test_equal_native_outputs_then_tampering_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            reports = {}
            for mode in ('online', 'offline'):
                directory = root / mode
                directory.mkdir()
                artifacts = {}
                for name in ('bcm.o', 'libcrypto.a', 'bssl', 'identity_probe'):
                    path = directory / name
                    path.write_bytes(name.encode())
                    artifacts[name] = {'path': name, 'sha256': native.repro.sha256(path)}
                reports[mode] = {'source': {'sha': 'same'}, 'tools': {'cc': 'same'},
                                 'commands': ['same'], 'certificate_profile_environment': False,
                                 'network_interfaces': ['lo', 'eth0'] if mode == 'online' else ['lo'],
                                 'artifacts': artifacts}
                native.repro.write_json(root / (mode + '.json'), reports[mode])
            args = (root / 'online.json', root / 'offline.json', root / 'online', root / 'offline', root / 'result.json')
            native.compare_native(*args)
            self.assertEqual(json.loads((root / 'result.json').read_text())['selected_artifacts'], 'offline')
            (root / 'offline/bcm.o').write_bytes(b'tampered')
            with self.assertRaisesRegex(ValueError, 'changed after build'):
                native.compare_native(*args)
            reports['offline']['artifacts']['bcm.o']['sha256'] = native.repro.sha256(root / 'offline/bcm.o')
            native.repro.write_json(root / 'offline.json', reports['offline'])
            with self.assertRaisesRegex(ValueError, 'mismatch'):
                native.compare_native(*args)

    def test_normal_workflow_does_not_enable_online_comparison(self):
        root = Path(__file__).resolve().parents[1]
        workflow = (root / '.github/workflows/fips-aws-lc-5314.yml').read_text()
        setting = workflow.split('compare_online:', 1)[1].split('runner_label:', 1)[0]
        self.assertIn('default: false', setting)
        self.assertNotIn('cargo build --locked --release', workflow)
        self.assertIn('scripts/fips-reproducible-provider.py', workflow)
        self.assertIn('cp -a "$RUNNER_TEMP/linked-bin/." "$artifact/bin/"', workflow)


if __name__ == '__main__':
    unittest.main()
