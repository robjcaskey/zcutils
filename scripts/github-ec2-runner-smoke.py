#!/usr/bin/env python3
"""GitHub controller for a single, 15-minute EC2 runner smoke test."""
import argparse
import base64
import binascii
from contextlib import ExitStack
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

RUNNER_URL = "https://github.com/actions/runner/releases/download/v2.337.0/actions-runner-linux-x64-2.337.0.tar.gz"
RUNNER_SHA256 = "70920811a4f8ad4328818682bca5c6469c1c942fab52448868071d0063816613"


def aws(config, service, operation, *, user_data=None, **parameters):
    # User data contains a one-job credential. Keep it out of argv and logs.
    with ExitStack() as stack:
        request = stack.enter_context(tempfile.NamedTemporaryFile(mode="w+", prefix="zc-aws-"))
        json.dump(parameters, request)
        request.flush()
        args = ["aws", service, operation, "--region", config["aws_region"],
                "--output", "json", "--cli-input-json", "file://" + request.name]
        if user_data is not None:
            script = stack.enter_context(tempfile.NamedTemporaryFile(mode="w+", prefix="zc-user-data-"))
            script.write(user_data)
            script.flush()
            args += ["--user-data", "file://" + script.name]
        result = subprocess.run(args, capture_output=True, text=True, timeout=60,
                                env={**os.environ, "AWS_PAGER": "", "AWS_MAX_ATTEMPTS": "3"})
    if result.returncode:
        raise RuntimeError(result.stderr.strip())
    return json.loads(result.stdout) if result.stdout.strip() else {}


def boot_script(jit_config, label):
    assert re.fullmatch(r"zc-fips-smoke-[0-9a-f]{12}", label)
    files = json.loads(base64.b64decode(jit_config, validate=True))
    settings = json.loads(base64.b64decode(files[".runner"], validate=True))
    settings = {key.casefold(): value for key, value in settings.items()}
    if str(settings.get("ephemeral")).lower() != "true" or settings.get("agentname") != label:
        raise ValueError("JIT configuration must name this runner and set ephemeral=true")
    script = r'''#!/bin/bash
set -euo pipefail
umask 077
trap 'echo ZC_RUNNER_BOOTSTRAP_FAILED >/dev/console; shutdown -P now' ERR
# Secondary guest watchdog; AWS Scheduler and the shared sweeper remain independent.
shutdown -P +14
dnf -y install tar gzip
useradd --create-home --shell /bin/bash gha
install -d -m 0755 /opt/actions-runner
curl -fL --connect-timeout 10 --max-time 180 --retry 3 '__RUNNER_URL__' -o /tmp/runner.tar.gz
echo '__RUNNER_SHA256__  /tmp/runner.tar.gz' | sha256sum -c -
tar -xzf /tmp/runner.tar.gz -C /opt/actions-runner
/opt/actions-runner/bin/installdependencies.sh
printf '%s' '__JIT_CONFIG__' > /home/gha/.jit-config
chown -R gha:gha /opt/actions-runner
chown gha:gha /home/gha/.jit-config
cat >/etc/systemd/system/zc-gh-runner.service <<'UNIT'
[Unit]
Description=One-job GitHub smoke runner
After=network-online.target
Wants=network-online.target
[Service]
Type=simple
User=gha
WorkingDirectory=/opt/actions-runner
Environment=HOME=/home/gha
ExecStart=/bin/bash -c 'exec ./run.sh --jitconfig "$(cat /home/gha/.jit-config)"'
ExecStopPost=+/usr/sbin/shutdown -P now
Restart=no
[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl start zc-gh-runner.service
for attempt in $(seq 1 120); do
  if journalctl -u zc-gh-runner.service --no-pager | grep -F -q 'Listening for Jobs'; then
    echo 'ZC_RUNNER_READY=__LABEL__' >/dev/console
    exit 0
  fi
  if systemctl is-failed --quiet zc-gh-runner.service; then
    echo ZC_RUNNER_BOOTSTRAP_FAILED >/dev/console
    shutdown -P now
    exit 1
  fi
  sleep 2
done
echo ZC_RUNNER_BOOTSTRAP_FAILED >/dev/console
shutdown -P now
exit 1
'''
    for key, value in (("RUNNER_URL", RUNNER_URL), ("RUNNER_SHA256", RUNNER_SHA256),
                       ("JIT_CONFIG", jit_config), ("LABEL", label)):
        script = script.replace("__" + key + "__", value)
    if len(script.encode()) > 16384:
        raise ValueError("Runner configuration exceeds EC2's user-data limit")
    return script


def console_text(value):
    # AWS clients differ in whether they decode EC2's base64 console output.
    try:
        return base64.b64decode(value, validate=True).decode(errors="replace")
    except binascii.Error:
        return value


def save(record, name):
    directory = Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir())) / "ec2-runner-smoke"
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    (directory / name).write_text(json.dumps(record, indent=2) + "\n")


def instances_for_run(config, run_id):
    result = aws(config, "ec2", "describe-instances", Filters=[
        {"Name": "tag:RunnerPool", "Values": [config["runner_pool"]]},
        {"Name": "tag:RunId", "Values": [run_id]},
    ])
    return [i for r in result["Reservations"] for i in r["Instances"]]


def cleanup(config, run_id):
    instances = instances_for_run(config, run_id)
    ids = [i["InstanceId"] for i in instances]
    if ids:
        aws(config, "ec2", "terminate-instances", InstanceIds=ids)
        end = time.monotonic() + 180
        while time.monotonic() < end:
            states = instances_for_run(config, run_id)
            if all(i["State"]["Name"] == "terminated" for i in states):
                break
            time.sleep(5)
        else:
            raise RuntimeError("Termination not confirmed; keeping the deadline schedule")
    try:
        aws(config, "scheduler", "delete-schedule", Name=run_id,
            GroupName=config["schedule_group_name"])
    except RuntimeError as error:
        if "ResourceNotFoundException" not in str(error):
            raise
    report = {"run_id": run_id, "instance_ids": ids, "termination_confirmed": True,
              "schedule_removed": True}
    save(report, "cleanup.json")
    print(json.dumps(report), flush=True)


def launch(config, run_id, identity):
    label = os.environ["RUNNER_LABEL"]
    jit = os.environ.pop("RUNNER_JIT_CONFIG").strip()
    if not jit:
        raise ValueError("Missing one-job runner configuration; run test-github-ec2-runner.py first")
    script = boot_script(jit, label)
    deadline = datetime.now(timezone.utc) + timedelta(minutes=15)
    expiry = deadline.strftime("%Y-%m-%dT%H:%M:%SZ")
    tag_values = {**config["required_tag_values"], "RunId": run_id, "Name": run_id,
                  **{key: expiry for key in config["expiry_tag_keys"]}}
    if tag_values.get("adhocKeepaliveModeAction") != "terminate":
        raise ValueError("Ad hoc termination enrollment is required")
    tags = [{"Key": key, "Value": value} for key, value in tag_values.items()]
    request = {
        "ImageId": config["allowed_image_ids"][0], "InstanceType": "m6i.large",
        "MinCount": 1, "MaxCount": 1, "ClientToken": run_id,
        "MetadataOptions": {"HttpTokens": "required", "HttpEndpoint": "enabled"},
        "InstanceInitiatedShutdownBehavior": "terminate",
        "NetworkInterfaces": [{"DeviceIndex": 0, "SubnetId": config["allowed_subnet_ids"][0],
                               "Groups": config["allowed_security_group_ids"],
                               "AssociatePublicIpAddress": True, "DeleteOnTermination": True}],
        "BlockDeviceMappings": [{"DeviceName": "/dev/sda1", "Ebs": {
            "VolumeType": "gp3", "VolumeSize": 32, "Iops": 3000, "Throughput": 125,
            "Encrypted": True, "DeleteOnTermination": True}}],
        "TagSpecifications": [{"ResourceType": kind, "Tags": tags}
                              for kind in ("instance", "volume", "network-interface")],
    }
    record = {"run_id": run_id, "runner_label": label, "deadline": expiry,
              "controller_identity": identity, "launch_request": request}
    save(record, "launch.json")
    try:
        response = aws(config, "ec2", "run-instances", user_data=script, **request)
        instance_id = response["Instances"][0]["InstanceId"]
        record["instance_id"] = instance_id
        save(record, "launch.json")
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            output.write(f"instance_id={instance_id}\nrunner_label={label}\nrun_id={run_id}\n")
        target = {"Arn": config["termination_target_arn"], "RoleArn": config["termination_role_arn"],
                  "Input": json.dumps({"InstanceIds": [instance_id]}),
                  "RetryPolicy": {"MaximumEventAgeInSeconds": 900, "MaximumRetryAttempts": 5}}
        expression = f"at({deadline.strftime('%Y-%m-%dT%H:%M:%S')})"
        aws(config, "scheduler", "create-schedule", Name=run_id, ClientToken=run_id,
            GroupName=config["schedule_group_name"], ScheduleExpression=expression,
            ScheduleExpressionTimezone="UTC", FlexibleTimeWindow={"Mode": "OFF"},
            State="ENABLED", ActionAfterCompletion="DELETE", Target=target)
        schedule = aws(config, "scheduler", "get-schedule", Name=run_id,
                       GroupName=config["schedule_group_name"])
        assert schedule["State"] == "ENABLED" and schedule["ScheduleExpression"] == expression
        assert schedule["ScheduleExpressionTimezone"] == "UTC"
        assert schedule["ActionAfterCompletion"] == "DELETE"
        assert schedule["Target"]["Arn"] == target["Arn"]
        assert schedule["Target"]["RoleArn"] == target["RoleArn"]
        assert json.loads(schedule["Target"]["Input"]) == {"InstanceIds": [instance_id]}
        record["schedule_verified"] = True
        save(record, "launch.json")
        print(f"Started {instance_id}; AWS termination deadline {expiry}", flush=True)
        until = time.monotonic() + 330
        while time.monotonic() < until:
            try:
                console = aws(config, "ec2", "get-console-output", InstanceId=instance_id, Latest=True)
            except RuntimeError as error:
                if not any(code in str(error) for code in ("InvalidInstanceID.NotFound", "IncorrectInstanceState")):
                    raise
                console = {}
            text = console_text(console.get("Output") or "")
            if f"ZC_RUNNER_READY={label}" in text:
                record["runner_ready"] = True
                save(record, "launch.json")
                print("One-job runner is listening for work", flush=True)
                return
            if "ZC_RUNNER_BOOTSTRAP_FAILED" in text:
                raise RuntimeError("Guest runner bootstrap failed")
            time.sleep(10)
        raise RuntimeError("Runner did not become ready within the bootstrap timeout")
    except BaseException:
        cleanup(config, run_id)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("launch", "cleanup"))
    args = parser.parse_args()
    config = json.loads(os.environ["RUNNER_CONFIG_JSON"])
    number, attempt = os.environ["GITHUB_RUN_ID"], os.environ["GITHUB_RUN_ATTEMPT"]
    assert number.isdigit() and attempt.isdigit()
    run_id = config["run_name_prefix"] + number + "-" + attempt
    assert re.fullmatch(r"[a-z0-9-]{1,64}", run_id)
    identity = aws(config, "sts", "get-caller-identity")
    role = os.environ["EXPECTED_ROLE_ARN"].rsplit("/", 1)[1]
    assert identity["Account"] == config["aws_account_id"]
    assert f":assumed-role/{role}/" in identity["Arn"]
    print("Controller identity:", identity["Arn"], flush=True)
    if args.command == "launch":
        launch(config, run_id, identity)
    else:
        cleanup(config, run_id)


if __name__ == "__main__":
    main()
