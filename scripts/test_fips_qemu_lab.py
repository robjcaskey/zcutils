#!/usr/bin/env python3
"""Negative tests prevent ordinary guests or partial evidence passing the lab."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("lab", Path(__file__).with_name("fips-qemu-lab.py"))
lab = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lab)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.node = {"os_id": "ubuntu", "fips_enabled": "1"}
        self.probe = dict.fromkeys(("passed", "fips_feature", "aws_lc_fips_mode", "tls_provider_fips", "host_fips_enabled"), True)

    def test_complete_runtime_evidence(self):
        self.assertEqual(lab.evaluate_guest(self.node, self.probe, "ubuntu"), [])

    def test_provider_alone_cannot_pass_guest(self):
        self.node["fips_enabled"] = "0"
        self.assertTrue(lab.evaluate_guest(self.node, self.probe, "ubuntu"))

    def test_ubi_cannot_impersonate_rhel_guest(self):
        self.node["os_id"] = "ubi"
        self.assertTrue(lab.evaluate_guest(self.node, self.probe, "rhel"))

    def test_every_probe_field_is_required_and_boolean(self):
        for field in self.probe:
            for bad in (None, False, "true", 1):
                probe = dict(self.probe, **{field: bad})
                with self.subTest(field=field, value=bad):
                    self.assertTrue(lab.evaluate_guest(self.node, probe, "ubuntu"))

    def test_missing_kvm_fails_preflight(self):
        with patch.object(lab.os, "access", return_value=False), patch.object(lab, "emit") as output:
            self.assertEqual(lab.preflight(SimpleNamespace(target="ubuntu", report=None)), 1)
            self.assertFalse(output.call_args.args[0]["ready"])

    def test_cluster_rejects_mutable_tag_before_contact(self):
        with patch.object(lab, "run") as command:
            with self.assertRaises(ValueError):
                lab.cluster(SimpleNamespace(image_digest="latest"))
            command.assert_not_called()


if __name__ == "__main__":
    unittest.main()
