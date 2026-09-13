#!/usr/bin/env python3
"""Local FIPS-aspiring image tests. Never issues a validation claim.

An outer Debian host is supported. Guests must be genuine, independently
provisioned FIPS-enabled OS images; no custom kernel or fake FIPS flag is used.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


def run(argv, **kwargs):
    return subprocess.run([str(x) for x in argv], check=True, text=True, **kwargs)


def sha256(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def emit(value, report=None):
    value["validation_claim"] = "none; runtime compatibility and mode evidence only"
    text = json.dumps(value, indent=2) + "\n"
    if report:
        Path(report).write_text(text)
    print(text, end="")


def preflight(args):
    problems = []
    for tool in ("qemu-system-x86_64", "qemu-img", "ssh", "scp"):
        if not shutil.which(tool):
            problems.append(f"missing {tool}")
    if not os.access("/dev/kvm", os.R_OK | os.W_OK):
        problems.append("/dev/kvm is unavailable: KVM-backed FIPS guest testing is blocked")
    base = getattr(args, "base", None)
    if base and not Path(base).is_file():
        problems.append("supplied guest base does not exist")
    if not base and args.target in ("rhel", "openshift"):
        problems.append("supply a genuine FIPS-installed RHEL/RHCOS guest; UBI is not a guest OS")
    if not base and args.target == "ubuntu":
        problems.append("supply an Ubuntu guest with Pro FIPS packages enabled and rebooted")
    if args.target == "openshift":
        problems.append("OpenShift requires a pull secret and fips:true at installation; OKD/CRC is not substituted")
    emit({"target": args.target, "host_os": Path("/etc/os-release").read_text(),
          "prerequisites": problems, "ready": not problems}, args.report)
    return 1 if problems else 0


def boot(args):
    base = Path(args.base).resolve(strict=True)
    if not re.fullmatch(r"[0-9a-f]{64}", args.sha256) or sha256(base) != args.sha256:
        raise ValueError("base image SHA-256 mismatch")
    if not os.access("/dev/kvm", os.R_OK | os.W_OK):
        raise ValueError("KVM is required; refusing an unreported TCG substitution")
    work = Path(args.work_dir).resolve()
    if any(c in str(work) for c in (",", "\n", "\r")):
        raise ValueError("QEMU work path must not contain commas or newlines")
    work.mkdir(parents=True, exist_ok=True)
    disk = work / "guest.qcow2"
    if disk.exists():
        raise ValueError("work directory already has a guest disk; use a fresh directory")
    if not 1024 <= args.ssh_port <= 65535:
        raise ValueError("SSH port must be between 1024 and 65535")
    if args.cpus < 1 or args.memory_mib < 1024:
        raise ValueError("guest requires positive CPU count and at least 1024 MiB")
    # Format is explicit: never probe untrusted images or follow a supplied
    # backing chain. Only standalone, already installed qcow2 bases are accepted.
    info = json.loads(run(["qemu-img", "info", "--output=json", "-f", "qcow2", base], capture_output=True).stdout)
    if info.get("backing-filename"):
        raise ValueError("base must be standalone, without a backing image")
    run(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", base, disk])
    emit({"base": str(base), "base_sha256": args.sha256, "target": args.target,
          "cpu": "host", "acceleration": "kvm", "ssh_port": args.ssh_port}, work / "boot.json")
    # Foreground lifetime belongs to the invoking terminal; no pattern cleanup.
    # User networking intentionally serves standalone RHEL/Ubuntu guests only.
    # OpenShift requires its own working cluster network and is checked separately.
    run(["qemu-system-x86_64", "-name", "zc-fips-lab", "-enable-kvm", "-cpu", "host",
         "-smp", str(args.cpus), "-m", str(args.memory_mib), "-nographic",
         "-drive", f"file={disk},if=virtio,format=qcow2",
         "-netdev", f"user,id=net0,hostfwd=tcp:127.0.0.1:{args.ssh_port}-:22",
         "-device", "virtio-net-pci,netdev=net0"])
    return 0


def evaluate_guest(node, probe, expected):
    errors = []
    ids = {"ubuntu": {"ubuntu"}, "rhel": {"rhel"}}
    if node.get("os_id") not in ids[expected]:
        errors.append("guest distribution does not match requested target")
    if node.get("fips_enabled") != "1":
        errors.append("guest kernel FIPS mode is not enabled")
    for field in ("passed", "fips_feature", "aws_lc_fips_mode", "tls_provider_fips", "host_fips_enabled"):
        if probe.get(field) is not True:
            errors.append(f"container probe did not establish {field}")
    return errors


def guest(args):
    if not re.fullmatch(r"[a-z_][a-z0-9_-]*", args.user):
        raise ValueError("invalid SSH user")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", args.image_id):
        raise ValueError("image-id must be the immutable local image ID sha256:<64 hex>")
    work = Path(args.work_dir).resolve()
    work.mkdir(parents=True, exist_ok=True)
    key = Path(args.ssh_key).resolve(strict=True)
    archive = Path(args.archive).resolve(strict=True)
    # Only a loopback SSH connection to a forwarded local guest is permitted.
    common = ["-F", "/dev/null", "-i", str(key), "-o", "BatchMode=yes",
              "-o", "IdentitiesOnly=yes", "-o", "ConnectTimeout=10",
              "-o", "StrictHostKeyChecking=accept-new", "-o", f"UserKnownHostsFile={work / 'known_hosts'}"]
    endpoint = f"{args.user}@127.0.0.1"
    ssh = ["ssh", *common, "-p", str(args.ssh_port), endpoint]
    raw = run([*ssh, "sh -s"], input='''set -eu
. /etc/os-release
printf '%s\\n' "$ID" "$(uname -r)" "$(uname -m)" "$(cat /proc/sys/crypto/fips_enabled 2>/dev/null || true)"
''', capture_output=True).stdout.splitlines()
    if len(raw) != 4:
        raise ValueError("unexpected guest identity response")
    node = dict(zip(("os_id", "kernel", "architecture", "fips_enabled"), raw))
    if node["fips_enabled"] != "1":
        emit({"node": node, "passed": False, "errors": ["guest is not FIPS enabled; no image loaded"]}, args.report)
        return 1
    # Use a content-addressed, predictable-safe basename; never interpolate user
    # image names or paths into remote shell commands.
    archive_hash = sha256(archive)
    remote = f"/tmp/zc-fips-{archive_hash}.tar"
    run(["scp", "-O", *common, "-P", str(args.ssh_port), archive, f"{endpoint}:{remote}"])
    command = f'''set -eu
printf '%s  %s\\n' '{archive_hash}' '{remote}' | sha256sum -c - >/dev/null
sudo -n podman load -i '{remote}' >&2
sudo -n podman run --rm --network none --entrypoint /usr/local/bin/zc-fips-check '{args.image_id}' --require-fips --require-host-fips
'''
    proc = subprocess.run([*ssh, "sh -s"], input=command, text=True, capture_output=True)
    try:
        probe = json.loads(proc.stdout)
    except json.JSONDecodeError:
        probe = {"passed": False, "error": proc.stderr[-2000:]}
    errors = evaluate_guest(node, probe, args.target)
    if proc.returncode:
        errors.append(f"guest probe exited {proc.returncode}")
    emit({"node": node, "image_id": args.image_id, "archive_sha256": archive_hash,
          "probe": probe, "passed": not errors, "errors": errors}, args.report)
    return int(bool(errors))


def cluster(args):
    # Read-only against an explicitly selected context: probe the first-party
    # container already installed on each node. No cluster is created or changed.
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", args.image_digest):
        raise ValueError("image-digest must be sha256:<64 hex>")
    oc = ["oc", "--context", args.context]
    version = json.loads(run([*oc, "get", "clusterversion", "version", "-o", "json"], capture_output=True).stdout)
    nodes = json.loads(run([*oc, "get", "nodes", "-o", "json"], capture_output=True).stdout)["items"]
    pods = json.loads(run([*oc, "-n", args.namespace, "get", "pods", "-l", args.selector, "-o", "json"], capture_output=True).stdout)["items"]
    evidence, errors = [], []
    if not nodes:
        errors.append("cluster has no nodes")
    for node in nodes:
        name = node["metadata"]["name"]
        os_image = node.get("status", {}).get("nodeInfo", {}).get("osImage", "")
        if not os_image.startswith("Red Hat Enterprise Linux"):
            errors.append(f"{name}: expected RHEL/RHCOS, found {os_image!r}")
        candidates = [p for p in pods if p.get("spec", {}).get("nodeName") == name and p.get("status", {}).get("phase") == "Running"]
        if len(candidates) != 1:
            errors.append(f"{name}: expected exactly one running CSI pod")
            continue
        pod = candidates[0]
        statuses = pod.get("status", {}).get("containerStatuses", [])
        status = next((s for s in statuses if s["name"] == args.container), {})
        image_id = status.get("imageID", "")
        if image_id.split("@")[-1] != args.image_digest:
            errors.append(f"{name}: container digest differs from requested release")
        proc = subprocess.run([*oc, "-n", args.namespace, "exec", pod["metadata"]["name"], "-c", args.container,
                               "--", "/usr/local/bin/zc-fips-check", "--require-fips", "--require-host-fips"], text=True, capture_output=True)
        try:
            probe = json.loads(proc.stdout)
        except json.JSONDecodeError:
            probe = {"passed": False, "error": proc.stderr[-2000:]}
        if proc.returncode or any(probe.get(f) is not True for f in ("passed", "fips_feature", "aws_lc_fips_mode", "tls_provider_fips", "host_fips_enabled")):
            errors.append(f"{name}: FIPS runtime probe failed")
        evidence.append({"node": name, "node_info": node["status"]["nodeInfo"], "image_id": image_id, "probe": probe})
    emit({"cluster_version": version.get("status", {}).get("desired"), "nodes": evidence,
          "scope": "first-party provider mode only; sidecar and service coverage remain unaudited",
          "passed": not errors, "errors": errors}, args.report)
    return int(bool(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("preflight")
    p.add_argument("--target", choices=("ubuntu", "rhel", "openshift"), required=True)
    p.add_argument("--base", help="already provisioned standalone guest qcow2 (runtime mode checked separately)")
    p.add_argument("--report")
    p.set_defaults(func=preflight)
    p = sub.add_parser("boot", help="boot an already installed Ubuntu/RHEL FIPS guest in a new overlay")
    p.add_argument("--target", choices=("ubuntu", "rhel"), required=True)
    p.add_argument("--base", required=True)
    p.add_argument("--sha256", required=True)
    p.add_argument("--work-dir", required=True)
    p.add_argument("--ssh-port", type=int, default=2244)
    p.add_argument("--cpus", type=int, default=4)
    p.add_argument("--memory-mib", type=int, default=8192)
    p.set_defaults(func=boot)
    p = sub.add_parser("guest", help="load an OCI archive and probe it inside a local FIPS guest")
    p.add_argument("--target", choices=("ubuntu", "rhel"), required=True)
    p.add_argument("--work-dir", required=True)
    p.add_argument("--ssh-key", required=True)
    p.add_argument("--user", default="lab")
    p.add_argument("--ssh-port", type=int, default=2244)
    p.add_argument("--archive", required=True)
    p.add_argument("--image-id", required=True)
    p.add_argument("--report", required=True)
    p.set_defaults(func=guest)
    p = sub.add_parser("openshift", help="read-only probes of an explicitly selected installed cluster")
    p.add_argument("--context", required=True)
    p.add_argument("--namespace", default="zcblock-csi")
    p.add_argument("--selector", default="app.kubernetes.io/name=zcblock-csi")
    p.add_argument("--container", default="zcblock-csi")
    p.add_argument("--image-digest", required=True)
    p.add_argument("--report", required=True)
    p.set_defaults(func=cluster)
    args = parser.parse_args()
    try:
        return args.func(args)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        emit({"passed": False, "error": str(error)}, getattr(args, "report", None))
        return 1


if __name__ == "__main__":
    sys.exit(main())
