#!/usr/bin/env python3
"""Tests for the certificate-5314 build-procedure runner."""
import importlib.util
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import zipfile


SPEC = importlib.util.spec_from_file_location(
    "fips_recompile_aws_lc", Path(__file__).with_name("fips-recompile-aws-lc.py"))
recompile = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(recompile)


class RecompilationTests(unittest.TestCase):
    def test_provider_adapter_uses_exact_0311_bindings_and_has_no_module_sources(self):
        adapter = Path(__file__).resolve().parents[1] / "vendor/aws-lc-fips-sys-provider"
        expected = {
            "aarch64_unknown_linux_gnu_crypto.rs": "86dfc77c98d23eaae2ae301158044596ca027aba077a107628fe380c0257873f",
            "x86_64_unknown_linux_gnu_crypto.rs": "ab5893cb4c330fd32a87e4eb2bf66dbdeaf20656d397a6eef15ba7153885ec34",
        }
        self.assertFalse((adapter / "aws-lc").exists())
        for name, digest in expected.items():
            value = (adapter / "bindings" / name).read_bytes()
            self.assertEqual(digest, hashlib.sha256(value).hexdigest())
            self.assertGreater(value.count(b"aws_lc_fips_0_13_11_"), 2500)
        build = (adapter / "build.rs").read_text()
        self.assertIn("AWS_LC_FIPS_SYS_SYSTEM_DIR is required", build)
        self.assertIn("certificate_profile_environment", build)
        self.assertIn("static:+whole-archive=zc_aws_lc_fips_startup_check", build)
        self.assertIn('Some(vec!["cmake3", "-DFIPS=1", ".."])', build)
        self.assertNotIn("SYSTEM_SKIP_VERSION_CHECK", build)

    def test_container_build_requires_the_external_provider(self):
        root = Path(__file__).resolve().parents[1]
        dockerfile = (root / "zccusan/deploy/zcblock-csi/Dockerfile.fips").read_text()
        cargo = (root / "Cargo.toml").read_text()
        self.assertIn("ARG FIPS_DISTRO=amzn2023", dockerfile)
        self.assertIn("COPY ${FIPS_PROVIDER_ROOT}/lib/ /opt/aws-lc-fips-5314/lib/", dockerfile)
        compiler, assembly = dockerfile.split('FROM ${FIPS_COMPILED_STAGE} AS builder')
        self.assertNotIn('COPY ${FIPS_PROVIDER_ROOT}/share/ /', compiler)
        self.assertIn('COPY ${FIPS_PROVIDER_ROOT}/share/ /', assembly)
        self.assertIn("ENV AWS_LC_FIPS_SYS_SYSTEM_DIR=/opt/aws-lc-fips-5314", dockerfile)
        self.assertIn("COPY .github/workflows/fips-aws-lc-5314.yml", dockerfile)
        self.assertIn("COPY scripts/github-ec2-runner-smoke.py", dockerfile)
        self.assertNotIn("fips-prepare-source.py", dockerfile)
        self.assertIn('aws-lc-fips-sys = { path = "vendor/aws-lc-fips-sys-provider" }', cargo)

    def test_only_certificate_environment_pairs_are_eligible(self):
        good = {"os_release": {"ID": "amzn", "VERSION_ID": "2023"},
                "architecture": "x86_64", "product_name": "c6i.metal"}
        self.assertEqual([], recompile.environment_findings(good))
        for changes in (
            {"os_release": {"ID": "rhel", "VERSION_ID": "9.6"}},
            {"architecture": "aarch64"},
            {"product_name": "m6i.large"},
        ):
            with self.subTest(changes=changes):
                self.assertTrue(recompile.environment_findings({**good, **changes}))

    def test_archive_manifest_and_extraction_are_content_bound(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive = root / "module.zip"
            with zipfile.ZipFile(archive, "w") as value:
                value.writestr("aws-lc/file.c", "/* exact source */\n")
                value.writestr("aws-lc/include/file.h", "/* exact header */\n")
            name, manifest = recompile.archive_manifest(archive)
            self.assertEqual("aws-lc", name)
            source, extracted = recompile.extract_archive(archive, root / "work")
            self.assertEqual(manifest, extracted)
            self.assertEqual("/* exact source */\n", (source / "file.c").read_text())
            with self.assertRaisesRegex(ValueError, "already exists"):
                recompile.extract_archive(archive, root / "work")

    def test_archive_path_traversal_and_symlinks_are_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            traversal = root / "traversal.zip"
            with zipfile.ZipFile(traversal, "w") as value:
                value.writestr("aws-lc/../escape", "bad")
            with self.assertRaisesRegex(ValueError, "unsafe"):
                recompile.archive_manifest(traversal)

            symlink = root / "symlink.zip"
            info = zipfile.ZipInfo("aws-lc/link")
            info.create_system = 3
            info.external_attr = (0o120777 << 16)
            with zipfile.ZipFile(symlink, "w") as value:
                value.writestr(info, "target")
            with self.assertRaisesRegex(ValueError, "symlink"):
                recompile.archive_manifest(symlink)

    def test_identity_probe_requires_version_and_static_symbol(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "include").mkdir()
            build = root / "build"
            build.mkdir()
            crypto = build / "libcrypto.a"
            crypto.write_bytes(b"synthetic")
            outputs = iter(("", recompile.MODULE_VERSION,
                            "0000000000001234 T awslc_version_string"))
            with mock.patch.object(recompile, "run", side_effect=lambda *args, **kwargs: next(outputs)):
                probe = recompile.build_identity_probe(root, build, crypto)
            self.assertEqual(build / "zc-aws-lc-identity", probe)

            outputs = iter(("", "wrong version", "0000000000001234 T awslc_version_string"))
            with mock.patch.object(recompile, "run", side_effect=lambda *args, **kwargs: next(outputs)):
                with self.assertRaisesRegex(ValueError, "module identity"):
                    recompile.build_identity_probe(root, build, crypto)

    def test_provider_package_copies_prescribed_artifacts_without_rebuild(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "source"
            (source / "include/openssl").mkdir(parents=True)
            (source / "include/openssl/base.h").write_text("#define OPENSSL_IS_AWSLC\n")
            crypto, bcm = root / "libcrypto.a", root / "bcm.o"
            crypto.write_bytes(b"prescribed archive")
            bcm.write_bytes(b"prescribed module")
            report = {
                "schema": 1,
                "certificate_number": 5314,
                "module_version_string": recompile.MODULE_VERSION,
                "source": {"archive_sha256": recompile.ARCHIVE_SHA256},
                "artifacts": {
                    "libcrypto.a": {"sha256": recompile.file_digest(crypto)},
                    "bcm.o": {"sha256": recompile.file_digest(bcm)},
                },
                "build_procedure_passed": True,
            }
            provider, receipt_path = recompile.create_provider(
                root / "provider", source, crypto, bcm, report)
            self.assertEqual(crypto.read_bytes(), (provider / "lib/libcrypto.a").read_bytes())
            self.assertEqual(bcm.read_bytes(), (provider / "lib/bcm.o").read_bytes())
            receipt = json.loads(receipt_path.read_text())
            self.assertEqual(report, json.loads(receipt_path.with_name('provider-build-record.json').read_text()))
            self.assertEqual(receipt["artifacts"]["libcrypto.a"]["sha256"],
                             receipt["provider"]["libcrypto_sha256"])
            with self.assertRaisesRegex(ValueError, "already exists"):
                recompile.create_provider(provider, source, crypto, bcm, report)

    def test_identity_excludes_run_metadata_but_binds_provider_and_build_inputs(self):
        report = {'completed_at': 'first', 'environment': {'boot_id': 'boot-1', 'product_name': 'c6i.metal'},
                  'source': {'archive_sha256': 'source'}, 'tools': {'cc': 'compiler'},
                  'artifacts': {'libcrypto.a': {'sha256': 'crypto', 'path': '/first/libcrypto.a'},
                                'bcm.o': {'sha256': 'bcm'}, 'cmake_cache': {'sha256': 'cache-1'}}}
        first = recompile.provider_identity(report)
        report['completed_at'] = 'second'
        report['environment']['boot_id'] = 'boot-2'
        report['artifacts']['cmake_cache']['sha256'] = 'cache-2'
        report['artifacts']['libcrypto.a']['path'] = '/second/libcrypto.a'
        self.assertEqual(first, recompile.provider_identity(report))
        for key in ('libcrypto.a', 'bcm.o'):
            changed = json.loads(json.dumps(report))
            changed['artifacts'][key]['sha256'] = 'changed'
            self.assertNotEqual(first, recompile.provider_identity(changed))
        for key in ('source', 'tools', 'environment'):
            changed = json.loads(json.dumps(report))
            changed[key]['changed'] = 'different input'
            self.assertNotEqual(first, recompile.provider_identity(changed))


if __name__ == "__main__":
    unittest.main()
