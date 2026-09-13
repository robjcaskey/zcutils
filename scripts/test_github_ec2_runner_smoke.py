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
    "expiry_tag_keys": ["ExpiresAt", "adhocKeepalive"], "allowed_image_ids": ["ami-test"],
    "allowed_subnet_ids": ["subnet-test"], "allowed_security_group_ids": ["sg-test"],
    "schedule_group_name": "group", "termination_target_arn": "target", "termination_role_arn": "role",
}
LABEL = "zc-fips-smoke-0123456789ab"
JIT = base64.b64encode(json.dumps({
    ".runner": base64.b64encode(json.dumps({"AgentName": LABEL, "Ephemeral": "True"}).encode()).decode(),
}).encode()).decode()


class RunnerSmokeTests(unittest.TestCase):
    def test_boot_script_and_console_formats(self):
        script = smoke.boot_script(JIT, LABEL)
        subprocess.run(["bash", "-n"], input=script, text=True, check=True)
        marker = f"ZC_RUNNER_READY={LABEL}\n"
        self.assertEqual(smoke.console_text(marker), marker)
        self.assertEqual(smoke.console_text("Booting…\n" + marker), "Booting…\n" + marker)
        self.assertEqual(smoke.console_text(base64.b64encode(marker.encode()).decode()), marker)
        with self.assertRaises(AssertionError):
            smoke.boot_script("eA==", "$(unexpected-command)")

    def test_reusable_runner_configuration_is_rejected(self):
        encoded = base64.b64encode(json.dumps({
            ".runner": base64.b64encode(json.dumps({"AgentName": LABEL, "Ephemeral": "False"}).encode()).decode(),
        }).encode()).decode()
        with self.assertRaisesRegex(ValueError, "ephemeral=true"):
            smoke.boot_script(encoded, LABEL)

    def test_success_requires_tags_schedule_and_ready_marker(self):
        created = {}

        def aws(config, service, operation, **kw):
            if operation == "run-instances":
                self.assertEqual(kw["MinCount"], kw["MaxCount"])
                self.assertEqual(kw["MaxCount"], 1)
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
        }), patch.object(smoke, "aws", side_effect=aws), patch.object(smoke, "save"), patch.object(smoke, "cleanup") as cleanup:
            smoke.launch(CONFIG, "pool-test", {"Arn": "controller"})
            cleanup.assert_not_called()

    def test_schedule_failure_rolls_back_instance(self):
        def aws(config, service, operation, **kw):
            if operation == "run-instances":
                return {"Instances": [{"InstanceId": "i-test"}]}
            if operation == "create-schedule":
                raise RuntimeError("scheduler unavailable")
            raise AssertionError(operation)

        with tempfile.NamedTemporaryFile() as output, patch.dict(os.environ, {
            "RUNNER_LABEL": LABEL, "RUNNER_JIT_CONFIG": JIT, "GITHUB_OUTPUT": output.name,
        }), patch.object(smoke, "aws", side_effect=aws), patch.object(smoke, "save"), patch.object(smoke, "cleanup") as cleanup:
            with self.assertRaisesRegex(RuntimeError, "scheduler unavailable"):
                smoke.launch(CONFIG, "pool-test", {})
            cleanup.assert_called_once_with(CONFIG, "pool-test")

    def test_unconfirmed_termination_preserves_schedule(self):
        instances = [{"InstanceId": "i-test", "State": {"Name": "running"}}]
        with patch.object(smoke, "instances_for_run", return_value=instances), patch.object(smoke, "aws") as aws, patch.object(smoke.time, "monotonic", side_effect=[0, 1000]):
            with self.assertRaisesRegex(RuntimeError, "keeping the deadline"):
                smoke.cleanup(CONFIG, "pool-test")
            self.assertEqual([call.args[2] for call in aws.call_args_list], ["terminate-instances"])


if __name__ == "__main__":
    unittest.main()
