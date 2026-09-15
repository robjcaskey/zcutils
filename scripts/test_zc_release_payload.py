import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location('payload', Path(__file__).with_name('zc-release-payload.py'))
payload = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(payload)


class PayloadTests(unittest.TestCase):
    def test_rebuild_ignores_filesystem_metadata_but_detects_changed_code_or_epoch(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for index in (1, 2):
                directory = root / str(index)
                directory.mkdir()
                binary = directory / 'app'
                binary.write_bytes(b'compiled executable')
                os.utime(binary, (index, index))
                binary.chmod(0o700 if index == 1 else 0o755)
                payload.create(directory, root / f'{index}.tar', 123, {'source': 'fixed'})
            self.assertEqual(payload.compare(root / '1.tar', root / '2.tar')['status'], 'identical')
            payload.create(root / '2', root / '2.tar', 124, {'source': 'fixed'})
            with self.assertRaisesRegex(ValueError, 'differ'):
                payload.compare(root / '1.tar', root / '2.tar')
            (root / '2/app').write_bytes(b'changed executable')
            payload.create(root / '2', root / '2.tar', 123, {'source': 'fixed'})
            with self.assertRaisesRegex(ValueError, 'differ'):
                payload.compare(root / '1.tar', root / '2.tar')

    def test_symlinks_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'app').symlink_to('/etc/passwd')
            with self.assertRaisesRegex(ValueError, 'regular'):
                payload.create(root, root.parent / 'unused.tar', 123)

    def test_timestamp_is_generated_only_when_missing(self):
        with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(payload.time, 'time', return_value=456):
            self.assertEqual(payload.effective_timestamp(), 456)
            self.assertEqual(payload.effective_timestamp(123), 123)
            self.assertEqual(payload.effective_timestamp(0), 0)
            with self.assertRaises(ValueError):
                payload.effective_timestamp('-1')

    def test_signature_verification_precedes_payload_acceptance(self):
        with mock.patch.object(payload.subprocess, 'run', side_effect=RuntimeError('wrong key')), mock.patch.object(payload, 'validate') as validate:
            with self.assertRaisesRegex(RuntimeError, 'wrong key'):
                payload.verify_signature('payload.tar', 'signature.bundle', 'trusted.pub')
            validate.assert_not_called()


if __name__ == '__main__':
    unittest.main()
