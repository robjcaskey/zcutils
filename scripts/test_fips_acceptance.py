#!/usr/bin/env python3
"""Regression tests for rejection/acceptance decisions; all green fixtures are synthetic."""
# ACCEPTANCE-CRITERIA-REVIEWED: 2026-09-15T09:56:50Z
# ACCEPTANCE-CRITERIA-SHA256: 9dfc5766b1ee2c99444a5d54b4300a5507d2df334050761afc6a8f52ebb909dc
# ACCEPTANCE-TESTS-SHA256: 53959f0da2af587b9a80d7f37ad5e9df6e586e3866debc5039f5efecffde4370
import argparse
import copy
import datetime as dt
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from unittest import mock
import zipfile


SPEC = importlib.util.spec_from_file_location("fips_acceptance", Path(__file__).with_name("fips-acceptance.py"))
fips = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fips)

ROOT = Path(__file__).resolve().parents[1]
CRITERIA_FILES = (
    "scripts/fips-acceptance.py",
    "zccusan/deploy/zcblock-csi/fips/acceptance-5314.json",
    "zccusan/deploy/zcblock-csi/fips/AWS-LC-RECOMPILATION.md",
)
REVIEW_HISTORY = ROOT / "zccusan/deploy/zcblock-csi/fips/acceptance-review-history.json"
REVIEW_FIELDS = ("ACCEPTANCE-CRITERIA-REVIEWED", "ACCEPTANCE-CRITERIA-SHA256", "ACCEPTANCE-TESTS-SHA256")


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def criteria_digest(root=ROOT):
    manifest = {name: sha256((root / name).read_bytes()) for name in CRITERIA_FILES}
    return sha256(json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode())


def normalized_test_digest(path=Path(__file__)):
    value = path.read_text()
    for field in REVIEW_FIELDS:
        value, count = re.subn(rf'^(# {re.escape(field)}:).+$', rf'\1 <review-metadata>', value, flags=re.M)
        if count != 1:
            raise AssertionError(f"expected one {field} review comment, found {count}")
    return sha256(value.encode())


def review_comments(path=Path(__file__)):
    value = path.read_text()
    result = {}
    for field in REVIEW_FIELDS:
        matches = re.findall(rf'^# {re.escape(field)}: (.+)$', value, re.M)
        if len(matches) != 1:
            raise AssertionError(f"expected one {field} review comment, found {len(matches)}")
        result[field] = matches[0]
    return result


def parse_reviewed_at(value):
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}", value):
        return dt.datetime.combine(dt.date.fromisoformat(value), dt.time(), dt.timezone.utc)
    parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise AssertionError("review timestamp must include a UTC offset")
    return parsed.astimezone(dt.timezone.utc)


def validate_history_transitions(reviews):
    dates = [parse_reviewed_at(item["reviewed_at"]) for item in reviews]
    if dates != sorted(set(dates)):
        raise AssertionError("review timestamps must be unique and strictly increasing")
    for previous, current in zip(reviews, reviews[1:]):
        before = (previous["criteria_sha256"], previous["tests_sha256"])
        after = (current["criteria_sha256"], current["tests_sha256"])
        if before == after:
            raise AssertionError("a new review entry must cover changed criteria or tests")
        if previous["criteria_sha256"] != current["criteria_sha256"] \
                and previous["tests_sha256"] == current["tests_sha256"]:
            raise AssertionError("changed acceptance criteria must be represented by a changed test file")
    return dates


def good_node():
    return {"os_release": {"ID": "amzn", "VERSION_ID": "2023"}, "kernel": "synthetic-kernel",
            "architecture": "x86_64", "node": "synthetic-test-only", "boot_id": "synthetic-boot",
            "fips_enabled": "1", "virtualization": "none", "system_vendor": "Amazon EC2",
            "product_name": "c6i.metal", "cpu_info": ["Intel Xeon Platinum 8375C"]}


def good_probe():
    tests = [{"name": name, "functional": True, "expected_approved": approved,
              "indicator_before": 5, "indicator_after": 6 if approved else 5,
              "approved": approved, "passed": True} for name, approved in fips.SERVICE_EXPECTATIONS.items()]
    tests.append({"name": "rejects_modified_ciphertext", "passed": True})
    return {"schema": 2, "passed": True, "fips_feature": True, "aws_lc_fips_mode": True,
            "tls_provider_fips": True, "tls_client_config_fips": True, "host_fips_enabled": True,
            "architecture": "x86_64", "container_os_release": 'ID="amzn"\nVERSION_ID="2023"\n',
            "module": {"module_version_string": "AWS-LC FIPS 3.1.0", "self_test": True,
                       "integrity_test": True, "provider_receipt_sha256": "d" * 64,
                       "provider_libcrypto_sha256": "e" * 64, "provider_bcm_sha256": "f" * 64,
                       "services": tests}}


def good_application():
    return {"schema": 1, "fips_feature": True, "aws_lc_fips_mode": True, "passed": True,
            "checks": [{"name": name, "functional": True, "indicator_before": 1, "indicator_after": 2,
                        "approved_service_observed": True, "passed": True} for name in sorted(fips.APPLICATION_SERVICES)]
                      + [{"name": "native_tamper_rejected", "passed": True}]}


class ControlTests(unittest.TestCase):
    def test_offline_compilation_recipes_are_bound_to_source_receipts(self):
        files = fips.source_files(ROOT)
        for name in ("scripts/fips-build-reproducibility.py", "scripts/fips-reproducible-provider.py"):
            self.assertEqual(files[name], fips.file_digest(ROOT / name))

    def test_provider_evidence_binds_installed_artifacts_and_exact_commands(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "include/openssl").mkdir(parents=True)
            (root / "include/openssl/base.h").write_text("#define OPENSSL_IS_AWSLC\n")
            (root / "lib").mkdir()
            (root / "lib/libcrypto.a").write_bytes(b"crypto")
            (root / "lib/bcm.o").write_bytes(b"bcm")
            header_hash = fips.digest(fips.canonical(fips.tree_files(root / "include")))
            receipt = {
                "schema": 1, "certificate_number": 5314,
                "module_version_string": "AWS-LC FIPS 3.1.0",
                "build_procedure_passed": True, "certificate_profile_environment": True,
                "source": {"archive_sha256": "fe408fa438850786396faf79eba9ea4116c3802e60f3a95865f0dd2adb64c9f1"},
                "commands": [
                    {"argv": ["cmake3", "-DFIPS=1", ".."], "cwd": "aws-lc-AWS-LC-FIPS-3.1.0/build"},
                    {"argv": ["make"], "cwd": "aws-lc-AWS-LC-FIPS-3.1.0/build"},
                ],
                "artifacts": {
                    "libcrypto.a": {"sha256": fips.file_digest(root / "lib/libcrypto.a")},
                    "bcm.o": {"sha256": fips.file_digest(root / "lib/bcm.o")},
                },
                "provider": {"format": "zc-aws-lc-fips-provider-v1", "linkage": "static-unprefixed",
                             "ffi_abi": "aws-lc-fips-sys-0.13.11",
                             "libcrypto_sha256": fips.file_digest(root / "lib/libcrypto.a"),
                             "bcm_sha256": fips.file_digest(root / "lib/bcm.o"),
                             "headers_manifest_sha256": header_hash},
            }
            path = root / "share/zcutils/fips/provider-receipt.json"
            fips.write_json(path, receipt)
            evidence, errors = fips.provider_evidence(root)
            self.assertEqual([], errors)
            self.assertEqual(receipt, evidence["receipt"])
            (root / "lib/libcrypto.a").write_bytes(b"changed")
            self.assertTrue(fips.provider_evidence(root)[1])
            original = b'!<arch>\n' + f'{"bcm.o/":<16}{123:<12}{0:<6}{0:<6}{"644":<8}{4:<10}`\n'.encode() + b'code'
            normalized = bytearray(original)
            normalized[24:36] = b'0           '
            (root / 'lib/libcrypto.a').write_bytes(normalized)
            receipt['provider']['archive_normalization'] = 'ar-deterministic-metadata-v1'
            receipt['provider']['libcrypto_sha256'] = fips.digest(normalized)
            receipt['artifacts']['libcrypto.a']['sha256'] = fips.digest(normalized)
            path.with_name('libcrypto.original.a').write_bytes(original)
            fips.write_json(path.with_name('provider-build-record.json'), {
                'artifacts': {'libcrypto.a': {'sha256': fips.digest(original)}},
                'provider': {'original_libcrypto_sha256': fips.digest(original)}})
            fips.write_json(path, receipt)
            self.assertEqual([], fips.provider_evidence(root)[1])
            # Updating packaged hashes cannot conceal a changed object payload.
            normalized[-1] ^= 1
            (root / 'lib/libcrypto.a').write_bytes(normalized)
            receipt['provider']['libcrypto_sha256'] = fips.digest(normalized)
            receipt['artifacts']['libcrypto.a']['sha256'] = fips.digest(normalized)
            fips.write_json(path, receipt)
            self.assertIn('normalized provider differs from the prescribed archive beyond archive metadata', fips.provider_evidence(root)[1])

    def test_acceptance_criteria_and_tests_match_reviewed_hashes(self):
        comments = review_comments()
        self.assertRegex(comments["ACCEPTANCE-CRITERIA-SHA256"], r"^[0-9a-f]{64}$")
        self.assertRegex(comments["ACCEPTANCE-TESTS-SHA256"], r"^[0-9a-f]{64}$")
        self.assertEqual(criteria_digest(), comments["ACCEPTANCE-CRITERIA-SHA256"])
        self.assertEqual(normalized_test_digest(), comments["ACCEPTANCE-TESTS-SHA256"])

        history = json.loads(REVIEW_HISTORY.read_text())
        self.assertEqual(1, history.get("schema"))
        self.assertEqual(list(CRITERIA_FILES), history.get("criteria_files"))
        reviews = history.get("reviews")
        self.assertIsInstance(reviews, list)
        self.assertTrue(reviews)
        dates = validate_history_transitions(reviews)
        latest = reviews[-1]
        self.assertEqual(comments["ACCEPTANCE-CRITERIA-REVIEWED"], latest["reviewed_at"])
        self.assertEqual(comments["ACCEPTANCE-CRITERIA-SHA256"], latest["criteria_sha256"])
        self.assertEqual(comments["ACCEPTANCE-TESTS-SHA256"], latest["tests_sha256"])
        self.assertLessEqual(dates[-1], dt.datetime.now(dt.timezone.utc),
                             "review timestamp cannot be in the future")

    def test_changed_criteria_require_changed_tests_and_later_review(self):
        baseline = {"reviewed_at": "2026-09-12", "criteria_sha256": "a" * 64,
                    "tests_sha256": "b" * 64}
        with self.assertRaisesRegex(AssertionError, "changed test file"):
            validate_history_transitions([
                baseline,
                {"reviewed_at": "2026-09-13", "criteria_sha256": "c" * 64,
                 "tests_sha256": "b" * 64},
            ])
        with self.assertRaisesRegex(AssertionError, "strictly increasing"):
            validate_history_transitions([
                baseline,
                {"reviewed_at": "2026-09-12", "criteria_sha256": "c" * 64,
                 "tests_sha256": "d" * 64},
            ])
        self.assertEqual(
            [dt.datetime(2026, 9, 12, tzinfo=dt.timezone.utc),
             dt.datetime(2026, 9, 13, 1, 2, 3, tzinfo=dt.timezone.utc)],
            validate_history_transitions([
                baseline,
                {"reviewed_at": "2026-09-13T01:02:03Z", "criteria_sha256": "c" * 64,
                 "tests_sha256": "d" * 64},
            ]),
        )

    def test_successful_crypto_without_indicator_is_rejected(self):
        probe = good_probe()
        probe["module"]["services"][0]["indicator_after"] = 5
        self.assertTrue(fips.service_errors(probe))

    def test_false_positive_indicator_is_detected_by_negative_control(self):
        probe = good_probe()
        negative = next(test for test in probe["module"]["services"] if not test.get("expected_approved", True))
        negative.update(indicator_after=6, approved=True)
        self.assertTrue(fips.service_errors(probe))

    def test_mode_flag_alone_never_passes(self):
        self.assertTrue(fips.service_errors({"passed": True, "aws_lc_fips_mode": True}))

    def test_missing_duplicate_and_malformed_controls_fail(self):
        for change in (lambda tests: tests.pop(), lambda tests: tests.append(tests[0]),
                       lambda tests: tests[0].update(indicator_before=True),
                       lambda tests: tests[0].update(functional=False)):
            with self.subTest(change=change):
                probe = good_probe()
                change(probe["module"]["services"])
                self.assertTrue(fips.service_errors(probe))

    def test_failed_integrity_and_non_fips_provider_fail(self):
        for key in ("integrity_test", "self_test"):
            probe = good_probe()
            probe["module"][key] = False
            self.assertTrue(fips.service_errors(probe))
        probe = good_probe()
        probe["tls_provider_fips"] = False
        self.assertTrue(fips.service_errors(probe))

    def test_missing_provider_hashes_fail_runtime_evidence(self):
        for field in ("provider_receipt_sha256", "provider_libcrypto_sha256", "provider_bcm_sha256"):
            with self.subTest(field=field):
                probe = good_probe()
                probe["module"].pop(field)
                self.assertTrue(fips.service_errors(probe))

    def test_valid_controls_pass(self):
        self.assertEqual([], fips.service_errors(good_probe()))

    def test_real_application_control_requires_approved_service(self):
        self.assertEqual([], fips.application_errors(good_application()))
        for name in fips.APPLICATION_SERVICES:
            probe = good_application()
            next(check for check in probe["checks"] if check["name"] == name)["indicator_after"] = 1
            self.assertTrue(fips.application_errors(probe))

    def test_environment_rejects_fips_enabled_unsupported_nodes(self):
        profile = json.loads(fips.DEFAULT_PROFILE.read_text())
        for changes in ({"virtualization": "kvm"}, {"product_name": "m6i.large"},
                        {"os_release": {"ID": "rhel", "VERSION_ID": "9.6"}},
                        {"fips_enabled": "0"}, {"cpu_info": []}, {"boot_id": None}):
            with self.subTest(changes=changes):
                node = {**good_node(), **changes}
                self.assertTrue(fips.environment_errors(node, good_probe()["container_os_release"], profile))
        self.assertTrue(fips.environment_errors(good_node(), 'ID="rhel"\nVERSION_ID="9.6"', profile))
        self.assertEqual([], fips.environment_errors(good_node(), good_probe()["container_os_release"], profile))
        builder = {**good_node(), "fips_enabled": "0"}
        self.assertEqual([], fips.environment_errors(
            builder, good_probe()["container_os_release"], profile, require_host_fips=False))

    def test_certificate_identity_status_and_sunset(self):
        profile = json.loads(fips.DEFAULT_PROFILE.read_text())
        page = f"<h1>Certificate #5314</h1><p>{profile['module_name']}</p><div>Status</div><div>Active</div><div>Sunset Date</div><div>6/4/2031</div>"
        self.assertTrue(fips.certificate_status(page, profile, dt.date(2026, 9, 13)))
        for bad in (page.replace("Active", "Historical"), page.replace("5314", "1234"),
                    page.replace("(static)", "(dynamic)"), page.replace("6/4/2031", "1/1/2026")):
            self.assertFalse(fips.certificate_status(bad, profile, dt.date(2026, 9, 13)))
        self.assertFalse(fips.certificate_status(page, profile, dt.date(2032, 1, 1)))

    def test_empty_and_partial_check_sets_cannot_accept(self):
        suite = fips.Acceptance()
        self.assertFalse(suite.report()["accepted"])
        suite.add("module", "one", [])
        self.assertFalse(suite.report()["accepted"])
        suite.add("services", "missing", ["missing"], missing=True)
        suite.add("operation", "bad", ["bad"])
        self.assertEqual({"module": "PASS", "services": "BLOCKED", "operation": "FAIL"}, suite.report()["gates"])


class AcceptanceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        for name, data in {
            "Cargo.toml": '[package]\nname="synthetic"\n', "Cargo.lock": "# synthetic lock\n",
            "build.rs": "fn main() {}\n", "src/lib.rs": "// synthetic library\n",
            "src/global_secure_rpc.rs": "// synthetic RPC\n",
            ".github/workflows/fips-aws-lc-5314.yml": "# synthetic workflow\n",
            "scripts/fips-acceptance.py": "# synthetic collector\n",
            "scripts/github-ec2-runner-smoke.py": "# synthetic launcher\n",
            "scripts/fips-prepare-source.py": "# synthetic source preparation\n",
            "scripts/fips-recompile-aws-lc.py": "# synthetic recompilation runner\n",
            "scripts/fips-build-reproducibility.py": "# synthetic offline compiler\n",
            "scripts/fips-reproducible-provider.py": "# synthetic native isolation\n",
            "zccusan/deploy/zcblock-csi/fips/AWS-LC-RECOMPILATION.md": "# synthetic guide\n",
            "zccusan/deploy/zcblock-csi/fips/acceptance-review-history.json": "{\"schema\": 1}\n",
            "zccusan/deploy/zcblock-csi/Dockerfile.fips": "# synthetic build recipe\n",
            "vendor/aws-lc-fips-sys-provider/Cargo.toml": "# synthetic provider adapter\n",
        }.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(data)
        self.archive = self.root / "synthetic-source.zip"
        with zipfile.ZipFile(self.archive, "w") as archive:
            archive.writestr("synthetic/module.c", "/* synthetic module */")
        self.profile = json.loads(fips.DEFAULT_PROFILE.read_text())
        self.profile["source_archive_sha256"] = fips.file_digest(self.archive)
        self.profile_path = self.root / "synthetic-profile.json"
        fips.write_json(self.profile_path, self.profile)
        self.node = good_node()
        self.probes = {binary: good_probe() for binary in self.profile["binaries"]}
        self.application = good_application()
        self.image_id = "sha256:" + "a" * 64
        self.image_digest = "sha256:" + "b" * 64
        files = fips.source_files(self.root)
        provider_receipt = {
            "schema": 1, "certificate_number": 5314, "module_version_string": "AWS-LC FIPS 3.1.0",
            "build_procedure_passed": True, "certificate_profile_environment": True,
            "source": {"archive_sha256": self.profile["source_archive_sha256"],
                       "manifest_sha256": fips.archive_tree(self.archive)},
            "commands": [
                {"argv": ["cmake3", "-DFIPS=1", ".."], "cwd": "aws-lc-AWS-LC-FIPS-3.1.0/build"},
                {"argv": ["make"], "cwd": "aws-lc-AWS-LC-FIPS-3.1.0/build"},
            ],
            "tools": dict.fromkeys(("cmake3", "go", "make", "cc"), "synthetic version"),
            "provider": {"format": "zc-aws-lc-fips-provider-v1", "linkage": "static-unprefixed",
                         "ffi_abi": "aws-lc-fips-sys-0.13.11", "libcrypto_sha256": "e" * 64,
                         "bcm_sha256": "f" * 64, "headers_manifest_sha256": "1" * 64},
            "artifacts": {"libcrypto.a": {"sha256": "e" * 64}, "bcm.o": {"sha256": "f" * 64}},
        }
        self.receipt = {"schema": 1, "source_files": files, "source_sha256": fips.digest(fips.canonical(files)),
                        "binaries": {binary: {"sha256": "c" * 64, "static_identity_symbols": ["awslc_version_string"],
                                              "prefixed_aws_lc_symbols": [], "dynamic_crypto_dependencies": []}
                                     for binary in self.profile["binaries"]},
                        "builder": good_node(), "module_source_tree_sha256": fips.archive_tree(self.archive),
                        "cmake_caches": {},
                        "provider": {"receipt": provider_receipt, "receipt_sha256": "d" * 64,
                                     "libcrypto_sha256": "e" * 64, "bcm_sha256": "f" * 64,
                                     "headers_manifest_sha256": "1" * 64},
                        "recompilation_assessment": {"security_policy_section": "5314:11.1",
                                                     "status": "section-11.1-linked", "reasons": [],
                                                     "review_override_allowed": False},
                        "tools": dict.fromkeys(("rustc", "cargo", "cc", "nm", "readelf"), "synthetic version"),
                        "dependencies": [{"name": "aws-lc-fips-sys", "version": "synthetic", "features": []}]}
        self.args = argparse.Namespace(profile=self.profile_path, source_root=self.root, image="mutable-tag",
                                       storage=None, offline=False, validated_source=self.archive,
                                       review=self.root / "review.json", report=self.root / "report.json")
        self.review = self.make_review()
        self.save_review()
        self.engine = mock.Mock()
        self.engine.inspect.return_value = {"Id": self.image_id, "Digest": self.image_digest}
        self.engine.run.side_effect = self.container_run
        self.addCleanup(mock.patch.stopall)
        mock.patch.object(fips, "Podman", return_value=self.engine).start()
        mock.patch.object(fips, "environment", side_effect=lambda: self.node).start()
        self.response = mock.MagicMock()
        self.response.__enter__.return_value.read.return_value = (
            f"Certificate #5314 {self.profile['module_name']} Status Active Sunset Date 6/4/2031").encode()
        mock.patch.object(fips.urllib.request, "urlopen", return_value=self.response).start()

    def make_review(self):
        today = dt.date.today()
        review = {"schema": 1, "reviewer": "SYNTHETIC TEST FIXTURE - NOT A REAL REVIEW",
                  "reviewed_at": today.isoformat(), "expires_at": (today + dt.timedelta(days=1)).isoformat(),
                  "certificate_number": 5314, "profile_sha256": fips.file_digest(self.profile_path),
                  "source_sha256": self.receipt["source_sha256"], "image_id": self.image_id,
                  "image_digest": self.image_digest, "build_receipt_sha256": fips.digest(json.dumps(self.receipt).encode()),
                  "environment_sha256": fips.digest(fips.canonical(self.node)), "findings": {}, "sections": {}}
        for section in fips.REVIEW_SECTIONS:
            path = self.root / (section + ".txt")
            path.write_text("Synthetic unit-test review; makes no real acceptance claim.\n")
            review["sections"][section] = {"path": path.name, "sha256": fips.file_digest(path)}
        return review

    def save_review(self):
        fips.write_json(self.args.review, self.review)

    def container_run(self, image, entrypoint, arguments):
        self.assertEqual(self.image_id, image, "all executions must bind the inspected immutable image")
        if entrypoint == "/bin/cat":
            output = json.dumps(self.receipt)
        elif entrypoint == "/bin/sh":
            output = "\n".join(self.profile["binaries"])
        elif entrypoint == "/usr/bin/sha256sum":
            output = "c" * 64 + "  " + arguments[0]
        elif arguments == ["--fips-application-evidence"]:
            output = json.dumps(self.application)
        else:
            self.assertEqual(["--fips-evidence"], arguments)
            output = json.dumps(self.probes[Path(entrypoint).name])
        return subprocess.CompletedProcess([], 0, output, "")

    def run_check(self):
        with mock.patch("sys.stdout", new_callable=io.StringIO):
            status = fips.check(self.args)
        report = json.loads(Path(self.args.report).read_text())
        self.assertEqual(status == 0, report["accepted"])
        return report

    def test_complete_synthetic_fixture_accepts(self):
        report = self.run_check()
        self.assertTrue(report["accepted"])
        self.assertEqual(set(self.profile["binaries"]), set(report["evidence"]["executables"]))

    def test_current_module_version_mismatch_rejects_even_with_good_mode(self):
        self.probes["zcblock-csi"]["module"]["module_version_string"] = "AWS-LC FIPS 4.2.0"
        report = self.run_check()
        self.assertFalse(report["accepted"])
        self.assertEqual("FAIL", report["gates"]["module"])

    def test_known_cargo_recompilation_difference_is_not_review_waivable(self):
        self.receipt["recompilation_assessment"] = {
            "security_policy_section": "5314:11.1",
            "status": "not-section-11.1-linked",
            "reasons": ["BORINGSSL_PREFIX renames module symbols"],
            "review_override_allowed": False,
        }
        self.review = self.make_review()
        self.save_review()
        report = self.run_check()
        self.assertFalse(report["accepted"])
        self.assertEqual("FAIL", report["gates"]["module"])

    def test_cargo_recompilation_assessment_records_wrapper_differences(self):
        cache = "\n".join((
            "CMAKE_HOME_DIRECTORY:INTERNAL=/cargo/aws-lc-fips-sys-0.13.11",
            "BORINGSSL_PREFIX:UNINITIALIZED=aws_lc_fips_0_13_11_",
            "CMAKE_BUILD_TYPE:STRING=release",
            "BUILD_TESTING:BOOL=OFF",
            "BUILD_TOOL:BOOL=OFF",
            "BUILD_LIBSSL:BOOL=OFF",
            "FIPS:UNINITIALIZED=1",
        ))
        result = fips.cargo_recompilation_assessment(
            {"name": "aws-lc-fips-sys", "version": "0.13.11"}, {"cache": cache}, False)
        self.assertEqual("not-section-11.1-linked", result["status"])
        self.assertFalse(result["review_override_allowed"])
        self.assertGreaterEqual(len(result["reasons"]), 6)

        provider = fips.cargo_recompilation_assessment(
            {"name": "aws-lc-fips-sys", "version": "0.13.11"}, {}, True, [])
        self.assertEqual("section-11.1-linked", provider["status"])

    def test_non_approved_application_call_fails_even_when_functional(self):
        self.application["checks"][0].update(indicator_after=1, approved_service_observed=False)
        self.assertEqual("FAIL", self.run_check()["gates"]["services"])

    def test_each_executable_is_required(self):
        for binary in self.profile["binaries"]:
            with self.subTest(binary=binary):
                self.probes[binary]["module"]["self_test"] = False
                self.assertFalse(self.run_check()["accepted"])
                self.probes[binary]["module"]["self_test"] = True

    def test_non_fips_qemu_or_wrong_image_os_rejects(self):
        self.node.update(virtualization="kvm", fips_enabled="0")
        self.assertEqual("FAIL", self.run_check()["gates"]["operation"])

    def test_binary_cannot_pass_under_a_different_architecture(self):
        self.probes["zcutils"]["architecture"] = "aarch64"
        self.assertEqual("FAIL", self.run_check()["gates"]["operation"])

    def test_source_change_invalidates_acceptance(self):
        (self.root / "src/lib.rs").write_text("// changed after build\n")
        self.assertEqual("FAIL", self.run_check()["gates"]["module"])

    def test_changed_binary_and_missing_linkage_reject(self):
        for field, value in (("sha256", "d" * 64), ("static_identity_symbols", []),
                             ("prefixed_aws_lc_symbols", ["aws_lc_fips_0_13_11_FIPS_mode"]),
                             ("dynamic_crypto_dependencies", ["libcrypto.so"])):
            with self.subTest(field=field):
                self.receipt["binaries"]["zcblock-csi"][field] = value
                self.assertFalse(self.run_check()["accepted"])

    def test_missing_source_archive_and_review_are_blocked(self):
        self.args.validated_source = None
        self.args.review = None
        report = self.run_check()
        self.assertFalse(report["accepted"])
        self.assertEqual(dict.fromkeys(fips.GATES, "BLOCKED"), report["gates"])

    def test_offline_cannot_accept(self):
        self.args.offline = True
        self.assertEqual("BLOCKED", self.run_check()["gates"]["module"])

    def test_modified_module_source_or_archive_rejects(self):
        self.receipt["provider"]["receipt"]["source"]["manifest_sha256"] = "d" * 64
        self.assertEqual("FAIL", self.run_check()["gates"]["module"])
        self.archive.write_bytes(b"not the validated source archive")
        self.assertFalse(self.run_check()["accepted"])

    def test_known_security_kdf_is_never_waived(self):
        (self.root / "src/global_secure_rpc.rs").write_text("fn frame_cipher(secret: &str) {\n let h = Sha256::new();\n}\n")
        self.assertTrue(fips.known_service_blockers(self.root))
        self.assertEqual("FAIL", self.run_check()["gates"]["services"])

    def test_only_rustc_excluded_legacy_kdf_is_ignored(self):
        path = self.root / "src/global_secure_rpc.rs"
        legacy = 'fn frame_cipher(secret: &str) {\n let h = Sha256::new();\n}\n'
        path.write_text('#[cfg(not(feature = "fips"))]\n' + legacy)
        self.assertEqual([], fips.known_service_blockers(self.root))
        path.write_text('// #[cfg(not(feature = "fips"))]\n' + legacy)
        self.assertTrue(fips.known_service_blockers(self.root))
        path.write_text('#[cfg(not(feature = "fips"))]\n' + legacy + '#[cfg(feature = "fips")]\n' + legacy)
        self.assertTrue(fips.known_service_blockers(self.root))

    def test_native_socket_kdf_is_also_a_hard_blocker(self):
        for name in ("zcnblk_aes256_lane_cipher", "zcnblk_payload_aes256_cipher"):
            (self.root / "src/lib.rs").write_text(f"fn {name}(token: &str) {{\n let h = Sha256::new();\n}}\n")
            self.assertTrue(fips.known_service_blockers(self.root))

    def test_frozen_module_advisories_require_disposition(self):
        findings = fips.source_findings(self.root, [{"name": "aws-lc-fips-sys", "version": "0.13.11", "features": ["fips"]}])
        advisory = next(item for item in findings if item["rule"] == "pinned-module-advisory-review")
        self.assertIn("GHSA-9f94-5g5w-gf6r", advisory["advisories"])

    def test_new_crypto_findings_invalidate_review(self):
        self.receipt["dependencies"].append({"name": "rustls", "version": "0.19.1", "features": ["tls"]})
        report = self.run_check()
        self.assertFalse(report["accepted"])
        self.assertTrue(report["evidence"]["findings"])

    def test_review_cannot_be_stale_expired_or_empty(self):
        baseline = copy.deepcopy(self.review)
        for changes in ({"image_id": "sha256:" + "d" * 64}, {"reviewer": ""},
                        {"expires_at": "2000-01-01"}, {"environment_sha256": "e" * 64},
                        {"profile_sha256": "f" * 64}, {"sections": {}}):
            with self.subTest(changes=changes):
                self.review = {**baseline, **changes}
                self.save_review()
                self.assertFalse(self.run_check()["accepted"])

    def test_changed_review_artifact_and_path_escape_reject(self):
        (self.root / "crypto_service_map.txt").write_text("changed after review")
        self.assertFalse(self.run_check()["accepted"])
        self.review["sections"]["crypto_service_map"]["path"] = "../outside.txt"
        self.save_review()
        self.assertFalse(self.run_check()["accepted"])

    def test_probe_timeout_does_not_skip_failure(self):
        real = self.engine.run.side_effect
        def timeout(image, entrypoint, arguments):
            if entrypoint.endswith("zcblock-csi"):
                raise subprocess.TimeoutExpired([entrypoint], 60)
            return real(image, entrypoint, arguments)
        self.engine.run.side_effect = timeout
        self.assertFalse(self.run_check()["accepted"])

    def test_non_json_probe_does_not_pass(self):
        real = self.engine.run.side_effect
        def old_binary(image, entrypoint, arguments):
            if entrypoint.endswith("zcblock-csi"):
                return subprocess.CompletedProcess([], 2, "", "unknown argument")
            return real(image, entrypoint, arguments)
        self.engine.run.side_effect = old_binary
        self.assertFalse(self.run_check()["accepted"])


class CleanupTests(unittest.TestCase):
    def test_timeout_cleans_only_its_recorded_container(self):
        cid = "9" * 64
        calls = []
        def command(argv, timeout=60):
            calls.append(argv)
            if "--cidfile" in argv:
                Path(argv[argv.index("--cidfile") + 1]).write_text(cid)
                raise subprocess.TimeoutExpired(argv, timeout)
            return subprocess.CompletedProcess(argv, 0, "", "")
        with mock.patch.object(fips, "invoke", side_effect=command):
            with self.assertRaises(subprocess.TimeoutExpired):
                fips.Podman().run("sha256:" + "a" * 64, "/bin/probe", [])
        self.assertEqual(["rm", "--force", "--ignore", cid], calls[-1][-4:])
        self.assertIn("--network=none", calls[0])
        self.assertIn("--read-only", calls[0])
        self.assertIn("--remote=false", calls[0])


if __name__ == "__main__":
    unittest.main()
