import base64
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("smoke", Path(__file__).with_name("github-ec2-runner-smoke.py"))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

CONFIG = {
    "runner_pool": "pool", "required_tag_values": {"RunnerPool": "pool", "adhocKeepaliveModeAction": "terminate"},
    "expiry_tag_keys": ["ExpiresAt", "adhocKeepalive"],
    "allowed_subnet_ids": ["subnet-test"], "allowed_security_group_ids": ["sg-test"],
    "schedule_group_name": "group", "termination_target_arn": "target", "termination_role_arn": "role",
    # Deprecated compatibility hints must not restrict a reviewed workflow's
    # concrete runtime selection.
    "allowed_image_ids": ["ami-deadbeefdeadbeef0"], "allowed_instance_types": ["t3.nano"],
}
LABEL = "zc-fips-smoke-0123456789ab"
JIT = base64.b64encode(json.dumps({
    ".runner": base64.b64encode(json.dumps({"AgentName": LABEL, "Ephemeral": "True"}).encode()).decode(),
}).encode()).decode()


class RunnerSmokeTests(unittest.TestCase):
    def test_boot_script_and_console_formats(self):
        script = smoke.boot_script(JIT, LABEL, 15)
        self.assertIn("shutdown -P +14", script)
        subprocess.run(["bash", "-n"], input=script, text=True, check=True)
        marker = f"ZC_RUNNER_READY={LABEL}\n"
        self.assertEqual(smoke.console_text(marker), marker)
        self.assertEqual(smoke.console_text("Booting…\n" + marker), "Booting…\n" + marker)
        self.assertEqual(smoke.console_text(base64.b64encode(marker.encode()).decode()), marker)
        with self.assertRaises(AssertionError):
            smoke.boot_script("eA==", "$(unexpected-command)", 15)

    def test_reusable_runner_configuration_is_rejected(self):
        encoded = base64.b64encode(json.dumps({
            ".runner": base64.b64encode(json.dumps({"AgentName": LABEL, "Ephemeral": "False"}).encode()).decode(),
        }).encode()).decode()
        with self.assertRaisesRegex(ValueError, "ephemeral=true"):
            smoke.boot_script(encoded, LABEL, 15)

    def test_success_requires_tags_schedule_and_ready_marker(self):
        created = {}

        def aws(config, service, operation, **kw):
            if operation == "describe-images":
                return {"Images": [{"State": "available", "Architecture": "x86_64",
                                    "RootDeviceName": "/dev/xvda"}]}
            if operation == "run-instances":
                self.assertEqual(kw["ImageId"], "ami-0123456789abcdef0")
                self.assertEqual(kw["InstanceType"], "m6i.large")
                self.assertEqual(kw["MinCount"], kw["MaxCount"])
                self.assertEqual(kw["MaxCount"], 1)
                self.assertEqual(kw["BlockDeviceMappings"][0]["DeviceName"], "/dev/xvda")
                self.assertEqual({item["ResourceType"] for item in kw["TagSpecifications"]},
                                 {"instance", "volume", "network-interface"})
                for item in kw["TagSpecifications"]:
                    tags = {t["Key"]: t["Value"] for t in item["Tags"]}
                    self.assertEqual(tags["adhocKeepaliveModeAction"], "terminate")
                    self.assertEqual(tags["adhocKeepalive"], tags["ExpiresAt"])
                return {"Instances": [{"InstanceId": "i-test"}]}
            if operation == "create-schedule":
                created.update(kw)
                return {}
            if operation == "get-schedule":
                return created
            if operation == "get-console-output":
                return {"Output": f"ZC_RUNNER_READY={LABEL}\n"}
            raise AssertionError(operation)

        with tempfile.NamedTemporaryFile() as output, patch.dict(os.environ, {
            "RUNNER_LABEL": LABEL, "RUNNER_JIT_CONFIG": JIT, "GITHUB_OUTPUT": output.name,
            "RUNNER_IMAGE_ID": "ami-0123456789abcdef0", "RUNNER_INSTANCE_TYPE": "m6i.large",
        }), patch.object(smoke, "aws", side_effect=aws), patch.object(smoke, "save"), patch.object(smoke, "cleanup") as cleanup:
            smoke.launch(CONFIG, "pool-test", {"Arn": "controller"})
            cleanup.assert_not_called()

    def test_schedule_failure_rolls_back_instance(self):
        def aws(config, service, operation, **kw):
            if operation == "describe-images":
                return {"Images": [{"State": "available", "Architecture": "x86_64",
                                    "RootDeviceName": "/dev/sda1"}]}
            if operation == "run-instances":
                return {"Instances": [{"InstanceId": "i-test"}]}
            if operation == "create-schedule":
                raise RuntimeError("scheduler unavailable")
            raise AssertionError(operation)

        with tempfile.NamedTemporaryFile() as output, patch.dict(os.environ, {
            "RUNNER_LABEL": LABEL, "RUNNER_JIT_CONFIG": JIT, "GITHUB_OUTPUT": output.name,
            "RUNNER_IMAGE_ID": "ami-0123456789abcdef0", "RUNNER_INSTANCE_TYPE": "m6i.large",
        }), patch.object(smoke, "aws", side_effect=aws), patch.object(smoke, "save"), patch.object(smoke, "cleanup") as cleanup:
            with self.assertRaisesRegex(RuntimeError, "scheduler unavailable"):
                smoke.launch(CONFIG, "pool-test", {})
            cleanup.assert_called_once_with(CONFIG, "pool-test")

    def test_launch_selection_must_be_concrete_and_bounded(self):
        for variable, value, message in (
            ("RUNNER_IMAGE_ID", "latest-amazon-linux", "AMI"),
            ("RUNNER_INSTANCE_TYPE", "*", "instance type"),
            ("RUNNER_LIFETIME_MINUTES", "46", "lifetime"),
        ):
            with self.subTest(variable=variable), patch.dict(os.environ, {
                "RUNNER_LABEL": LABEL, "RUNNER_JIT_CONFIG": JIT,
                "RUNNER_IMAGE_ID": "ami-0123456789abcdef0",
                "RUNNER_INSTANCE_TYPE": "m6i.large", variable: value,
            }, clear=False):
                with self.assertRaisesRegex(ValueError, message):
                    smoke.launch(CONFIG, "pool-test", {})

    def test_unconfirmed_termination_preserves_schedule(self):
        instances = [{"InstanceId": "i-test", "State": {"Name": "running"}}]
        with patch.object(smoke, "instances_for_run", return_value=instances), patch.object(smoke, "aws") as aws, patch.object(smoke.time, "monotonic", side_effect=[0, 1000]):
            with self.assertRaisesRegex(RuntimeError, "keeping the deadline"):
                smoke.cleanup(CONFIG, "pool-test")
            self.assertEqual([call.args[2] for call in aws.call_args_list], ["terminate-instances"])

    def test_iam_selection_is_open_but_tag_enrollment_is_mandatory(self):
        root = Path(__file__).resolve().parents[1]
        policy = (root / "zccusan/deploy/github-ec2-runner/main.tf").read_text()
        self.assertIn('"arn:aws:ec2:${var.aws_region}::image/*"', policy)
        self.assertNotIn('"ec2:InstanceType"', policy)
        self.assertIn('"aws:RequestTag/adhocKeepaliveModeAction" = "terminate"', policy)
        self.assertIn('"aws:RequestTag/adhocKeepalive" = "$${aws:RequestTag/ExpiresAt}"', policy)
        for sid in ("TaggedInstances", "TaggedNewNetworkInterfaces", "BoundedTaggedVolumes"):
            self.assertIn(f'Sid      = "{sid}"', policy)

    def test_fips_workflow_checks_actual_instance_metadata_before_build(self):
        workflow = (Path(__file__).resolve().parents[1]
                    / ".github/workflows/fips-aws-lc-5314.yml").read_text()
        for check in ("metadata instance-id", "metadata ami-id", "metadata instance-type",
                      "/sys/class/dmi/id/sys_vendor", "/sys/class/dmi/id/product_name",
                      "Intel(R) Xeon(R) Platinum 8375C", 'test "$ID" = amzn',
                      'test "$VERSION_ID" = 2023'):
            self.assertIn(check, workflow)


if __name__ == "__main__":
    unittest.main()
