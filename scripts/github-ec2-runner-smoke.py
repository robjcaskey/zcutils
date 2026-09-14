#!/usr/bin/env python3
"""GitHub controller for one bounded, ephemeral EC2 Actions runner."""
import argparse
import base64
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
REGISTRY_IMAGE = "docker.io/library/registry@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373"


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


def boot_script(jit_config, label, lifetime_minutes, cache_volume_id="",
                cache_allow_format_empty=False, cache_enable_squid=False):
    assert re.fullmatch(r"zc-fips-(?:smoke|build)-[0-9a-f]{12}", label)
    assert 10 <= lifetime_minutes <= 45
    if cache_volume_id and not re.fullmatch(r"vol-[0-9a-f]{8}(?:[0-9a-f]{9})?", cache_volume_id):
        raise ValueError("cache volume ID is invalid")
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
shutdown -P +__SHUTDOWN_MINUTES__
useradd --create-home --shell /bin/bash gha
dnf -y install xfsprogs util-linux
__CACHE_SETUP__
dnf -y install tar gzip
if [[ '__LABEL__' == zc-fips-build-* ]]; then
  # Build prerequisites live in the disposable worker, not the Terraform stack.
  yum -y groupinstall 'Development Tools'
  yum -y install cmake3 golang rust cargo docker
  systemctl enable --now docker
  usermod -aG docker gha
  __REGISTRY_SETUP__
fi
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
EnvironmentFile=-/etc/zcutils-build-cache.env
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
    cache_setup = ""
    if cache_volume_id:
        by_id = "/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_" + cache_volume_id.replace("-", "")
        format_empty = "1" if cache_allow_format_empty else "0"
        enable_squid = "1" if cache_enable_squid else "0"
        cache_setup = f'''cache_device={by_id!r}
for attempt in $(seq 1 120); do [[ -b "$cache_device" ]] && break; sleep 1; done
[[ -b "$cache_device" ]]
cache_filesystem="$(blkid -s TYPE -o value "$cache_device" 2>/dev/null || true)"
if [[ -z "$cache_filesystem" && {format_empty} == 1 ]]; then
  # XFS labels are limited to 12 bytes.
  mkfs.xfs -L zcbuildcache "$cache_device"
  cache_filesystem=xfs
fi
[[ "$cache_filesystem" == xfs ]]
install -d -m 0755 /mnt/zcutils-build-cache
mount -o noatime "$cache_device" /mnt/zcutils-build-cache
install -d -o gha -g gha -m 0755 /mnt/zcutils-build-cache/cargo-registry /mnt/zcutils-build-cache/cargo-git
install -d -m 0755 /mnt/zcutils-build-cache/dnf /mnt/zcutils-build-cache/registry
install -d -o gha -g gha -m 0755 /home/gha/.cargo
ln -s /mnt/zcutils-build-cache/cargo-registry /home/gha/.cargo/registry
ln -s /mnt/zcutils-build-cache/cargo-git /home/gha/.cargo/git
printf '\ncachedir=/mnt/zcutils-build-cache/dnf\nkeepcache=True\n' >>/etc/dnf/dnf.conf
cat >/etc/zcutils-build-cache.env <<'CACHE_ENV'
ZCUTILS_DNF_CACHE_DIR=/mnt/zcutils-build-cache/dnf
CACHE_ENV
if [[ {enable_squid} == 1 ]]; then
  dnf -y install squid
  cat >/etc/squid/squid.conf <<'SQUID_CONFIG'
http_port 127.0.0.1:3128
acl localhost src 127.0.0.1/32 ::1
acl CONNECT method CONNECT
http_access allow localhost
http_access deny all
cache deny CONNECT
cache_dir rock /mnt/zcutils-build-cache/squid 4096 max-size=1048576
cache_mem 256 MB
maximum_object_size 1024 MB
SQUID_CONFIG
  install -d -o squid -g squid -m 0750 /mnt/zcutils-build-cache/squid
  squid -z
  systemctl enable --now squid
  cat >>/etc/zcutils-build-cache.env <<'PROXY_ENV'
HTTP_PROXY=http://127.0.0.1:3128
http_proxy=http://127.0.0.1:3128
NO_PROXY=127.0.0.1,localhost,169.254.169.254
no_proxy=127.0.0.1,localhost,169.254.169.254
PROXY_ENV
fi'''
    registry_setup = ""
    if cache_volume_id:
        registry_setup = f'''docker pull {REGISTRY_IMAGE}
docker run -d --name zcutils-build-cache-registry --restart unless-stopped \\
  -p 127.0.0.1:5000:5000 \\
  -v /mnt/zcutils-build-cache/registry:/var/lib/registry \\
  {REGISTRY_IMAGE}
for attempt in $(seq 1 30); do
  if curl -fsS http://127.0.0.1:5000/v2/ >/dev/null; then break; fi
  sleep 1
done
curl -fsS http://127.0.0.1:5000/v2/ >/dev/null
cat >>/etc/zcutils-build-cache.env <<'REGISTRY_ENV'
ZCUTILS_CACHE_REGISTRY=127.0.0.1:5000
ZCUTILS_CACHE_REF=127.0.0.1:5000/zcutils/zcblock-csi-buildcache:trusted
REGISTRY_ENV'''
    for key, value in (("RUNNER_URL", RUNNER_URL), ("RUNNER_SHA256", RUNNER_SHA256),
                       ("JIT_CONFIG", jit_config), ("LABEL", label),
                       ("SHUTDOWN_MINUTES", str(lifetime_minutes - 1)),
                       ("CACHE_SETUP", cache_setup),
                       ("REGISTRY_SETUP", registry_setup)):
        script = script.replace("__" + key + "__", value)
    if len(script.encode()) > 16384:
        raise ValueError("Runner configuration exceeds EC2's user-data limit")
    return script


def console_text(value):
    # AWS clients differ in whether they decode EC2's base64 console output.
    try:
        return base64.b64decode(value, validate=True).decode(errors="replace")
    except ValueError:
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


def cache_volume_for_launch(config, label):
    volume_id = os.environ.get("RUNNER_CACHE_VOLUME_ID", "").strip()
    expected_az = os.environ.get("RUNNER_CACHE_AVAILABILITY_ZONE", "").strip()
    if not volume_id:
        return None
    if not label.startswith("zc-fips-build-"):
        raise ValueError("persistent cache is allowed only for FIPS build runners")
    if not re.fullmatch(r"vol-[0-9a-f]{8}(?:[0-9a-f]{9})?", volume_id) or not expected_az:
        raise ValueError("cache requires one concrete volume ID and availability zone")
    subnet = aws(config, "ec2", "describe-subnets",
                 SubnetIds=[config["allowed_subnet_ids"][0]])["Subnets"]
    if len(subnet) != 1 or subnet[0].get("AvailabilityZone") != expected_az:
        raise ValueError("runner subnet and cache volume must use the configured availability zone")
    volumes = aws(config, "ec2", "describe-volumes", VolumeIds=[volume_id]).get("Volumes", [])
    if len(volumes) != 1:
        raise ValueError("expected exactly one cache volume")
    volume = volumes[0]
    expected = {"VolumeId": volume_id, "AvailabilityZone": expected_az,
                "VolumeType": "gp3", "Size": 20, "Encrypted": True,
                "MultiAttachEnabled": False, "State": "available"}
    for key, value in expected.items():
        if volume.get(key) != value:
            raise ValueError(f"cache volume {key} mismatch")
    if volume.get("Attachments"):
        raise ValueError("cache volume is already attached")
    tags = {item["Key"]: item["Value"] for item in volume.get("Tags", [])}
    if (tags.get("RunnerPool") != config["runner_pool"]
            or tags.get("ZcutilsBuildCacheAuthority") != "Rob-J-Caskey"
            or tags.get("ZcutilsSingleWriter") != "true"):
        raise ValueError("cache volume tags do not authorize this runner pool")
    initialize = os.environ.get("RUNNER_CACHE_ALLOW_FORMAT_EMPTY", "false").strip().lower()
    enable_squid = os.environ.get("RUNNER_CACHE_ENABLE_SQUID", "false").strip().lower()
    if initialize not in ("true", "false") or enable_squid not in ("true", "false"):
        raise ValueError("runner cache boolean variables must be true or false")
    return {"volume_id": volume_id, "availability_zone": expected_az,
            "allow_format_empty": initialize == "true",
            "enable_squid": enable_squid == "true"}


def wait_for_cache_attachment(config, volume_id, instance_id, state, timeout=120):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        volumes = aws(config, "ec2", "describe-volumes", VolumeIds=[volume_id]).get("Volumes", [])
        attachments = volumes[0].get("Attachments", []) if len(volumes) == 1 else []
        if state == "attached" and len(attachments) == 1:
            attachment = attachments[0]
            if attachment.get("InstanceId") == instance_id and attachment.get("State") == "attached":
                return
        if state == "available" and len(volumes) == 1 and volumes[0].get("State") == "available" and not attachments:
            return
        time.sleep(3)
    raise RuntimeError(f"cache volume did not reach {state}")


def wait_for_instance_running(config, instance_id, expected_az, timeout=180):
    """Wait until EC2 permits attachment and confirm the instance stayed in the cache AZ."""
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        result = aws(config, "ec2", "describe-instances", InstanceIds=[instance_id])
        instances = [instance for reservation in result.get("Reservations", [])
                     for instance in reservation.get("Instances", [])]
        if len(instances) == 1:
            instance = instances[0]
            state = instance.get("State", {}).get("Name")
            if state == "running":
                if instance.get("Placement", {}).get("AvailabilityZone") != expected_az:
                    raise RuntimeError("launched runner is not in the cache volume availability zone")
                return
            if state in ("shutting-down", "terminated", "stopping"):
                raise RuntimeError(f"runner entered {state} before cache attachment")
        time.sleep(3)
    raise RuntimeError("runner did not reach running state before cache attachment timeout")


def cleanup(config, run_id, termination_timeout=180):
    instances = instances_for_run(config, run_id)
    ids = [i["InstanceId"] for i in instances]
    if ids:
        aws(config, "ec2", "terminate-instances", InstanceIds=ids)
        end = time.monotonic() + termination_timeout
        while time.monotonic() < end:
            states = instances_for_run(config, run_id)
            if all(i["State"]["Name"] == "terminated" for i in states):
                break
            time.sleep(5)
        else:
            raise RuntimeError("Termination not confirmed; keeping the deadline schedule")
    cache_volume_id = os.environ.get("RUNNER_CACHE_VOLUME_ID", "").strip()
    if cache_volume_id:
        wait_for_cache_attachment(config, cache_volume_id, ids[0] if ids else "", "available")
    try:
        aws(config, "scheduler", "delete-schedule", Name=run_id,
            GroupName=config["schedule_group_name"])
    except RuntimeError as error:
        if "ResourceNotFoundException" not in str(error):
            raise
    report = {"run_id": run_id, "instance_ids": ids, "termination_confirmed": True,
              "schedule_removed": True}
    if cache_volume_id:
        report["cache_volume_id"] = cache_volume_id
        report["cache_volume_available"] = True
    save(report, "cleanup.json")
    print(json.dumps(report), flush=True)


def launch(config, run_id, identity):
    label = os.environ["RUNNER_LABEL"]
    jit = os.environ.pop("RUNNER_JIT_CONFIG").strip()
    if not jit:
        raise ValueError("Missing one-job runner configuration; run test-github-ec2-runner.py first")
    image_id = os.environ.get("RUNNER_IMAGE_ID", "")
    instance_type = os.environ.get("RUNNER_INSTANCE_TYPE", "")
    if not re.fullmatch(r"ami-[0-9a-f]{8}(?:[0-9a-f]{9})?", image_id):
        raise ValueError("workflow must select one concrete AMI")
    if not re.fullmatch(r"[a-z0-9-]+[.][a-z0-9]+", instance_type):
        raise ValueError("workflow must select one concrete instance type")
    lifetime_minutes = int(os.environ.get("RUNNER_LIFETIME_MINUTES", "15"))
    if not 10 <= lifetime_minutes <= 45:
        raise ValueError("runner lifetime must be between 10 and 45 minutes")
    images = aws(config, "ec2", "describe-images", ImageIds=[image_id]).get("Images", [])
    if (len(images) != 1 or images[0].get("State") != "available"
            or images[0].get("Architecture") != "x86_64"):
        raise ValueError("requested AMI is not one available x86_64 image")
    root_device = images[0].get("RootDeviceName")
    if not root_device:
        raise ValueError("requested AMI does not report a root device")
    cache_volume = cache_volume_for_launch(config, label)
    script = boot_script(jit, label, lifetime_minutes,
                         cache_volume["volume_id"] if cache_volume else "",
                         cache_volume["allow_format_empty"] if cache_volume else False,
                         cache_volume["enable_squid"] if cache_volume else False)
    deadline = datetime.now(timezone.utc) + timedelta(minutes=lifetime_minutes)
    expiry = deadline.strftime("%Y-%m-%dT%H:%M:%SZ")
    tag_values = {**config["required_tag_values"], "RunId": run_id, "Name": run_id,
                  **{key: expiry for key in config["expiry_tag_keys"]}}
    if tag_values.get("adhocKeepaliveModeAction") != "terminate":
        raise ValueError("Ad hoc termination enrollment is required")
    tags = [{"Key": key, "Value": value} for key, value in tag_values.items()]
    request = {
        "ImageId": image_id, "InstanceType": instance_type,
        "MinCount": 1, "MaxCount": 1, "ClientToken": run_id,
        "MetadataOptions": {"HttpTokens": "required", "HttpEndpoint": "enabled"},
        "InstanceInitiatedShutdownBehavior": "terminate",
        "NetworkInterfaces": [{"DeviceIndex": 0, "SubnetId": config["allowed_subnet_ids"][0],
                               "Groups": config["allowed_security_group_ids"],
                               "AssociatePublicIpAddress": True, "DeleteOnTermination": True}],
        "BlockDeviceMappings": [{"DeviceName": root_device, "Ebs": {
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
        # Establish and verify the independent AWS deadline immediately. Any later
        # cache or guest failure is then bounded even if controller cleanup fails.
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
        if cache_volume:
            wait_for_instance_running(config, instance_id, cache_volume["availability_zone"])
            aws(config, "ec2", "attach-volume", VolumeId=cache_volume["volume_id"],
                InstanceId=instance_id, Device="/dev/sdf")
            wait_for_cache_attachment(config, cache_volume["volume_id"], instance_id, "attached")
            record["cache_volume"] = cache_volume
        save(record, "launch.json")
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            output.write(f"instance_id={instance_id}\nrunner_label={label}\nrun_id={run_id}\n")
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
        termination_timeout = int(os.environ.get("RUNNER_CLEANUP_TIMEOUT_SECONDS", "720"))
        if not 180 <= termination_timeout <= 900:
            raise ValueError("cleanup timeout must be between 180 and 900 seconds")
        cleanup(config, run_id, termination_timeout=termination_timeout)


if __name__ == "__main__":
    main()
