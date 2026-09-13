#!/usr/bin/env python3
"""Mint a one-job runner config, dispatch the smoke workflow, and download its artifacts."""
import argparse
import base64
import hashlib
import json
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
    args = parser.parse_args()
    suffix = uuid.uuid4().hex[:12]
    label = "zc-fips-smoke-" + suffix
    secret = "FIPS_EPHEMERAL_JIT_" + suffix.upper()
    out = args.output_dir / label
    out.mkdir(mode=0o700, parents=True, exist_ok=False)
    endpoint = f"repos/{args.repo}/actions"
    workflow = "fips-ec2-runner-smoke.yml"
    report = {"repo": args.repo, "label": label, "secret_name": secret}
    runner_id, run_id, complete = None, None, False

    def save():
        (out / "report.json").write_text(json.dumps(report, indent=2) + "\n")

    try:
        # Existing local gh authentication has repository administration access.
        # Only the one-job JIT configuration is stored temporarily in Actions.
        response = api(endpoint + "/runners/generate-jitconfig", "POST", {
            "name": label, "runner_group_id": 1,
            "labels": ["self-hosted", "Linux", "X64", label], "work_folder": "_work",
        })
        runner_id = response["runner"]["id"]
        report["runner_id"] = runner_id
        # The REST runner summary omits ephemeral; inspect the actual settings.
        files = json.loads(base64.b64decode(response["encoded_jit_config"], validate=True))
        settings = json.loads(base64.b64decode(files[".runner"], validate=True))
        if settings.get("ephemeral") is not True or settings.get("agentName") != label:
            raise ValueError("GitHub did not issue the expected one-job runner configuration")
        gh(["secret", "set", secret, "--repo", args.repo], response["encoded_jit_config"])
        del response
        save()
        api(endpoint + f"/workflows/{workflow}/dispatches", "POST", {
            "ref": "main", "inputs": {"runner_label": label, "jit_secret_name": secret},
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
        end = time.monotonic() + 20 * 60
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
        hello = out / "artifacts" / f"hello-world-{run_id}"
        assert (hello / "hello.txt").read_text() == "Hello, world!\n"
        worker = json.loads((hello / "worker.json").read_text())
        launch = json.loads((out / "artifacts" / f"runner-launch-{run_id}" / "launch.json").read_text())
        cleanup = json.loads((out / "artifacts" / f"runner-cleanup-{run_id}" / "cleanup.json").read_text())
        assert worker["instance_id"] == launch["instance_id"]
        assert cleanup["termination_confirmed"] and cleanup["schedule_removed"]
        assert worker["instance_id"] in cleanup["instance_ids"]
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
