#!/usr/bin/env python3
"""Loopback E2E: two OAuth issuers, an independent JIT approval authority,
and east/west gateways backed by actual Rust policy/failover/limiter code.
No cloud resources, Kubernetes cluster, block devices or pattern-based cleanup.
"""
import argparse
import hashlib
import json
from pathlib import Path
import secrets
import signal
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request
from zc_auth_debug_server import Rejected, http_json, loopback_open, write_private

ROOT = Path(__file__).resolve().parent.parent


class Scenario:
    def __init__(self, work, driver):
        self.work, self.driver = Path(work), driver
        self.processes, self.logs, self.checks, self.credentials = [], [], [], {}

    def check(self, name, condition=True):
        if not condition:
            raise AssertionError(name)
        self.checks.append(name)
        print("PASS " + name, flush=True)

    def start(self, name, mode, config):
        conf, ready = self.work / (name + ".json"), self.work / (name + "-ready.json")
        write_private(conf, config)
        log = open(self.work / (name + ".log"), "w")
        process = subprocess.Popen([sys.executable, str(ROOT / "scripts/zc_auth_debug_server.py"),
                                    "--mode", mode, "--config", str(conf), "--ready-file", str(ready)], stdout=log, stderr=log)
        self.processes.append(process)
        self.logs.append(log)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(name + " exited; inspect debug logs")
            if ready.exists():
                return json.loads(ready.read_text()), process
            time.sleep(0.02)
        raise RuntimeError(name + " failed to become ready")

    def oauth(self, name, clients):
        for client in clients.values():
            client.setdefault("secret", secrets.token_urlsafe(24))
            client.setdefault("audience", "urn:zcutils:auth-lab")
            client.setdefault("ttl", 120)
        ready, _ = self.start(name, "oauth", {"clients": clients})
        self.credentials[name] = (ready["url"], clients)
        return ready

    def token(self, issuer, client):
        url, clients = self.credentials[issuer]
        data = urllib.parse.urlencode({"grant_type": "client_credentials", "client_id": client,
                                       "client_secret": clients[client]["secret"]}).encode()
        req = urllib.request.Request(url + "/token", data=data, headers={"Content-Type": "application/x-www-form-urlencoded"})
        with loopback_open(req) as response:
            return json.load(response)["access_token"]

    def op(self, token, region, action, resource, expected=200, approval=None, **fields):
        code, result = http_json(self.regions[region] + "/operation", dict(fields, action=action, resource=resource), token, approval)
        if code != expected:
            raise AssertionError(f"{region}/{action}/{resource}: expected {expected}, got {code}: {result.get('error')}")
        return result

    def read(self, token, region, resource):
        return self.op(token, region, "read", resource)

    def fingerprint(self):
        return {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in (self.work / "media").glob("*.json")}

    def request(self, token, resource="alpha-retail", duration=20, **overrides):
        body = dict(resource=resource, region="us-east", action="delete_record", role="record-maintainer",
                    ticket="CHG123", reason="approved debug repair", duration_seconds=duration)
        body.update(overrides)
        return http_json(self.workflow + "/requests", body, token)

    def decision(self, request_id, action="approve", token=None):
        return http_json(self.workflow + "/requests/" + request_id + "/" + action, {}, token or self.operator_secret)

    def approval(self, token, duration=20):
        code, request = self.request(token, duration=duration)
        assert code == 200, "approval request failed"
        assert self.decision(request["id"])[0] == 200, "approval decision failed"
        code, grant = http_json(self.workflow + "/requests/" + request["id"], token=token)
        assert code == 200 and grant["state"] == "approved", "grant unavailable"
        return request["id"], grant["approval_token"], grant["expires_at"]

    def configure(self, interactive=False):
        a = self.oauth("issuer-a", {
            "alpha-admin": {"subject": "alpha-admin"}, "retail-only": {"subject": "retail-only"},
            "claims-admin": {"subject": "unbound", "extra_claims": {"roles": ["admin", "record-maintainer"], "tenant": "beta"}},
            "wrong-audience": {"subject": "alpha-admin", "audience": "urn:other-federation"},
            "future": {"subject": "alpha-admin", "ttl": 300, "not_before_delay": 120},
            "expired": {"subject": "alpha-admin", "ttl": -1}, "short": {"subject": "alpha-admin", "ttl": 2}})
        b = self.oauth("issuer-b", {"alpha-operator": {"subject": "alpha-operator"},
                                    "west-admin": {"subject": "west-admin"}, "same-subject": {"subject": "alpha-admin"}})
        keys = [a["verification_key"], b["verification_key"]]
        principal = {"issuer": a["url"], "subject": "alpha-admin"}
        resources = {name: dict(id=name, tenant=tenant, budget_id="budget-" + name) for name, tenant in {
            "alpha-root": ["acme"], "alpha-retail": ["acme", "retail"], "alpha-wholesale": ["acme", "wholesale"],
            "quota-probe": ["acme"], "lookalike": ["acme-other"], "beta-west": ["beta"]}.items()}
        self.operator_secret, self.introspection_secret = secrets.token_urlsafe(32), secrets.token_urlsafe(32)
        workflow, self.workflow_process = self.start("approval", "approval", {
            "identity_keys": keys, "identity_audience": "urn:zcutils:auth-lab", "approval_audience": "urn:zcutils:auth-lab:approval",
            "federation": "auth-lab", "operator_id": "independent-security-reviewer", "operator_secret": self.operator_secret,
            "introspection_secret": self.introspection_secret, "max_duration_seconds": 30,
            "request_timeout_seconds": 300 if interactive else 2,
            "eligibility": [dict(principal=principal, resource=r, tenant=resources[r]["tenant"], region="us-east",
                                 action="delete_record", role="record-maintainer") for r in ("alpha-retail", "alpha-wholesale")]})
        self.workflow = workflow["url"]
        ordinary = ["read", "write", "replicate", "manage_tenant", "failover", "quota"]
        grants = []
        for issuer, subject, tenant, descendants in [(a["url"], "alpha-admin", ["acme"], True),
            (b["url"], "alpha-operator", ["acme"], True), (a["url"], "retail-only", ["acme", "retail"], False),
            (b["url"], "west-admin", ["beta"], True)]:
            for region in ("us-east", "us-west"):
                actions = ordinary + (["delete_record"] if region == "us-east" or subject == "west-admin" else [])
                grants.append(dict(principal=dict(issuer=issuer, subject=subject), region=region, tenant=tenant,
                                   descendants=descendants, actions=actions))
        policy = dict(federation="auth-lab", audience="urn:zcutils:auth-lab", identity_keys=keys,
                      approval_key=workflow["verification_key"], approval_audience="urn:zcutils:auth-lab:approval",
                      max_approval_seconds=30, resources=resources, grants=grants,
                      approval_roles={"us-east": {"delete_record": "record-maintainer"}})
        driver_config = self.work / "driver.json"
        write_private(driver_config, dict(authorization=policy, data_dir=str(self.work / "media"), west_owned=["beta-west"],
                                         atomic_groups={"alpha-app": ["alpha-root", "alpha-retail", "alpha-wholesale"]}))
        ready, _ = self.start("regions", "regions", dict(driver=str(self.driver), driver_config=str(driver_config),
            driver_log=str(self.work / "driver.log"), approval_url=self.workflow, introspection_secret=self.introspection_secret))
        self.regions = ready["regions"]
        self.a, self.b = self.token("issuer-a", "alpha-admin"), self.token("issuer-b", "alpha-operator")
        self.retail, self.beta = self.token("issuer-a", "retail-only"), self.token("issuer-b", "west-admin")
        for name, (url, _) in self.credentials.items():
            metadata = http_json(url + "/.well-known/oauth-authorization-server")[1]
            jwks = http_json(metadata["jwks_uri"])[1]
            self.check(name + " discovery and asymmetric JWKS", metadata["issuer"] == url and len(jwks["keys"]) == 1)
        self.check("two independently keyed identity issuers", keys[0]["public_key"] != keys[1]["public_key"])

    def authentication_and_isolation(self):
        try:
            http_json("http://unregistered.invalid/token", {}, self.a)
        except Rejected as error:
            self.check("debug clients reject non-loopback destinations before sending credentials", error.reason == "debug_http_requires_loopback")
        else:
            raise AssertionError("external debug HTTP destination accepted")
        self.read(self.a, "us-east", "alpha-root")
        self.read(self.b, "us-east", "alpha-root")
        self.check("east accepts both registered issuers")
        before = self.fingerprint()
        for client, issuer in [("same-subject", "issuer-b"), ("claims-admin", "issuer-a"), ("wrong-audience", "issuer-a"),
                               ("future", "issuer-a"), ("expired", "issuer-a")]:
            self.op(self.token(issuer, client), "us-east", "read", "alpha-root", 403)
            self.check(client + " cannot acquire local authority")
        head, body, signature = self.a.split(".")
        forged = head + "." + body + "." + ("A" if signature[0] != "A" else "B") + signature[1:]
        self.op(forged, "us-east", "read", "alpha-root", 403)
        self.op(self.a, "us-east", "read", "lookalike", 403)
        self.op(self.a, "us-west", "delete_record", "beta-west", 403, record_key="seed", epoch=1)
        self.op(self.beta, "us-east", "manage_tenant", "alpha-retail", 403)
        self.op(self.retail, "us-east", "write", "alpha-wholesale", 403, record_key="bad", value="intrusion", epoch=1)
        self.check("forgery, prefix collision, foreign deletion and sibling writes leave data unchanged", before == self.fingerprint())
        self.check("foreign west tenant retains exact records", self.read(self.beta, "us-west", "beta-west")["data"]["records"] == {"seed": "private-beta-west"})
        for resource in ("alpha-retail", "alpha-wholesale"):
            self.op(self.a, "us-west", "manage_tenant", resource, value="Recovery site " + resource)
        metadata = json.loads((self.work / "media/us-west-alpha-retail-tenant.json").read_text())
        self.check("west delegates actual subtenant metadata updates", metadata["label"] == "Recovery site alpha-retail"
                   and metadata["budget_id"] == "budget-alpha-retail")
        before = self.fingerprint()
        code, _ = http_json(self.regions["us-east"] + "/operation", dict(action="manage_tenant", resource="alpha-root", grants=["admin"]), self.a)
        self.check("delegated management cannot rewrite grants", code == 400 and before == self.fingerprint())
        quota = self.op(self.a, "us-east", "quota", "quota-probe", ops=4)
        self.op(self.b, "us-east", "quota", "quota-probe", 429)
        self.op(self.token("issuer-a", "alpha-admin"), "us-east", "quota", "quota-probe", 429)
        self.check("both issuers and token refresh share one existing lane bucket", quota["admitted"] == 4)

    def failover_roundtrips(self):
        members = ["alpha-root", "alpha-retail", "alpha-wholesale"]
        source, target, epoch = "us-east", "us-west", 1
        for cycle in range(4):
            # Real HTTP workload, paused by the harness for each clean cut.
            for resource in members:
                self.op(self.a, source, "write", resource, epoch=epoch, record_key="workload-sequence", value=str(cycle + 1))
            before = self.fingerprint()
            placement_before = [self.read(self.a, source, r)["volume"] for r in members]
            self.op(self.retail, source, "failover", "alpha-retail", 403, epoch=epoch, target_region=target)
            self.check(f"cycle {cycle}: atomic closure rejects sibling scope escalation", before == self.fingerprint()
                       and placement_before == [self.read(self.a, source, r)["volume"] for r in members])
            if cycle == 0:
                self.op(self.a, source, "failover", "alpha-root", 403, epoch=epoch, target_region=target)
                self.check("promotion denied before replication catches up", self.read(self.a, source, "alpha-root")["volume"]["placement_epoch"] == epoch)
            packets = []
            for resource in members:
                exported = self.op(self.a, source, "replicate", resource, epoch=epoch, target_region=target)
                packets.append(exported["replica"])
                self.check(f"cycle {cycle}: {resource} encrypted HTTP replication restores exact data",
                    self.read(self.a, source, resource)["data"] == self.read(self.b, target, resource)["data"]
                    and "private-" not in json.dumps(exported["replica"]))
            before = self.fingerprint()
            self.op(self.a, target, "import", "alpha-root", 403, replica=dict(packets[0], resource="beta-west"))
            cipher = packets[0]["ciphertext"]
            corrupted = dict(packets[0], ciphertext=("A" if cipher[0] != "A" else "B") + cipher[1:])
            self.op(self.a, target, "import", "alpha-root", 403, replica=corrupted)
            self.check(f"cycle {cycle}: replica substitution denied without mutation", before == self.fingerprint())
            result = self.op(self.b, source, "failover", "alpha-root", epoch=epoch, target_region=target)
            self.check(f"cycle {cycle}: actual failover state machine completes custody transfer", result["operation"]["phase"] == "completed"
                and len(result["volumes"]) == 3 and all(v["authority_region"] == target and v["placement_epoch"] == epoch + 1 for v in result["volumes"]))
            self.check(f"cycle {cycle}: workload actions stop source and start destination", len(result["workload_actions"]) == 3
                and all(a["source_replicas"] == 0 and a["target_replicas"] == 1 and a["acknowledged"] for a in result["workload_actions"]))
            before = self.fingerprint()
            self.op(self.a, source, "write", "alpha-root", 403, epoch=epoch, record_key="bad", value="stale")
            self.op(self.a, target, "write", "alpha-root", 403, epoch=epoch, record_key="bad", value="stale")
            self.op(self.a, target, "import", "alpha-root", 403, replica=packets[0])
            self.check(f"cycle {cycle}: old source, epoch and replica fenced", before == self.fingerprint())
            session = self.read(self.a, target, "alpha-root")["session"]
            self.check(f"cycle {cycle}: capable session rebound", session["state"] == "active" and session["observed_placement_epoch"] == epoch + 1)
            self.op(self.a, target, "write", "alpha-root", epoch=epoch + 1, record_key="after-cut", value="workload-running-" + target)
            self.op(self.a, target, "delete_record", "beta-west", 403, epoch=1, record_key="seed")
            source, target, epoch = target, source, epoch + 1
        self.check("two full east-west-east workload roundtrips preserve foreign tenant", source == "us-east" and epoch == 5
                   and self.read(self.beta, "us-west", "beta-west")["data"]["sequence"] == 1)

    def breakglass(self):
        before = self.fingerprint()
        for grant in (None, self.a):
            self.op(self.a, "us-east", "delete_record", "alpha-retail", 403, approval=grant, epoch=5, record_key="seed")
        self.check("east administrators require independent approval", before == self.fingerprint())
        self.check("approval TTL capped", self.request(self.a, duration=31)[0] == 403)
        self.check("unapproved principal ineligible", self.request(self.b)[0] == 403)
        self.check("different role ineligible", self.request(self.a, role="federation-admin")[0] == 403)
        code, pending = self.request(self.a)
        self.check("request starts pending without privilege", code == 200 and "approval_token" not in pending)
        self.check("requester cannot self-approve", self.decision(pending["id"], token=self.a)[0] == 403)
        self.check("independent reviewer can deny", self.decision(pending["id"], "deny")[1]["state"] == "denied")
        self.check("denied request cannot be approved", self.decision(pending["id"])[0] == 403)
        request_id, grant, _ = self.approval(self.a)
        self.check("decision replay cannot extend grant", self.decision(request_id)[0] == 403)
        before = self.fingerprint()
        for token, region, resource, epoch in [(self.b, "us-east", "alpha-retail", 5), (self.a, "us-east", "alpha-wholesale", 5),
            (self.a, "us-west", "alpha-retail", 5), (self.a, "us-west", "beta-west", 1)]:
            self.op(token, region, "delete_record", resource, 403, approval=grant, epoch=epoch, record_key="seed")
        self.check("approval cannot cross subject, subtenant, region or tenant", before == self.fingerprint())
        self.op(self.a, "us-east", "delete_record", "alpha-retail", approval=grant, epoch=5, record_key="seed")
        self.check("approved exact action succeeds", "seed" not in self.read(self.a, "us-east", "alpha-retail")["data"]["records"])
        self.check("active grant revoked", self.decision(request_id, "revoke")[1]["state"] == "revoked")
        before = self.fingerprint()
        self.op(self.a, "us-east", "delete_record", "alpha-retail", 403, approval=grant, epoch=5, record_key="workload-sequence")
        self.check("revocation invalidates issued grant", before == self.fingerprint())
        _, short, expires = self.approval(self.a, duration=2)
        self.op(self.a, "us-east", "delete_record", "alpha-retail", approval=short, epoch=5, record_key="nonexistent")
        time.sleep(max(0, expires - time.time()) + 0.05)
        before = self.fingerprint()
        self.op(self.token("issuer-a", "alpha-admin"), "us-east", "delete_record", "alpha-retail", 403, approval=short, epoch=5, record_key="workload-sequence")
        self.check("hard approval expiry survives OAuth refresh", before == self.fingerprint())
        short_identity = self.token("issuer-a", "short")
        _, live_grant, _ = self.approval(short_identity)
        _, pending_expiry = self.request(self.a)
        time.sleep(2.05)
        before = self.fingerprint()
        self.op(short_identity, "us-east", "delete_record", "alpha-retail", 403, approval=live_grant, epoch=5, record_key="workload-sequence")
        self.check("valid approval cannot extend expired identity", before == self.fingerprint())
        self.check("pending request timeout never auto-approves", self.decision(pending_expiry["id"])[0] == 403
                   and http_json(self.workflow + "/requests/" + pending_expiry["id"], token=self.a)[1]["state"] == "expired")
        code, audit = http_json(self.workflow + "/audit", token=self.operator_secret)
        self.audit = audit["events"]
        self.check("correlated secret-free approval lifecycle audit", code == 200
            and {e["event"] for e in self.audit} >= {"requested", "approved", "denied", "revoked"}
            and "approval_token" not in json.dumps(self.audit))
        self.workflow_process.terminate()
        self.workflow_process.wait(timeout=10)
        before = self.fingerprint()
        self.op(self.a, "us-east", "delete_record", "alpha-retail", 403, approval=live_grant, epoch=5, record_key="workload-sequence")
        self.read(self.a, "us-east", "alpha-retail")
        self.check("approval outage denies elevation but allows ordinary access", before == self.fingerprint())

    def close(self):
        for process in reversed(self.processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        for log in self.logs:
            log.close()


def main():
    def interrupted(*_):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver", type=Path, default=ROOT / "target/debug/zc-federation-auth-debug")
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--serve", action="store_true", help="keep fresh fixtures running for manual debugging; do not auto-approve or run the scenario")
    parser.add_argument("--output", type=Path, default=ROOT / "target/federation-auth-e2e")
    args = parser.parse_args()
    if not args.skip_build:
        subprocess.run(["cargo", "build", "--locked", "--bin", "zc-federation-auth-debug"], cwd=ROOT, check=True)
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="zc-auth-e2e-") as work:
        scenario, passed = Scenario(work, args.driver.resolve()), False
        try:
            scenario.configure(interactive=args.serve)
            if args.serve:
                print(json.dumps({"debug_only": True, "issuers": {k: v[0] for k, v in scenario.credentials.items()},
                                  "approval": scenario.workflow, "regions": scenario.regions,
                                  "private_config_directory": str(scenario.work)}, indent=2), flush=True)
                try:
                    while True:
                        time.sleep(1)
                except KeyboardInterrupt:
                    return
            scenario.authentication_and_isolation()
            scenario.failover_roundtrips()
            scenario.breakglass()
            passed = True
        finally:
            scenario.close()
            summary = dict(passed=passed, checks=scenario.checks,
                topology=dict(oauth_servers=2, approval_servers=1, regional_http_listeners=2,
                    consensus="in-process existing state machines", ha_observations="modeled",
                    storage="encrypted ordinary files", representative_benchmark=False),
                limitations=["loopback debug HTTP", "no live Raft or Kubernetes cluster", "no real ServiceNow, MFA or emergency bypass"])
            if not args.serve:
                (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
                (args.output / "approval-audit.json").write_text(json.dumps(getattr(scenario, "audit", []), indent=2) + "\n")
                for path in Path(work).glob("*.log"):
                    (args.output / path.name).write_bytes(path.read_bytes())
    print(f"ZC_FEDERATION_AUTH_E2E_PASS checks={len(scenario.checks)} evidence={args.output}")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
