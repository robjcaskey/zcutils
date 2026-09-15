import copy
import datetime as dt
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('assurances', Path(__file__).with_name('fips-service-assurances.py'))
a = importlib.util.module_from_spec(spec)
spec.loader.exec_module(a)


class AssuranceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'src').mkdir()
        (self.root / 'src/lib.rs').write_text('let config = rustls::ClientConfig::builder();\n')
        for name in ('Cargo.toml', 'Cargo.lock', 'build.rs'):
            (self.root / name).write_text(name)
        self.metadata = {'packages': [
            {'id': 'app', 'name': 'zcutils', 'version': '1'},
            {'id': 'tls', 'name': 'rustls', 'version': '0.23.1'},
            {'id': 'provider', 'name': 'aws-lc-rs', 'version': '1.15.3'},
            {'id': 'sys', 'name': 'aws-lc-fips-sys', 'version': '0.13.11'},
            {'id': 'ring', 'name': 'ring', 'version': '0.17.1'}],
            'resolve': {'root': 'app', 'nodes': [
                {'id': 'app', 'features': ['fips'], 'dependencies': ['tls']},
                {'id': 'tls', 'features': ['fips'], 'dependencies': ['provider', 'ring']},
                {'id': 'provider', 'features': ['fips'], 'dependencies': ['sys']},
                {'id': 'sys', 'features': [], 'dependencies': []},
                {'id': 'ring', 'features': [], 'dependencies': []}]}}
        self.report = a.inventory(self.root, self.metadata)
        (self.root / 'record.txt').write_text('fixture justification, not a deployment assessment')
        record = {'path': 'record.txt', 'sha256': a.digest((self.root / 'record.txt').read_bytes())}
        self.review = {'schema': 1, 'reviewer': 'Fixture reviewer',
            'inventory_sha256': self.report['inventory_sha256'],
            'image_digest': 'sha256:' + 'a' * 64,
            'reviewed_at': '2026-09-15', 'expires_at': '2026-09-16',
            'findings': {f['id']: {'disposition': 'unreachable-in-release', 'rationale': 'fixture', 'record': record} for f in self.report['findings']},
            'sections': {name: record for name in a.SECTIONS},
            'gcm_key_budgets': [{'key_scope': 'fixture key family', 'max_encrypting_instances': 2,
                'max_attempts_per_second_per_instance': 1000, 'max_key_lifetime_seconds': 3600,
                'prior_attempts': 0, 'enforcement_record': record}]}

    def check(self):
        return a.validate_review(self.report, self.review, self.root, "sha256:" + "a" * 64, dt.date(2026, 9, 15))

    def test_complete_fixture_and_dependency_paths(self):
        self.assertEqual(self.check(), [])
        ring = next(f for f in self.report['findings'] if f.get('name') == 'ring')
        self.assertEqual(ring['dependency_path'], ['app', 'tls', 'ring'])

    def test_missing_or_disabled_fips_provider_fails_collection(self):
        for node in ('app', 'tls', 'provider'):
            metadata = copy.deepcopy(self.metadata)
            next(n for n in metadata['resolve']['nodes'] if n['id'] == node)['features'] = []
            with self.assertRaises(ValueError):
                a.inventory(self.root, metadata)

    def test_dependency_graph_cannot_silently_omit_nodes(self):
        self.metadata['resolve']['nodes'].pop()
        with self.assertRaisesRegex(ValueError, 'incomplete'):
            a.inventory(self.root, self.metadata)

    def test_changed_source_outside_scan_matches_invalidates_review(self):
        (self.root / 'src/lib.rs').write_text('// changed control flow\nlet config = rustls::ClientConfig::builder();\n')
        self.report = a.inventory(self.root, self.metadata)
        self.assertTrue(any('bind' in error for error in self.check()))

    def test_missing_new_and_duplicate_findings_do_not_pass(self):
        self.review['findings'].pop(next(iter(self.review['findings'])))
        self.assertTrue(any('exactly' in error for error in self.check()))

    def test_review_for_different_image_fails(self):
        self.review["image_digest"] = "sha256:" + "b" * 64
        self.assertTrue(any("image being accepted" in error for error in self.check()))

    def test_changed_record_fails(self):
        (self.root / 'record.txt').write_text('changed')
        self.assertTrue(self.check())

    def test_symlink_outside_review_directory_is_rejected(self):
        with tempfile.TemporaryDirectory() as outside:
            target = Path(outside) / 'record'
            target.write_text('outside')
            (self.root / 'link').symlink_to(target)
            self.assertFalse(a.supporting_record({'path': 'link', 'sha256': a.digest(b'outside')}, self.root))

    def test_aggregate_limit_includes_prior_attempts(self):
        budget = self.review['gcm_key_budgets'][0]
        budget.update(max_encrypting_instances=1, max_attempts_per_second_per_instance=1,
                      max_key_lifetime_seconds=1, prior_attempts=2**32 - 1)
        self.assertEqual(self.check(), [])
        budget['prior_attempts'] += 1
        self.assertTrue(any('exceeds' in error for error in self.check()))

    def test_no_fractional_negative_boolean_or_missing_bounds(self):
        budget = self.review['gcm_key_budgets'][0]
        for invalid in (-1, 0, True, 1.5, '1000', None):
            budget['max_encrypting_instances'] = invalid
            self.assertTrue(self.check(), invalid)

    def test_missing_section_or_budget_cannot_pass(self):
        self.review['sections'].pop('self_test_failure_handling')
        self.review['gcm_key_budgets'] = []
        errors = self.check()
        self.assertTrue(any('sections' in error for error in errors))
        self.assertTrue(any('budget inventory' in error for error in errors))

    def test_explicit_long_review_period_is_allowed(self):
        self.review["expires_at"] = "2028-09-15"
        self.assertEqual(self.check(), [])

    def test_expired_review_and_tampered_inventory_fail(self):
        self.review['expires_at'] = '2026-09-14'
        self.report['source_files']['Cargo.lock'] = '0' * 64
        errors = self.check()
        self.assertTrue(any('integrity' in error for error in errors))
        self.assertTrue(any('expired' in error for error in errors))


if __name__ == '__main__':
    unittest.main()
