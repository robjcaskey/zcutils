import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('admission', Path(__file__).with_name('zc-reproducibility-admission.py'))
admission = importlib.util.module_from_spec(spec)
spec.loader.exec_module(admission)


class AdmissionTests(unittest.TestCase):
    def test_policy_binds_repository_key_and_exact_bundle(self):
        value = admission.admission_policy('registry.example/zcblock-csi', 'trusted-key', 'a' * 64)
        authority = value['spec']['authorities'][0]
        self.assertEqual(authority['key']['data'], 'trusted-key')
        self.assertEqual(value['spec']['images'], [{'glob': 'registry.example/zcblock-csi@sha256:*'}])
        condition = authority['attestations'][0]['policy']['data']
        self.assertIn('count(matches) == 1', condition)
        self.assertIn('sha256=' + 'a' * 64, condition)
        for repository in ('registry.example/*', 'registry.example/zc:tag', 'registry.example/zc@sha256:abc'):
            with self.assertRaises(ValueError):
                admission.admission_policy(repository, 'key', 'a' * 64)

    def test_badge_requires_signatures_and_successful_comparison(self):
        for failure in ('signature', 'comparison', None):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                key = root / 'key.pem'
                key.write_text('trusted-public-key')
                output = root / 'output'
                argv = ['tool', '--first', str(root / 'first'), '--second', str(root / 'second'),
                        '--first-launch', 'first.json', '--second-launch', 'second.json',
                        '--trusted-public-key', str(key), '--output-dir', str(output),
                        '--image-repository', 'registry.example/zc']
                def comparison(*args):
                    if failure == 'comparison':
                        raise ValueError('different builds')
                    Path(args[-1]).write_text(json.dumps({'unsigned_executable_bundle_sha256': 'a' * 64}))
                with mock.patch('sys.argv', argv), mock.patch.object(admission.attest, 'verify_directory',
                        side_effect=ValueError('bad signature') if failure == 'signature' else None) as verify, \
                        mock.patch.object(admission.compare, 'compare_builds', side_effect=comparison) as compare:
                    if failure:
                        with self.assertRaises(ValueError):
                            admission.main()
                        self.assertFalse((output / 'reproducibility-badge.json').exists())
                        if failure == 'signature':
                            compare.assert_not_called()
                    else:
                        admission.main()
                        self.assertEqual(verify.call_count, 2)
                        self.assertTrue((output / 'reproducibility-badge.json').exists())
                        self.assertTrue((output / 'cluster-image-policy.json').exists())
