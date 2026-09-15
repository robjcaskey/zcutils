#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import json
from pathlib import Path
from argparse import Namespace
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("zc-image-attest.py")
SPEC = importlib.util.spec_from_file_location("zc_image_attest", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ImageAttestationTests(unittest.TestCase):
    def test_signing_terraform_uses_exact_non_exportable_identity(self) -> None:
        module = SCRIPT.parents[1] / "zccusan/deploy/image-attestation-signing"
        main = (module / "main.tf").read_text()
        outputs = (module / "outputs.tf").read_text()
        self.assertIn('key_alias         = "alias/zcutils-build-attestation-signing-authority-Rob-J-Caskey"', main)
        self.assertIn('parameter_name    = "/zcutils/build-attestation/signing-authority/Rob-J-Caskey/kms-key-arn"', main)
        self.assertIn('signing_authority = "Rob J. Caskey"', main)
        self.assertIn('key_usage                = "SIGN_VERIFY"', main)
        self.assertIn('customer_master_key_spec = "ECC_NIST_P256"', main)
        self.assertIn('type        = "String"', main)
        self.assertIn('value       = aws_kms_key.attestation_signing.arn', main)
        self.assertIn('actions   = ["kms:DescribeKey", "kms:GetPublicKey", "kms:Sign", "kms:Verify"]', main)
        self.assertIn('name                 = "zcutils-build-attestation-signer-Rob-J-Caskey"', main)
        self.assertIn('"token.actions.githubusercontent.com:sub" = var.github_oidc_subjects', main)
        self.assertIn('policy_arn = aws_iam_policy.signer.arn', main)
        self.assertNotIn("SecureString", main)
        self.assertNotIn("private", outputs.casefold())
        self.assertGreaterEqual(main.count("prevent_destroy = true"), 2)

    def test_inspect_accepts_container_timestamps_and_normalizes_to_utc(self) -> None:
        timestamps = (
            ("2026-01-01T01:02:03.123456+00:00", "2026-01-01T01:02:03Z"),
            ("2026-01-01T01:02:03.123456789Z", "2026-01-01T01:02:03Z"),
            ("2026-01-01T23:59:59.999999999-01:00", "2026-01-02T00:59:59Z"),
        )
        for raw, expected in timestamps:
            with self.subTest(raw=raw):
                inspected = [{"Id": "c" * 64, "Created": raw}]
                with mock.patch.object(MODULE, "run", return_value=json.dumps(inspected)):
                    digest, created = MODULE.inspect_image("podman", "example:test")
                self.assertEqual(digest, "c" * 64)
                self.assertEqual(created, expected)

        self.assertEqual(
            MODULE.python_iso_timestamp("2026-01-01T01:02:03.123456789Z"),
            "2026-01-01T01:02:03.123456+00:00",
        )

    def test_inspect_rejects_creation_timestamp_without_offset(self) -> None:
        inspected = [{"Id": "c" * 64, "Created": "2026-01-01T01:02:03.123456789"}]
        with mock.patch.object(MODULE, "run", return_value=json.dumps(inspected)):
            with self.assertRaisesRegex(ValueError, "has no UTC offset"):
                MODULE.inspect_image("docker", "example:test")

    def test_normalizers_name_authority_and_bind_subject(self) -> None:
        digest = "a" * 64
        spdx = {"creationInfo": {"creators": ["Tool: syft"]}}
        cdx = {"metadata": {}}
        MODULE.normalize_spdx(spdx, "example/image:test", "nonfips", digest, "2026-01-01T00:00:00Z")
        MODULE.normalize_cyclonedx(cdx, "example/image:test", "nonfips", digest, "2026-01-01T00:00:00Z")
        self.assertIn("Person: Rob J. Caskey", spdx["creationInfo"]["creators"])
        self.assertEqual(cdx["metadata"]["authors"], [{"name": "Rob J. Caskey"}])
        statement = MODULE.make_statement("example/image:test", digest, MODULE.SPDX_PREDICATE, spdx)
        self.assertEqual(statement["subject"][0]["digest"]["sha256"], digest)
        self.assertEqual(statement["predicate"], spdx)

    def test_generate_end_to_end_for_both_build_variants(self) -> None:
        digest = "d" * 64

        def fake_run(command: list[str], **_kwargs: object) -> str:
            if command[:2] == ['docker', 'create']:
                return 'container-test'
            if command[:2] == ['docker', 'rm']:
                return ''
            if command[:2] == ['docker', 'cp']:
                directory = Path(command[-1])
                directory.mkdir()
                (directory / 'zcblock-csi').write_bytes(b'executable')
                return ''
            if command[1:3] == ["image", "inspect"]:
                return json.dumps([{"Id": "sha256:" + digest, "Created": "2026-01-01T00:00:00Z"}])
            if command[0] == "syft":
                for output in command:
                    if output.startswith("spdx-json="):
                        Path(output.split("=", 1)[1]).write_text(json.dumps({"spdxVersion": "SPDX-2.3", "creationInfo": {"creators": ["Tool: syft"]}}))
                    if output.startswith("cyclonedx-json="):
                        Path(output.split("=", 1)[1]).write_text(json.dumps({"bomFormat": "CycloneDX", "specVersion": "1.6", "metadata": {}}))
                return ""
            self.fail(f"unexpected command: {command}")

        for variant in ("nonfips", "fips-aspiring"):
            with self.subTest(variant=variant), tempfile.TemporaryDirectory() as temporary:
                args = Namespace(
                    variant=variant,
                    image=f"example/{variant}:test",
                    output_dir=Path(temporary),
                    engine="docker",
                    syft="syft",
                    cosign="cosign",
                    cosign_key=None,
                    aws="aws",
                    aws_profile=None,
                    kms_key_parameter=None,
                    source_date_epoch=None,
                    skip_build=True,
                    push_image=False,
                    sign_image=False,
                    provider_only_fips_check=False,
                    cache_from=None,
                    cache_verification_key=None,
                    cache_trusted_public_key_sha256=None,
                    cache_export_ref=None,
                    cache_builder_identity=None,
                    sign_cache_export=False,
                    allow_insecure_loopback_registry=False,
                )
                def offline_record(a):
                    (a.output_dir / 'offline-build.json').write_text(json.dumps({
                        'artifacts': {'zcblock-csi': MODULE.release_payload.digest(b'executable')},
                        'inputs': {}, 'provider_libcrypto_sha256': 'a' * 64}))
                with mock.patch.object(MODULE.shutil, "which", return_value="/bin/fake"), mock.patch.object(MODULE, "run", side_effect=fake_run), mock.patch.object(MODULE, "verify_offline_image", side_effect=offline_record) as verify_offline:
                    MODULE.generate(args)
                    self.assertEqual(verify_offline.call_count, int(variant == "fips-aspiring"))
                manifest = json.loads((Path(temporary) / f"zcblock-csi-{variant}.attestation-manifest.json").read_text())
                self.assertEqual(manifest["signingAuthority"], "Rob J. Caskey")
                self.assertEqual(manifest["subject"]["digest"]["sha256"], digest)
                self.assertFalse(manifest["signed"])
                exported = Path(temporary) / f'zcblock-csi-{variant}.unsigned-executable-bundle.sha256'
                self.assertEqual(exported.read_text().strip(), manifest['payload']['digest']['sha256'])
                # A caller can require the exact earlier bundle before signing.
                args.effective_build_timestamp = args.source_date_epoch
                args.expected_unsigned_executable_bundle_sha256 = '0' * 64
                with mock.patch.object(MODULE.shutil, 'which', return_value='/bin/fake'), mock.patch.object(MODULE, 'run', side_effect=fake_run), mock.patch.object(MODULE, 'verify_offline_image', side_effect=offline_record), mock.patch.object(MODULE, 'resolve_signing_key') as signing:
                    with self.assertRaisesRegex(SystemExit, 'signing and publication refused'):
                        MODULE.generate(args)
                    signing.assert_not_called()
                # Updating the outer checksum cannot hide a conflicting SBOM hash.
                exported.write_text('0' * 64 + '\n')
                manifest['files'][exported.name]['sha256'] = MODULE.sha256_file(exported)
                manifest_path = Path(temporary) / f'zcblock-csi-{variant}.attestation-manifest.json'
                manifest_path.write_text(json.dumps(manifest))
                with self.assertRaisesRegex(SystemExit, 'exported payload hash'):
                    MODULE.verify_directory(Path(temporary), f'zcblock-csi-{variant}')

    def test_ssm_reference_resolves_to_cosign_kms_uri(self) -> None:
        args = Namespace(
            cosign_key=None,
            kms_key_parameter=MODULE.SSM_KMS_PARAMETER,
            aws="aws",
            aws_profile="slopmud-cicd",
        )
        key_arn = "arn:aws:kms:us-east-1:968134102381:key/00000000-0000-0000-0000-000000000000"
        with mock.patch.object(MODULE.shutil, "which", return_value="/bin/aws"), mock.patch.object(MODULE, "run", return_value=key_arn + "\n") as invoked:
            self.assertEqual(MODULE.resolve_signing_key(args), "awskms:///" + key_arn)
        command = invoked.call_args.args[0]
        self.assertIn(MODULE.SSM_KMS_PARAMETER, command)
        self.assertIn("slopmud-cicd", command)

    def test_manifest_verification_detects_tampering(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            prefix = "zcblock-csi-fips-aspiring"
            sbom = output / f"{prefix}.spdx.json"
            statement = output / f"{prefix}.spdx.json.intoto.json"
            MODULE.canonical_write(sbom, {"spdxVersion": "SPDX-2.3"})
            digest = "b" * 64
            subject = {"name": "example/fips:test", "digest": {"sha256": digest}}
            MODULE.canonical_write(statement, MODULE.make_statement(subject["name"], digest, MODULE.SPDX_PREDICATE, json.loads(sbom.read_text())))
            MODULE.canonical_write(
                output / f"{prefix}.attestation-manifest.json",
                {
                    "schema": 1,
                    "signingAuthority": MODULE.AUTHORITY,
                    "variant": "fips-aspiring",
                    "subject": subject,
                    "files": {
                        sbom.name: {"sha256": MODULE.sha256_file(sbom)},
                        statement.name: {"sha256": MODULE.sha256_file(statement)},
                    },
                },
            )
            MODULE.verify_directory(output, prefix)
            sbom.write_text("{}\n")
            with self.assertRaises(SystemExit):
                MODULE.verify_directory(output, prefix)

    def test_corrupt_cosign_bundle_fails_cryptographic_verification(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            prefix = "zcblock-csi-nonfips"
            digest = "e" * 64
            subject = {"name": "example/image:test", "digest": {"sha256": digest}}
            files: list[Path] = []
            for suffix, predicate_type in (
                ("spdx.json", MODULE.SPDX_PREDICATE),
                ("cyclonedx.json", MODULE.CYCLONEDX_PREDICATE),
            ):
                sbom = output / f"{prefix}.{suffix}"
                statement = output / f"{sbom.name}.intoto.json"
                bundle = Path(str(statement) + ".cosign.bundle")
                MODULE.canonical_write(sbom, {"format": suffix})
                MODULE.canonical_write(statement, MODULE.make_statement(subject["name"], digest, predicate_type, json.loads(sbom.read_text())))
                bundle.write_text("corrupt bundle\n")
                files.extend([sbom, statement, bundle])
            MODULE.canonical_write(
                output / f"{prefix}.attestation-manifest.json",
                {
                    "schema": 1,
                    "signingAuthority": MODULE.AUTHORITY,
                    "variant": "nonfips",
                    "signed": True,
                    "subject": subject,
                    "files": {path.name: {"sha256": MODULE.sha256_file(path)} for path in files},
                },
            )
            public_key = output / "public.pem"
            public_key.write_text("not a public key\n")
            failure = subprocess.CalledProcessError(1, ["cosign", "verify-blob"])
            with mock.patch.object(MODULE.shutil, "which", return_value="/bin/cosign"), mock.patch.object(MODULE, "run", side_effect=failure):
                with self.assertRaises(subprocess.CalledProcessError):
                    MODULE.verify_directory(
                        output,
                        prefix,
                        cosign="cosign",
                        cosign_verification_key=str(public_key),
                        require_signature=True,
                    )

    def test_required_signature_rejects_manifest_downgrade(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            prefix = "zcblock-csi-nonfips"
            MODULE.canonical_write(
                output / f"{prefix}.attestation-manifest.json",
                {
                    "schema": 1,
                    "signingAuthority": MODULE.AUTHORITY,
                    "variant": "nonfips",
                    "signed": False,
                    "subject": {"name": "example/image:test", "digest": {"sha256": "f" * 64}},
                    "files": {},
                },
            )
            with self.assertRaisesRegex(SystemExit, "signature required"):
                MODULE.verify_directory(output, prefix, require_signature=True)

    def cache_args(self, reference: str = "registry.example/zcutils/cache:dev") -> Namespace:
        return Namespace(
            cache_from=reference,
            cache_verification_key="awskms:///alias/zcutils-build-attestation-signing-authority-Rob-J-Caskey",
            cache_trusted_public_key_sha256=None,
            cache_export_ref=None,
            cache_builder_identity="https://github.com/robjcaskey/zcutils/.github/workflows/zcblock-csi-images.yml@refs/heads/main",
            engine="docker",
            cosign="cosign",
            allow_insecure_loopback_registry=False,
        )

    def test_cache_import_is_digest_pinned_after_authority_verification(self) -> None:
        digest = "sha256:" + "1" * 64
        verified = [{
            "critical": {"image": {"docker-manifest-digest": digest}},
            "optional": {
                "signingAuthority": MODULE.AUTHORITY,
                "builderIdentity": self.cache_args().cache_builder_identity,
            },
        }]

        def fake_run(command: list[str], **_kwargs: object) -> str:
            if "imagetools" in command:
                return json.dumps({"digest": digest})
            if command[:2] == ["cosign", "verify"]:
                self.assertEqual(command[-1], f"registry.example/zcutils/cache@{digest}")
                self.assertIn("signingAuthority=Rob J. Caskey", command)
                self.assertIn(
                    "builderIdentity=" + self.cache_args().cache_builder_identity,
                    command,
                )
                return json.dumps(verified)
            self.fail(f"unexpected command: {command}")

        with mock.patch.object(MODULE.shutil, "which", return_value="/bin/cosign"), mock.patch.object(MODULE, "run", side_effect=fake_run):
            info, claims = MODULE.resolve_and_verify_cache(self.cache_args())
        self.assertEqual(info["resolvedRef"], f"registry.example/zcutils/cache@{digest}")
        self.assertEqual(claims, verified)
        build = ["docker", "buildx", "build"]
        args = self.cache_args()
        MODULE.append_build_cache_args(build, args, info)
        self.assertIn(f"type=registry,ref=registry.example/zcutils/cache@{digest}", build)
        self.assertNotIn("type=registry,ref=registry.example/zcutils/cache:dev", build)

    def test_unsigned_cache_fails_closed(self) -> None:
        digest = "sha256:" + "2" * 64

        def fake_run(command: list[str], **_kwargs: object) -> str:
            if "imagetools" in command:
                return json.dumps({"digest": digest})
            raise subprocess.CalledProcessError(1, command)

        with mock.patch.object(MODULE.shutil, "which", return_value="/bin/cosign"), mock.patch.object(MODULE, "run", side_effect=fake_run):
            with self.assertRaises(subprocess.CalledProcessError):
                MODULE.resolve_and_verify_cache(self.cache_args())

    def test_mutable_only_cache_resolution_fails_closed(self) -> None:
        with mock.patch.object(MODULE.shutil, "which", return_value="/bin/cosign"), mock.patch.object(MODULE, "run", return_value=json.dumps({"mediaType": "application/vnd.oci.image.manifest.v1+json"})):
            with self.assertRaisesRegex(SystemExit, "immutable sha256"):
                MODULE.resolve_and_verify_cache(self.cache_args())

    def test_wrong_cache_signing_authority_fails_closed(self) -> None:
        digest = "sha256:" + "3" * 64
        wrong = [{
            "critical": {"image": {"docker-manifest-digest": digest}},
            "optional": {
                "signingAuthority": "Mallory",
                "builderIdentity": self.cache_args().cache_builder_identity,
            },
        }]
        with mock.patch.object(MODULE.shutil, "which", return_value="/bin/cosign"), mock.patch.object(MODULE, "run", side_effect=[json.dumps({"digest": digest}), json.dumps(wrong)]):
            with self.assertRaisesRegex(SystemExit, "authority"):
                MODULE.resolve_and_verify_cache(self.cache_args())

    def test_wrong_cache_builder_identity_fails_closed(self) -> None:
        digest = "sha256:" + "a" * 64
        wrong = [{
            "critical": {"image": {"docker-manifest-digest": digest}},
            "optional": {
                "signingAuthority": MODULE.AUTHORITY,
                "builderIdentity": "https://example.invalid/untrusted-builder",
            },
        }]
        with mock.patch.object(MODULE.shutil, "which", return_value="/bin/cosign"), mock.patch.object(
            MODULE, "run", side_effect=[json.dumps({"digest": digest}), json.dumps(wrong)]
        ):
            with self.assertRaisesRegex(SystemExit, "builder identity"):
                MODULE.resolve_and_verify_cache(self.cache_args())

    def test_mismatched_pinned_cache_digest_fails_closed(self) -> None:
        supplied = "sha256:" + "4" * 64
        resolved = "sha256:" + "5" * 64
        args = self.cache_args(f"registry.example/zcutils/cache@{supplied}")
        with mock.patch.object(MODULE.shutil, "which", return_value="/bin/cosign"), mock.patch.object(MODULE, "run", return_value=json.dumps({"digest": resolved})):
            with self.assertRaisesRegex(SystemExit, "does not match"):
                MODULE.resolve_and_verify_cache(args)

    def test_verified_cache_metadata_is_written_to_both_sbom_formats(self) -> None:
        imported = {
            "resolvedRef": "registry.example/cache@sha256:" + "6" * 64,
            "signingAuthority": MODULE.AUTHORITY,
            "builderIdentity": self.cache_args().cache_builder_identity,
            "verification": {"method": "cosign verify", "outputSha256": "7" * 64},
        }
        cache = {"import": imported}
        spdx = {"creationInfo": {"creators": []}}
        cyclonedx = {"metadata": {}}
        MODULE.normalize_spdx(spdx, "example/image", "nonfips", "8" * 64,
                              "2026-01-01T00:00:00Z", cache)
        MODULE.normalize_cyclonedx(cyclonedx, "example/image", "nonfips", "8" * 64,
                                   "2026-01-01T00:00:00Z", cache)
        self.assertIn(imported["resolvedRef"], spdx["creationInfo"]["comment"])
        properties = {item["name"]: item["value"] for item in cyclonedx["metadata"]["properties"]}
        self.assertEqual(properties["io.zcutils.attestation.cache.import-authority"], MODULE.AUTHORITY)
        self.assertEqual(
            properties["io.zcutils.attestation.cache.import-builder-identity"],
            self.cache_args().cache_builder_identity,
        )
        self.assertEqual(properties["io.zcutils.attestation.cache.verification-output-sha256"], "7" * 64)

    def test_cache_export_receipt_is_explicitly_untrusted_and_digest_pinned(self) -> None:
        digest = "sha256:" + "9" * 64
        with mock.patch.object(MODULE, "run", return_value=json.dumps({"digest": digest})):
            receipt = MODULE.cache_export_receipt(
                "docker", "registry.example/cache:dev-next", self.cache_args().cache_builder_identity
            )
        self.assertFalse(receipt["trusted"])
        self.assertEqual(receipt["resolvedRef"], f"registry.example/cache@{digest}")
        self.assertIn("signingAuthority=Rob J. Caskey", receipt["postBuildSignCommand"])
        self.assertIn(self.cache_args().cache_builder_identity, receipt["postBuildSignCommand"])
        with self.assertRaises(SystemExit):
            MODULE.validate_cache_export_ref("registry.example/cache;unsafe:tag")

    def test_loopback_cache_requires_explicit_transport_opt_in(self) -> None:
        self.assertEqual(
            MODULE.cosign_registry_args(
                "127.0.0.1:5000/zcutils/cache@sha256:" + "1" * 64,
                True,
                verify=True,
            ),
            ["--allow-insecure-registry", "--insecure-ignore-tlog=true"],
        )
        with self.assertRaisesRegex(SystemExit, "only for a loopback"):
            MODULE.cosign_registry_args(
                "registry.example/zcutils/cache:dev", True, verify=True
            )

    def test_signed_loopback_export_is_immediately_verified(self) -> None:
        digest = "sha256:" + "b" * 64
        builder = "local://test/zcutils-builder"
        args = Namespace(
            cosign="cosign",
            cache_builder_identity=builder,
            engine="docker",
            allow_insecure_loopback_registry=True,
        )
        receipt = {
            "resolvedRef": f"127.0.0.1:5000/zcutils/cache@{digest}",
            "trusted": False,
        }
        verified_info = {"verification": {"outputSha256": "c" * 64}}
        with mock.patch.object(MODULE, "run", return_value="") as invoked, mock.patch.object(
            MODULE, "resolve_and_verify_cache", return_value=(verified_info, [{"ok": True}])
        ):
            updated, claims = MODULE.sign_and_verify_cache_export(
                args, receipt, "/tmp/cache.key"
            )
        command = invoked.call_args.args[0]
        self.assertIn("--allow-insecure-registry", command)
        self.assertIn("--tlog-upload=false", command)
        self.assertTrue(updated["trusted"])
        self.assertEqual(claims, [{"ok": True}])

    def test_pushed_image_signature_is_digest_and_builder_bound(self) -> None:
        digest = "d" * 64
        builder = "https://github.com/robjcaskey/zcutils/.github/workflows/fips-aws-lc-5314.yml@refs/heads/main"
        args = Namespace(cosign="cosign", cache_builder_identity=builder)
        claims = [{
            "critical": {"image": {"docker-manifest-digest": "sha256:" + digest}},
            "optional": {
                "signingAuthority": MODULE.AUTHORITY,
                "builderIdentity": builder,
            },
        }]
        with mock.patch.object(MODULE, "run", side_effect=["", json.dumps(claims)]) as invoked:
            record, verified = MODULE.sign_and_verify_image(
                args, "awskms:///alias/test", "docker.io/robjcaskey/zcblock-csi:test", digest
            )
        self.assertEqual(
            record["resolvedRef"],
            "docker.io/robjcaskey/zcblock-csi@sha256:" + digest,
        )
        self.assertEqual(verified, claims)
        for call in invoked.call_args_list:
            command = call.args[0]
            self.assertIn("builderIdentity=" + builder, command)
            self.assertEqual(command[-1], record["resolvedRef"])


if __name__ == "__main__":
    unittest.main()
