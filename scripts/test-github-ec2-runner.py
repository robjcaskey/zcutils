#!/usr/bin/env python3
"""Mint a one-job runner config, dispatch a bounded workflow, and verify its artifacts."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
import uuid


def gh(args, payload=None, allow_missing=False):
    result = subprocess.run(["gh", *args], input=payload, text=True, capture_output=True, timeout=60)
    if result.returncode:
        if allow_missing and "404" in result.stderr:
            return None
        raise RuntimeError(result.stderr.strip())
    return result.stdout


def api(path, method="GET", body=None, allow_missing=False):
    args = ["api", path, "--method", method]
    payload = None
    if body is not None:
        args += ["--input", "-"]
        payload = json.dumps(body)
    text = gh(args, payload, allow_missing=allow_missing)
    return json.loads(text) if text else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default="robjcaskey/zcutils")
    parser.add_argument("--output-dir", type=Path, default=Path("target/github-ec2-runner-smoke"))
    parser.add_argument("--fips-build", action="store_true",
                        help="run the certificate-5314 provider/link workflow instead of the cheap smoke test")
    parser.add_argument("--compare-online", action="store_true", help="explicit FIPS reproducibility experiment")
    parser.add_argument('--effective-build-timestamp', type=int)
    parser.add_argument('--expected-unsigned-executable-bundle-sha256')
    parser.add_argument("--aws-profile", default="slopmud-breakglass",
                        help="local profile used to verify KMS-backed FIPS image signatures")
    args = parser.parse_args()
    suffix = uuid.uuid4().hex[:12]
    label = ("zc-fips-build-" if args.fips_build else "zc-fips-smoke-") + suffix
    secret = "FIPS_EPHEMERAL_JIT_" + suffix.upper()
    out = args.output_dir / label
    out.mkdir(mode=0o700, parents=True, exist_ok=False)
    endpoint = f"repos/{args.repo}/actions"
    workflow = "fips-aws-lc-5314.yml" if args.fips_build else "fips-ec2-runner-smoke.yml"
    report = {"repo": args.repo, "label": label, "secret_name": secret,
              "workflow": workflow}
    runner_id, run_id, complete = None, None, False

    def save():
        (out / "report.json").write_text(json.dumps(report, indent=2) + "\n")

    try:
        # Existing local gh authentication has repository administration access.
        # Only the one-job JIT configuration is stored temporarily in Actions.
        response = api(endpoint + "/runners/generate-jitconfig", "POST", {
            "name": label, "runner_group_id": 1,
            "labels": [label], "work_folder": "_work",
        })
        runner_id = response["runner"]["id"]
        report["runner_id"] = runner_id
        # The REST runner summary omits ephemeral; inspect the actual settings.
        files = json.loads(base64.b64decode(response["encoded_jit_config"], validate=True))
        settings = json.loads(base64.b64decode(files[".runner"], validate=True))
        settings = {key.casefold(): value for key, value in settings.items()}
        if str(settings.get("ephemeral")).lower() != "true" or settings.get("agentname") != label:
            raise ValueError("GitHub did not issue the expected one-job runner configuration")
        gh(["secret", "set", secret, "--repo", args.repo], response["encoded_jit_config"])
        del response
        save()
        inputs = {"runner_label": label, "jit_secret_name": secret}
        if args.fips_build:
            inputs["compare_online"] = args.compare_online
            if args.effective_build_timestamp is not None:
                inputs['effective_build_timestamp'] = str(args.effective_build_timestamp)
            if args.expected_unsigned_executable_bundle_sha256:
                inputs['expected_unsigned_executable_bundle_sha256'] = args.expected_unsigned_executable_bundle_sha256
        api(endpoint + f"/workflows/{workflow}/dispatches", "POST", {
            "ref": "main", "inputs": inputs,
        })
        for _ in range(30):
            runs = api(endpoint + f"/workflows/{workflow}/runs?event=workflow_dispatch&per_page=20")
            found = [r for r in runs["workflow_runs"] if label in r["display_title"]]
            if found:
                run_id = found[0]["id"]
                report.update(run_id=run_id, url=found[0]["html_url"])
                break
            time.sleep(2)
        if run_id is None:
            raise RuntimeError("Dispatched run did not appear within one minute")
        save()
        print(report["url"], flush=True)
        # The worker is independently capped at 45 minutes, but GitHub-hosted
        # launch and cleanup jobs run outside that lifetime.  Leave enough
        # monitoring time to observe cleanup after the worker has terminated.
        end = time.monotonic() + (120 if args.fips_build else 20) * 60
        previous = None
        while time.monotonic() < end:
            run = api(endpoint + f"/runs/{run_id}")
            jobs = api(endpoint + f"/runs/{run_id}/jobs?per_page=100")["jobs"]
            state = [(j["name"], j["status"], j["conclusion"]) for j in jobs]
            if state != previous:
                print(json.dumps(state), flush=True)
                previous = state
            if run["status"] == "completed":
                complete = True
                report.update(conclusion=run["conclusion"], jobs=state)
                break
            time.sleep(10)
        if not complete:
            raise RuntimeError("Workflow exceeded the local monitoring deadline")
        save()
        artifacts = api(endpoint + f"/runs/{run_id}/artifacts")["artifacts"]
        report["artifacts"] = [{k: a[k] for k in ("id", "name", "size_in_bytes")} for a in artifacts]
        if artifacts:
            gh(["run", "download", str(run_id), "--repo", args.repo, "--dir", str(out / "artifacts")])
        if report["conclusion"] != "success":
            log = gh(["run", "view", str(run_id), "--repo", args.repo, "--log-failed"])
            (out / "failed-jobs.log").write_text(log)
            raise RuntimeError("Workflow failed; evidence saved in " + str(out))
        launch_prefix = "5314-runner-launch" if args.fips_build else "runner-launch"
        cleanup_prefix = "5314-runner-cleanup" if args.fips_build else "runner-cleanup"
        launch = json.loads((out / "artifacts" / f"{launch_prefix}-{run_id}" / "launch.json").read_text())
        cleanup = json.loads((out / "artifacts" / f"{cleanup_prefix}-{run_id}" / "cleanup.json").read_text())
        assert cleanup["termination_confirmed"] and cleanup["schedule_removed"]
        assert launch["instance_id"] in cleanup["instance_ids"]
        if args.fips_build:
            artifact = out / "artifacts" / f"aws-lc-fips-5314-x86_64-{run_id}"
            subprocess.run(["sha256sum", "-c", "evidence/files.sha256"], cwd=artifact,
                           check=True, stdout=subprocess.DEVNULL)
            provider = json.loads((artifact / "provider/share/zcutils/fips/provider-receipt.json").read_text())
            receipt = json.loads((artifact / "evidence/build-receipt.json").read_text())
            assert provider["certificate_number"] == 5314
            assert provider["certificate_profile_environment"] is True
            assert provider["source"]["archive_sha256"] == \
                "fe408fa438850786396faf79eba9ea4116c3802e60f3a95865f0dd2adb64c9f1"
            assert receipt["recompilation_assessment"]["status"] == "section-11.1-linked"
            assert receipt["binaries"]["zc-fips-check"]["static_identity_symbols"] == \
                ["awslc_version_string"]
            attestations = artifact / "image-attestations"
            manifest = json.loads((attestations / "zcblock-csi-fips-aspiring.attestation-manifest.json").read_text())
            assert manifest["signed"] is True
            assert manifest["fipsRuntimeCheck"] == "provider-only"
            assert manifest["imageSignature"]["signingAuthority"] == "Rob J. Caskey"
            image_ref = (artifact / "evidence/fips-image-ref.txt").read_text().strip()
            assert manifest["subject"]["name"] == image_ref
            verify_env = {**os.environ, "AWS_PROFILE": args.aws_profile, "AWS_REGION": "us-east-1"}
            kms_key = "awskms:///alias/zcutils-build-attestation-signing-authority-Rob-J-Caskey"
            subprocess.run([
                "python3", "scripts/zc-image-attest.py", "verify",
                "--variant", "fips-aspiring", "--output-dir", str(attestations),
                "--require-signature", "--cosign-verification-key", kms_key,
            ], check=True, env=verify_env)
            subprocess.run([
                "cosign", "verify", "--key", kms_key,
                "-a", "signingAuthority=Rob J. Caskey",
                "-a", f"builderIdentity={manifest['imageSignature']['builderIdentity']}",
                manifest["imageSignature"]["resolvedRef"],
            ], check=True, env=verify_env, stdout=subprocess.DEVNULL)
            report.update(instance_id=launch["instance_id"], downloaded_and_verified=True,
                          provider_libcrypto_sha256=provider["provider"]["libcrypto_sha256"],
                          linked_binary_sha256=receipt["binaries"]["zc-fips-check"]["sha256"],
                          fips_image=image_ref,
                          unsigned_executable_bundle_sha256=manifest['payload']['digest']['sha256'],
                          effective_build_timestamp=manifest['payload']['effective_build_timestamp'],
                          fips_image_digest=manifest["subject"]["digest"]["sha256"])
            print("Downloaded and verified:", artifact, flush=True)
        else:
            hello = out / "artifacts" / f"hello-world-{run_id}"
            assert (hello / "hello.txt").read_text() == "Hello, world!\n"
            worker = json.loads((hello / "worker.json").read_text())
            assert worker["instance_id"] == launch["instance_id"]
            report.update(instance_id=worker["instance_id"], downloaded_and_verified=True,
                          hello_sha256=hashlib.sha256((hello / "hello.txt").read_bytes()).hexdigest())
            print("Downloaded and verified:", hello / "hello.txt", flush=True)
    finally:
        if run_id is not None and not complete:
            try:
                api(endpoint + f"/runs/{run_id}/cancel", "POST")
            except RuntimeError:
                pass
        try:
            if runner_id is not None:
                # JIT runners normally remove themselves after their single job.
                runner = api(endpoint + f"/runners/{runner_id}", allow_missing=True)
                if runner is not None and not runner["busy"]:
                    api(endpoint + f"/runners/{runner_id}", "DELETE", allow_missing=True)
                report["runner_removed"] = api(endpoint + f"/runners/{runner_id}", allow_missing=True) is None
        finally:
            try:
                api(endpoint + f"/secrets/{secret}", "DELETE", allow_missing=True)
                report["temporary_secret_removed"] = True
            finally:
                save()
                print("Evidence:", out, flush=True)

    assert report["runner_removed"], "Runner registration cleanup needs inspection"


if __name__ == "__main__":
    main()
