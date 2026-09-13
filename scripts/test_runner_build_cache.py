#!/usr/bin/env python3
from pathlib import Path
import importlib.util
import os
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
RUNNER_SPEC = importlib.util.spec_from_file_location(
    "github_ec2_runner_smoke_cache", ROOT / "scripts/github-ec2-runner-smoke.py"
)
assert RUNNER_SPEC and RUNNER_SPEC.loader
RUNNER = importlib.util.module_from_spec(RUNNER_SPEC)
RUNNER_SPEC.loader.exec_module(RUNNER)


class RunnerBuildCacheStaticTests(unittest.TestCase):
    def test_terraform_is_one_encrypted_single_writer_volume_and_no_instance(self) -> None:
        text = (ROOT / "zccusan/deploy/github-ec2-runner-cache/main.tf").read_text()
        self.assertEqual(text.count('resource "aws_ebs_volume"'), 1)
        self.assertNotIn('resource "aws_instance"', text)
        self.assertIn("size                 = 20", text)
        self.assertIn('type                 = "gp3"', text)
        self.assertIn("encrypted            = true", text)
        self.assertIn("multi_attach_enabled = false", text)
        self.assertIn('ZcutilsBuildCacheAuthority = local.authority_tag', text)
        self.assertIn('ec2:ResourceTag/RunnerPool', text)
        self.assertIn('ec2:ResourceTag/adhocKeepaliveModeAction', text)
        self.assertIn('ec2:ResourceTag/ExpiresAt', text)
        self.assertIn('resource "aws_iam_role_policy_attachment"', text)

    def test_squid_is_loopback_only_without_tls_interception(self) -> None:
        text = (ROOT / "config/squid-runner-build-cache.conf").read_text()
        self.assertIn("http_port 127.0.0.1:3128", text)
        self.assertIn("cache deny CONNECT", text)
        self.assertNotIn("ssl_bump", text)
        self.assertIn("http_access deny all", text)

    def test_registry_cache_is_loopback_persistent_and_immutably_pinned(self) -> None:
        launcher = (ROOT / "scripts/github-ec2-runner-smoke.py").read_text()
        self.assertIn(
            "docker.io/library/registry@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373",
            launcher,
        )
        self.assertIn("-p 127.0.0.1:5000:5000", launcher)
        self.assertIn("/mnt/zcutils-build-cache/registry:/var/lib/registry", launcher)
        self.assertNotIn("-p 0.0.0.0:5000:5000", launcher)
        buildkit = (ROOT / "config/buildkit-loopback-registry.toml").read_text()
        self.assertIn('[registry."127.0.0.1:5000"]', buildkit)
        workflow = (ROOT / ".github/workflows/fips-aws-lc-5314.yml").read_text()
        self.assertIn(
            "moby/buildkit@sha256:28a898719c18a33f4e8000685287fa36fd0dd9560c6440227d3a732d79bb41d8",
            workflow,
        )
        self.assertIn("--cache-verification-key", workflow)
        self.assertIn("--sign-cache-export", workflow)
        self.assertIn("--allow-insecure-loopback-registry", workflow)
        self.assertNotIn("zcblock-csi-buildcache", (ROOT / ".github/workflows/zcblock-csi-images.yml").read_text())

    def test_bootstrap_checks_az_attachment_encryption_and_size(self) -> None:
        text = (ROOT / "scripts/configure-runner-build-cache.sh").read_text()
        for required in ("AvailabilityZone", "Attachments", '"Size": 20',
                         '"Encrypted": True', '"MultiAttachEnabled": False',
                         "--allow-format-empty", "/dev/disk/by-id/", "dnf -y install xfsprogs util-linux"):
            self.assertIn(required, text)
        self.assertNotIn("CARGO_TARGET_DIR", text)

    def test_docker_cache_mounts_persist_only_authenticated_download_inputs(self) -> None:
        text = (ROOT / "zccusan/deploy/zcblock-csi/Dockerfile").read_text()
        self.assertTrue(text.startswith("# syntax=docker/dockerfile:1@sha256:"))
        self.assertIn("target=/usr/local/cargo/registry", text)
        self.assertIn("target=/usr/local/cargo/git", text)
        self.assertIn("cargo build --locked", text)
        self.assertNotIn("target=/work/target", text)
        fips = (ROOT / "zccusan/deploy/zcblock-csi/Dockerfile.fips").read_text()
        self.assertTrue(fips.startswith("# syntax=docker/dockerfile:1@sha256:"))
        self.assertIn("ARG AL2023_IMAGE=public.ecr.aws/amazonlinux/amazonlinux@sha256:", fips)
        self.assertIn("ARG RUST_IMAGE=docker.io/library/rust@sha256:", fips)
        self.assertEqual(fips.count("id=zcutils-fips-cargo-registry"), 4)
        self.assertEqual(fips.count("id=zcutils-fips-cargo-git"), 4)
        self.assertNotIn("target=/work/target", fips)

    def test_controller_accepts_only_available_tagged_same_az_cache(self) -> None:
        config = {"allowed_subnet_ids": ["subnet-test"], "runner_pool": "zcutils-fips-runner"}
        volume = {
            "VolumeId": "vol-0123456789abcdef0", "AvailabilityZone": "us-east-1a",
            "VolumeType": "gp3", "Size": 20, "Encrypted": True,
            "MultiAttachEnabled": False, "State": "available", "Attachments": [],
            "Tags": [
                {"Key": "RunnerPool", "Value": "zcutils-fips-runner"},
                {"Key": "ZcutilsBuildCacheAuthority", "Value": "Rob-J-Caskey"},
                {"Key": "ZcutilsSingleWriter", "Value": "true"},
            ],
        }

        def fake_aws(_config, service, operation, **_parameters):
            self.assertEqual(service, "ec2")
            if operation == "describe-subnets":
                return {"Subnets": [{"AvailabilityZone": "us-east-1a"}]}
            if operation == "describe-volumes":
                return {"Volumes": [volume]}
            self.fail(operation)

        environment = {
            "RUNNER_CACHE_VOLUME_ID": "vol-0123456789abcdef0",
            "RUNNER_CACHE_AVAILABILITY_ZONE": "us-east-1a",
            "RUNNER_CACHE_ALLOW_FORMAT_EMPTY": "false",
            "RUNNER_CACHE_ENABLE_SQUID": "false",
        }
        with mock.patch.dict(os.environ, environment, clear=False), mock.patch.object(RUNNER, "aws", side_effect=fake_aws):
            result = RUNNER.cache_volume_for_launch(config, "zc-fips-build-0123456789ab")
            self.assertEqual(result["availability_zone"], "us-east-1a")
            with self.assertRaisesRegex(ValueError, "only for FIPS build"):
                RUNNER.cache_volume_for_launch(config, "zc-fips-smoke-0123456789ab")

    def test_root_bootstrap_mounts_cache_before_runner_readiness(self) -> None:
        # Use the same valid JIT fixture shape as the controller's own unit tests.
        import base64, json
        runner = base64.b64encode(json.dumps({"ephemeral": True, "agentName": "zc-fips-build-0123456789ab"}).encode()).decode()
        encoded = base64.b64encode(json.dumps({".runner": runner}).encode()).decode()
        script = RUNNER.boot_script(encoded, "zc-fips-build-0123456789ab", 35,
                                    "vol-0123456789abcdef0")
        self.assertLess(len(script.encode()), 16384)
        self.assertLess(script.index("mount -o noatime"), script.index("systemctl start zc-gh-runner.service"))
        self.assertIn("EnvironmentFile=-/etc/zcutils-build-cache.env", script)
        self.assertNotIn("CARGO_TARGET_DIR", script)
        self.assertIn('&& 0 == 1', script)
        initialize = RUNNER.boot_script(encoded, "zc-fips-build-0123456789ab", 35,
                                        "vol-0123456789abcdef0", True, True)
        self.assertIn('&& 1 == 1', initialize)
        self.assertIn("http_port 127.0.0.1:3128", initialize)
        self.assertIn("docker run -d --name zcutils-build-cache-registry", initialize)
        self.assertIn("ZCUTILS_CACHE_REF=127.0.0.1:5000/", initialize)
        self.assertIn("HTTP_PROXY=http://127.0.0.1:3128", initialize)
        self.assertNotIn("HTTPS_PROXY=", initialize)
        self.assertNotIn("ssl_bump", initialize)

    def test_build_launcher_attaches_verified_cache_before_runner_ready(self) -> None:
        import base64, json
        label = "zc-fips-build-0123456789ab"
        runner = base64.b64encode(json.dumps({"ephemeral": True, "agentName": label}).encode()).decode()
        jit = base64.b64encode(json.dumps({".runner": runner}).encode()).decode()
        config = {
            "aws_region": "us-east-1", "runner_pool": "zcutils-fips-runner",
            "required_tag_values": {"RunnerPool": "zcutils-fips-runner", "adhocKeepaliveModeAction": "terminate"},
            "expiry_tag_keys": ["ExpiresAt", "adhocKeepalive"],
            "allowed_subnet_ids": ["subnet-test"], "allowed_security_group_ids": ["sg-test"],
            "schedule_group_name": "group", "termination_target_arn": "target",
            "termination_role_arn": "role",
        }
        operations: list[str] = []
        volume = {
            "VolumeId": "vol-0123456789abcdef0", "AvailabilityZone": "us-east-1a",
            "VolumeType": "gp3", "Size": 20, "Encrypted": True,
            "MultiAttachEnabled": False, "State": "available", "Attachments": [],
            "Tags": [
                {"Key": "RunnerPool", "Value": "zcutils-fips-runner"},
                {"Key": "ZcutilsBuildCacheAuthority", "Value": "Rob-J-Caskey"},
                {"Key": "ZcutilsSingleWriter", "Value": "true"},
            ],
        }
        schedule = {}

        def fake_aws(_config, _service, operation, **parameters):
            operations.append(operation)
            if operation == "describe-images":
                return {"Images": [{"State": "available", "Architecture": "x86_64", "RootDeviceName": "/dev/xvda"}]}
            if operation == "describe-subnets":
                return {"Subnets": [{"AvailabilityZone": "us-east-1a"}]}
            if operation == "describe-volumes":
                if "attach-volume" in operations:
                    attached = dict(volume, State="in-use", Attachments=[{"InstanceId": "i-test", "State": "attached"}])
                    return {"Volumes": [attached]}
                return {"Volumes": [volume]}
            if operation == "run-instances":
                return {"Instances": [{"InstanceId": "i-test", "Placement": {"AvailabilityZone": "us-east-1a"}}]}
            if operation == "describe-instances":
                return {"Reservations": [{"Instances": [{
                    "InstanceId": "i-test", "State": {"Name": "running"},
                    "Placement": {"AvailabilityZone": "us-east-1a"},
                }]}]}
            if operation == "attach-volume":
                return {}
            if operation == "create-schedule":
                schedule.update(parameters)
                return {}
            if operation == "get-schedule":
                return schedule
            if operation == "get-console-output":
                self.assertIn("attach-volume", operations)
                return {"Output": f"ZC_RUNNER_READY={label}\n"}
            self.fail(operation)

        environment = {
            "RUNNER_LABEL": label, "RUNNER_JIT_CONFIG": jit,
            "RUNNER_IMAGE_ID": "ami-0123456789abcdef0", "RUNNER_INSTANCE_TYPE": "c6i.metal",
            "RUNNER_CACHE_VOLUME_ID": "vol-0123456789abcdef0",
            "RUNNER_CACHE_AVAILABILITY_ZONE": "us-east-1a",
            "RUNNER_CACHE_ALLOW_FORMAT_EMPTY": "false", "RUNNER_CACHE_ENABLE_SQUID": "false",
        }
        with tempfile.NamedTemporaryFile() as output, mock.patch.dict(os.environ, {**environment, "GITHUB_OUTPUT": output.name}, clear=False), mock.patch.object(RUNNER, "aws", side_effect=fake_aws), mock.patch.object(RUNNER, "save"), mock.patch.object(RUNNER, "cleanup"):
            RUNNER.launch(config, "zcutils-fips-runner-test", {"Arn": "controller"})
        self.assertLess(operations.index("get-schedule"), operations.index("describe-instances"))
        self.assertLess(operations.index("get-schedule"), operations.index("attach-volume"))
        self.assertLess(operations.index("attach-volume"), operations.index("get-console-output"))

    def test_cache_attach_waits_through_pending_until_running(self) -> None:
        responses = [
            {"Reservations": [{"Instances": [{
                "State": {"Name": "pending"},
                "Placement": {"AvailabilityZone": "us-east-1a"},
            }]}]},
            {"Reservations": [{"Instances": [{
                "State": {"Name": "running"},
                "Placement": {"AvailabilityZone": "us-east-1a"},
            }]}]},
        ]
        with mock.patch.object(RUNNER, "aws", side_effect=responses) as invoked, \
                mock.patch.object(RUNNER.time, "sleep"):
            RUNNER.wait_for_instance_running({}, "i-test", "us-east-1a", timeout=10)
        self.assertEqual(invoked.call_count, 2)
        for invocation in invoked.call_args_list:
            self.assertEqual(invocation.args[1:3], ("ec2", "describe-instances"))
            self.assertEqual(invocation.kwargs["InstanceIds"], ["i-test"])

    def test_cleanup_waits_for_static_cache_to_be_available(self) -> None:
        instances = [[{"InstanceId": "i-test", "State": {"Name": "running"}}],
                     [{"InstanceId": "i-test", "State": {"Name": "terminated"}}]]
        operations: list[str] = []

        def fake_aws(_config, _service, operation, **_parameters):
            operations.append(operation)
            if operation == "describe-volumes":
                return {"Volumes": [{"State": "available", "Attachments": []}]}
            if operation in ("terminate-instances", "delete-schedule"):
                return {}
            self.fail(operation)

        with mock.patch.dict(os.environ, {"RUNNER_CACHE_VOLUME_ID": "vol-0123456789abcdef0"}, clear=False), mock.patch.object(RUNNER, "instances_for_run", side_effect=instances), mock.patch.object(RUNNER, "aws", side_effect=fake_aws), mock.patch.object(RUNNER, "save"):
            RUNNER.cleanup({"schedule_group_name": "group"}, "run-test")
        self.assertLess(operations.index("terminate-instances"), operations.index("describe-volumes"))
        self.assertIn("delete-schedule", operations)


if __name__ == "__main__":
    unittest.main()
