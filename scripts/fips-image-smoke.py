#!/usr/bin/env python3
"""Verify built images without mounts, devices, networking, or a cluster."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", action="append", required=True)
    parser.add_argument("--storage", help="optional isolated Podman storage directory")
    parser.add_argument("--report", required=True)
    args = parser.parse_args()
    engine = ["podman"]
    if args.storage:
        engine += ["--root", str(Path(args.storage) / "root"), "--runroot", str(Path(args.storage) / "run")]

    def invoke(argv):
        if argv[0] != "run":
            return subprocess.run(engine + argv, text=True, capture_output=True, timeout=60)
        # A failed startup guard must not leave a running service behind after
        # the CLI times out. Cleanup only the container created by this call.
        with tempfile.TemporaryDirectory(prefix="zc-fips-smoke-") as directory:
            cidfile = Path(directory) / "container.id"
            try:
                return subprocess.run(engine + ["run", "--cidfile", str(cidfile)] + argv[1:],
                                      text=True, capture_output=True, timeout=60)
            finally:
                if cidfile.exists():
                    cid = cidfile.read_text().strip()
                    if len(cid) == 64 and all(c in "0123456789abcdef" for c in cid):
                        subprocess.run(engine + ["rm", "--force", "--ignore", cid],
                                       text=True, capture_output=True, timeout=15)

    records = []
    for image in args.image:
        record = {"image": image, "passed": False}
        records.append(record)
        try:
            inspected = invoke(["image", "inspect", image])
            if inspected.returncode:
                raise ValueError(inspected.stderr)
            metadata = json.loads(inspected.stdout)[0]
            record["image_id"] = metadata["Id"]
            record["image_digest"] = metadata.get("Digest")
            record["labels"] = metadata["Config"].get("Labels")
            immutable = metadata["Id"]
            probe_args = ["run", "--rm", "--network", "none", "--entrypoint", "/usr/local/bin/zc-fips-check", immutable, "--require-fips"]
            probe = invoke(probe_args)
            record["provider_probe"] = json.loads(probe.stdout)
            if probe.returncode or any(record["provider_probe"].get(f) is not True for f in ("passed", "fips_feature", "aws_lc_fips_mode", "tls_provider_fips")):
                raise ValueError("provider probe failed")
            strict = invoke(probe_args + ["--require-host-fips"])
            record["strict_probe"] = json.loads(strict.stdout)
            host_fips = record["provider_probe"].get("host_fips_enabled") is True
            if strict.returncode != (0 if host_fips else 1) or record["strict_probe"].get("passed") is not host_fips:
                raise ValueError("strict probe did not enforce the actual host mode")
            if not host_fips:
                startup = invoke(["run", "--rm", "--network", "none", immutable])
                record["strict_entrypoint_exit"] = startup.returncode
                if startup.returncode != 1 or "kernel FIPS mode is not enabled" not in startup.stderr:
                    raise ValueError("CSI entrypoint did not reject a non-FIPS host")
            checksum = invoke(["run", "--rm", "--network", "none", "--entrypoint", "/bin/sh", immutable,
                               "-c", "cd /usr/local/bin && sha256sum -c /usr/share/zcutils/fips/binaries.sha256"])
            record["binary_checksums"] = checksum.stdout.splitlines()
            if checksum.returncode:
                raise ValueError("packaged binary hashes failed")
            # Explicit provider-only startup, inside an isolated container. No
            # volumes are provisioned and no host storage devices are exposed.
            script = '''set -eu
zcblock-csi --node-id fips-image-smoke --endpoint unix:///tmp/fips-csi.sock --state-dir /tmp/fips-state --control-socket /tmp/fips-control.sock >/tmp/csi.log 2>&1 &
child=$!
trap 'kill "$child" 2>/dev/null || true' EXIT
for attempt in 1 2 3 4 5; do
  if test -S /tmp/fips-csi.sock && kill -0 "$child"; then cat /tmp/csi.log; exit 0; fi
  sleep 1
done
cat /tmp/csi.log
exit 1
'''
            startup = invoke(["run", "--rm", "--network", "none", "-e", "ZC_REQUIRE_HOST_FIPS=0",
                              "--entrypoint", "/bin/sh", immutable, "-c", script])
            record["provider_only_csi_startup"] = {"exit": startup.returncode, "log": startup.stdout}
            if startup.returncode:
                raise ValueError("provider-only CSI listener did not start: " + startup.stderr)
            record["passed"] = True
        except (OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
            record["error"] = str(error)
    report = {"validation_claim": "none; application-provider and listener smoke tests only",
              "full_storage_acceptance": "not run", "fips_guest_acceptance": "not run",
              "openshift_acceptance": "not run", "images": records,
              "passed": all(r["passed"] for r in records)}
    Path(args.report).write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return int(not report["passed"])


if __name__ == "__main__":
    raise SystemExit(main())
