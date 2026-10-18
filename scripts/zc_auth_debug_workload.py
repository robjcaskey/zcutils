#!/usr/bin/env python3
"""One real debug workload process, pinned to a region and a tenant resource.

It authenticates independently, resumes its persisted counter, and writes until
the regional adapter stops it or a custody/epoch check fences it. Credentials
are read from a private temporary configuration, never command arguments.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import threading
import time
import urllib.parse
import urllib.error
import urllib.request

from zc_auth_debug_server import http_json, loopback_open, write_private


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    stopped = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stopped.set())
    signal.signal(signal.SIGINT, lambda *_: stopped.set())
    oauth = config["oauth"]
    token, renew_at, credential_generation = None, 0, 0

    def access_token():
        nonlocal token, renew_at, credential_generation
        if time.monotonic() >= renew_at:
            body = urllib.parse.urlencode(dict(grant_type="client_credentials", client_id=oauth["client_id"],
                                               client_secret=oauth["client_secret"])).encode()
            request = urllib.request.Request(oauth["url"] + "/token", data=body,
                                             headers={"Content-Type": "application/x-www-form-urlencoded"})
            with loopback_open(request) as response:
                issued = json.load(response)
            token = issued["access_token"]
            credential_generation += 1
            renew_at = time.monotonic() + max(1, issued["expires_in"] / 2)
        return token

    def operation(action, **fields):
        return http_json(config["region_url"] + "/operation",
                         dict(fields, action=action, resource=config["resource"]), access_token())

    code, data = operation("read")
    if code != 200 or data["volume"]["authority_region"] != config["region"]:
        raise RuntimeError("workload has no authoritative starting volume")
    epoch = data["volume"]["placement_epoch"]
    previous = json.loads(data["data"]["records"].get("workload-runtime", "{}"))
    counter = previous.get("counter", 0)
    witness = {}
    while not stopped.is_set():
        counter += 1
        marker = dict(workload="workload-" + config["resource"], resource=config["resource"],
                      region=config["region"], pid=os.getpid(), run_id=config["run_id"], counter=counter, epoch=epoch,
                      credential_generation=credential_generation)
        code, result = operation("write", epoch=epoch, record_key="workload-runtime", value=json.dumps(marker, sort_keys=True))
        if code != 200:
            print(json.dumps(dict(event="workload_fenced_or_failed", status=code, pid=os.getpid(),
                                  resource=config["resource"], region=config["region"])), flush=True)
            return 2
        witness = dict(marker, sequence=result["sequence"])
        ready = Path(config["ready_file"])
        temporary = ready.with_suffix(".tmp")
        write_private(temporary, witness)
        os.replace(temporary, ready)
        stopped.wait(0.03)
    print(json.dumps(dict(event="workload_stopped", **witness)), flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except urllib.error.HTTPError as error:
        if error.code == 401:
            print(json.dumps(dict(event="workload_authentication_denied", status=401, pid=os.getpid())), flush=True)
            raise SystemExit(77)
        raise
