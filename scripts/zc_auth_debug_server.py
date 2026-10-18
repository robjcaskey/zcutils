#!/usr/bin/env python3
"""Loopback-only OAuth, approval-workflow, and regional debug HTTP adapters.

Not a production identity provider, PAM system, or regional storage service.
The regional adapter calls the actual Rust authorization/failover/limiter code.
Requires the Python `cryptography` package for standard ES256 primitives.
"""

import argparse
import base64
import hmac
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import decode_dss_signature, encode_dss_signature


def b64(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def unb64(value):
    return base64.b64decode(value + "=" * (-len(value) % 4), altchars=b"-_", validate=True)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate_json_key")
        result[key] = value
    return result


def parse_json(data):
    return json.loads(data, object_pairs_hook=unique_object)


class Rejected(Exception):
    def __init__(self, reason, status=403):
        self.reason, self.status = reason, status


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_):
        raise Rejected("debug_http_redirect_forbidden")


def loopback_open(request, timeout=5):
    url = request.full_url if isinstance(request, urllib.request.Request) else request
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != "http" or parsed.hostname != "127.0.0.1" or not parsed.port or parsed.username or parsed.password:
        raise Rejected("debug_http_requires_loopback")
    # Never send debug client/reviewer credentials through ambient proxies or
    # follow a redirect supplied by an endpoint.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    return opener.open(request, timeout=timeout)


class Signer:
    def __init__(self, issuer):
        self.issuer = issuer
        self.key = ec.generate_private_key(ec.SECP256R1())
        self.kid = secrets.token_hex(12)

    def verification_key(self):
        point = self.key.public_key().public_numbers()
        return {"issuer": self.issuer, "kid": self.kid,
                "public_key": b64(b"\x04" + point.x.to_bytes(32, "big") + point.y.to_bytes(32, "big"))}

    def jwks(self):
        point = self.key.public_key().public_numbers()
        return {"keys": [{"kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig",
                          "kid": self.kid, "x": b64(point.x.to_bytes(32, "big")),
                          "y": b64(point.y.to_bytes(32, "big"))}]}

    def sign(self, claims, typ):
        header = {"alg": "ES256", "kid": self.kid, "typ": typ}
        data = ".".join(b64(json.dumps(v, separators=(",", ":")).encode()) for v in (header, claims))
        r, s = decode_dss_signature(self.key.sign(data.encode(), ec.ECDSA(hashes.SHA256())))
        return data + "." + b64(r.to_bytes(32, "big") + s.to_bytes(32, "big"))


def identity(token, keys, audience):
    try:
        if len(token) > 16384:
            raise ValueError()
        head, body, sig = token.split(".")
        claims, header = parse_json(unb64(body)), parse_json(unb64(head))
        key = next(k for k in keys if k["issuer"] == claims["iss"])
        if header != {"alg": "ES256", "kid": key["kid"], "typ": "at+jwt"}:
            raise ValueError()
        public = ec.EllipticCurvePublicKey.from_encoded_point(ec.SECP256R1(), unb64(key["public_key"]))
        signature = unb64(sig)
        if len(signature) != 64:
            raise ValueError()
        public.verify(encode_dss_signature(int.from_bytes(signature[:32], "big"), int.from_bytes(signature[32:], "big")),
                      (head + "." + body).encode(), ec.ECDSA(hashes.SHA256()))
        now = int(time.time())
        if claims["aud"] != audience or not claims["sub"] or not claims["jti"]:
            raise ValueError()
        if not (claims["iat"] <= claims["nbf"] <= now < claims["exp"] <= claims["iat"] + 3600):
            raise ValueError()
        return {"issuer": claims["iss"], "subject": claims["sub"]}
    except Exception as error:
        raise Rejected("invalid_identity") from error


def http_json(url, body=None, token=None, approval=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    if approval:
        headers["X-Zc-Approval"] = approval
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, headers=headers)
    try:
        with loopback_open(req) as response:
            return response.status, parse_json(response.read(65537))
    except urllib.error.HTTPError as error:
        return error.code, parse_json(error.read(65537))


class Workflow:
    def __init__(self, config, signer):
        self.config, self.signer = config, signer
        self.requests, self.audit = {}, []
        self.lock = threading.Lock()

    def event(self, event, request):
        self.audit.append({"sequence": len(self.audit) + 1, "time": int(time.time()),
                           "event": event, "request_id": request["id"], "ticket": request["ticket"],
                           "principal": request["principal"], "resource": request["resource"],
                           "region": request["region"], "action": request["action"], "role": request["role"],
                           "expires_at": request.get("expires"), "grant_jti": request.get("grant_jti"),
                           "approver": self.config["operator_id"] if event in ("approved", "denied", "revoked") else None})

    def handle(self, method, path, body, bearer):
        with self.lock:
            return self._handle(method, path, body, bearer)

    def _handle(self, method, path, body, bearer):
        config, now = self.config, int(time.time())
        if method == "POST" and path == "/requests":
            principal = identity(bearer, config["identity_keys"], config["identity_audience"])
            expected = {"resource", "region", "action", "role", "ticket", "reason", "duration_seconds"}
            if set(body) != expected or not isinstance(body["duration_seconds"], int) or isinstance(body["duration_seconds"], bool):
                raise Rejected("invalid_request", 400)
            if not (1 <= body["duration_seconds"] <= config["max_duration_seconds"]):
                raise Rejected("duration_exceeds_policy")
            if not all(isinstance(body[k], str) and 0 < len(body[k]) <= 256 for k in expected - {"duration_seconds"}):
                raise Rejected("ticket_and_reason_required", 400)
            eligible = next((g for g in config["eligibility"] if g["principal"] == principal
                             and all(g[k] == body[k] for k in ("resource", "region", "action", "role"))), None)
            if not eligible:
                raise Rejected("not_eligible")
            if len(self.requests) >= 256:
                raise Rejected("request_limit", 429)
            request = dict(body, id=secrets.token_hex(16), principal=principal,
                           tenant=eligible["tenant"], state="pending",
                           request_expires=now + config.get("request_timeout_seconds", 30))
            self.requests[request["id"]] = request
            self.event("requested", request)
            return {"id": request["id"], "state": "pending"}
        if method == "GET" and path == "/audit":
            self.require_operator(bearer)
            return {"events": self.audit}
        if method == "POST" and path == "/status":
            if not hmac.compare_digest(bearer, config["introspection_secret"]):
                raise Rejected("introspection_credential_required")
            if set(body) != {"grant_jti"}:
                raise Rejected("invalid_status_request", 400)
            request = next((r for r in self.requests.values() if r.get("grant_jti") == body["grant_jti"]), None)
            if request and request["state"] == "approved" and request["expires"] <= now:
                request["state"] = "expired"
                self.event("expired", request)
            if not request or request["expires"] <= now:
                raise Rejected("grant_expired_or_unknown")
            claims = {"iss": self.signer.issuer, "aud": config["approval_audience"], "iat": now,
                      "nbf": now, "exp": min(now + 2, request["expires"]),
                      "grant_jti": request["grant_jti"], "active": request["state"] == "approved"}
            return {"status_token": self.signer.sign(claims, "zc-status+jwt")}
        parts = path.strip("/").split("/")
        if len(parts) not in (2, 3) or parts[0] != "requests" or parts[1] not in self.requests:
            raise Rejected("not_found", 404)
        request = self.requests[parts[1]]
        if method == "GET" and len(parts) == 2:
            principal = identity(bearer, config["identity_keys"], config["identity_audience"])
            if principal != request["principal"]:
                raise Rejected("request_owner_required")
            state = request["state"]
            if state == "pending" and request["request_expires"] <= now:
                state = "expired"
            if state == "approved" and request["expires"] <= now:
                state = "expired"
            if state != request["state"]:
                request["state"] = state
                self.event("expired", request)
            response = {"id": request["id"], "state": state}
            if state == "approved":
                response.update(approval_token=request["token"], expires_at=request["expires"])
            return response
        self.require_operator(bearer)
        if method != "POST" or len(parts) != 3 or body:
            raise Rejected("invalid_decision", 400)
        decision = parts[2]
        if decision in ("approve", "deny"):
            if request["state"] != "pending" or request["request_expires"] <= now:
                raise Rejected("request_not_pending")
            if decision == "deny":
                request["state"] = "denied"
            else:
                request["state"] = "approved"
                request["expires"] = now + request["duration_seconds"]
                request["grant_jti"] = secrets.token_hex(16)
                claims = {"iss": self.signer.issuer, "aud": config["approval_audience"], "iat": now,
                          "nbf": now, "exp": request["expires"], "jti": request["grant_jti"],
                          "principal": request["principal"], "federation": config["federation"],
                          "region": request["region"], "resource": request["resource"], "tenant": request["tenant"],
                          "action": request["action"], "role": request["role"], "ticket": request["ticket"],
                          "approver": config["operator_id"]}
                request["token"] = self.signer.sign(claims, "zc-approval+jwt")
        elif decision == "revoke" and request["state"] == "approved":
            request["state"] = "revoked"
        else:
            raise Rejected("invalid_state_transition")
        self.event(request["state"], request)
        return {"id": request["id"], "state": request["state"]}

    def require_operator(self, bearer):
        if not hmac.compare_digest(bearer, self.config["operator_secret"]):
            raise Rejected("independent_approver_required")


class Driver:
    def __init__(self, config):
        self.config = config
        self.stderr = open(config["driver_log"], "w")
        self.process = subprocess.Popen([config["driver"], "--debug-config", config["driver_config"]],
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr, text=True)
        self.lock = threading.Lock()

    def call(self, body):
        with self.lock:
            if self.process.poll() is not None:
                raise Rejected("driver_unavailable", 503)
            self.process.stdin.write(json.dumps(body) + "\n")
            self.process.stdin.flush()
            line = self.process.stdout.readline(65537)
            if not line:
                raise Rejected("driver_unavailable", 503)
            response = parse_json(line)
            if not response["ok"]:
                raise Rejected(response["error"], response["status"])
            return response["result"]

    def handle(self, region, body, token, approval):
        allowed = {"action", "resource", "target_region", "record_key", "value", "epoch", "ops", "replica"}
        if set(body) - allowed or not {"action", "resource"} <= set(body):
            raise Rejected("invalid_operation", 400)
        status_token = None
        if approval:
            try:
                claims = parse_json(unb64(approval.split(".")[1]))
                code, result = http_json(self.config["approval_url"] + "/status", {"grant_jti": claims["jti"]},
                                         self.config["introspection_secret"])
                if code == 200:
                    status_token = result["status_token"]
            except Exception:
                pass  # Rust fails closed when no independently signed status exists.
        request = dict(body, region=region, token=token, approval=approval, status=status_token)
        if body["action"] != "replicate":
            return self.call(request)
        request["action"] = "export"
        exported = self.call(request)
        # Routes are pinned to the two local listeners, never to caller URLs.
        target = body["target_region"]
        code, imported = http_json(self.config["region_urls"][target] + "/operation",
                                  {"action": "import", "resource": body["resource"], "replica": exported["replica"]}, token)
        if code != 200:
            raise Rejected(imported.get("error", "replication_failed"), code)
        return dict(imported, replica=exported["replica"])

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(timeout=5)
        self.stderr.close()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, *_):
        pass  # Never log Authorization headers, client secrets, or token bodies.

    def do_GET(self):
        self.dispatch("GET")

    def do_POST(self):
        self.dispatch("POST")

    def dispatch(self, method):
        try:
            if self.headers.get("Transfer-Encoding") or len(self.headers.get_all("Content-Length", [])) > 1:
                raise Rejected("invalid_framing", 400)
            if len(self.headers.get_all("Authorization", [])) > 1 or len(self.headers.get_all("X-Zc-Approval", [])) > 1:
                raise Rejected("duplicate_credentials", 400)
            size = int(self.headers.get("Content-Length", "0"))
            if not 0 <= size <= 65536:
                raise Rejected("request_too_large", 413)
            self.connection.settimeout(5)
            raw = self.rfile.read(size)
            if len(raw) != size:
                raise Rejected("incomplete_body", 400)
            authorization = self.headers.get("Authorization", "")
            bearer = authorization[7:] if authorization.startswith("Bearer ") else ""
            server = self.server
            if method == "GET" and self.path == "/health":
                result = {"debug_only": True, "mode": server.mode}
            elif server.mode == "oauth":
                result = self.oauth(method, raw)
            elif server.mode == "approval":
                body = parse_json(raw) if raw else {}
                if not isinstance(body, dict):
                    raise Rejected("object_required", 400)
                result = server.workflow.handle(method, self.path, body, bearer)
            elif server.mode == "regions" and method == "POST" and self.path == "/operation":
                body = parse_json(raw)
                if not isinstance(body, dict):
                    raise Rejected("object_required", 400)
                result = server.driver.handle(server.region, body, bearer, self.headers.get("X-Zc-Approval"))
            else:
                raise Rejected("not_found", 404)
            self.respond(200, result)
        except Rejected as error:
            self.respond(error.status, {"error": error.reason})
        except (ValueError, KeyError, TypeError):
            self.respond(400, {"error": "malformed_request"})
        except Exception:
            self.respond(503, {"error": "debug_service_unavailable"})

    def respond(self, status, result):
        data = json.dumps(result, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def oauth(self, method, raw):
        server = self.server
        if method == "GET" and self.path == "/.well-known/oauth-authorization-server":
            return {"issuer": server.signer.issuer, "token_endpoint": server.signer.issuer + "/token",
                    "jwks_uri": server.signer.issuer + "/jwks", "grant_types_supported": ["client_credentials"],
                    "token_endpoint_auth_methods_supported": ["client_secret_post"]}
        if method == "GET" and self.path == "/jwks":
            return server.signer.jwks()
        if method != "POST" or self.path != "/token":
            raise Rejected("not_found", 404)
        parsed = urllib.parse.parse_qs(raw.decode(), keep_blank_values=True, strict_parsing=True)
        if any(len(v) != 1 for v in parsed.values()) or set(parsed) - {"grant_type", "client_id", "client_secret", "scope"}:
            raise Rejected("invalid_request", 400)
        values = {k: v[0] for k, v in parsed.items()}
        client = server.config["clients"].get(values.get("client_id"))
        if not client or not hmac.compare_digest(values.get("client_secret", ""), client["secret"]):
            raise Rejected("invalid_client", 401)
        if values.get("grant_type") != "client_credentials":
            raise Rejected("unsupported_grant_type", 400)
        scopes = set(values.get("scope", client.get("scope", "zcutils")).split())
        if not scopes <= set(client.get("scope", "zcutils").split()):
            raise Rejected("invalid_scope", 400)
        now = int(time.time())
        claims = dict(client.get("extra_claims", {}), iss=server.signer.issuer, sub=client["subject"],
                      aud=client["audience"], iat=now, nbf=now + client.get("not_before_delay", 0),
                      exp=now + client.get("ttl", 120), jti=secrets.token_hex(16), scope=" ".join(sorted(scopes)))
        return {"access_token": server.signer.sign(claims, "at+jwt"), "token_type": "Bearer",
                "expires_in": client.get("ttl", 120), "scope": claims["scope"]}


def write_private(path, data):
    path = Path(path)
    with open(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), "w") as stream:
        json.dump(data, stream)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("oauth", "approval", "regions"), required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--ready-file", type=Path, required=True)
    args = parser.parse_args()
    config = parse_json(args.config.read_bytes())
    servers, driver = [], None
    for name in (["us-east", "us-west"] if args.mode == "regions" else [args.mode]):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        server.mode, server.config, server.region = args.mode, config, name
        server.base_url = "http://127.0.0.1:" + str(server.server_port)
        servers.append(server)
    if args.mode == "regions":
        config["region_urls"] = {s.region: s.base_url for s in servers}
        driver = Driver(config)
        for server in servers:
            server.driver = driver
        ready = {"regions": config["region_urls"]}
    else:
        server = servers[0]
        server.signer = Signer(server.base_url)
        if args.mode == "approval":
            server.workflow = Workflow(config, server.signer)
        ready = {"url": server.base_url, "verification_key": server.signer.verification_key()}
    stopped = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stopped.set())
    signal.signal(signal.SIGINT, lambda *_: stopped.set())
    threads = []
    for server in servers:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        threads.append(thread)
    write_private(args.ready_file, ready)
    print("ZC_AUTH_DEBUG_READY loopback_only=true production=false", flush=True)
    try:
        stopped.wait()
    finally:
        for server in servers:
            server.shutdown()
            server.server_close()
        if driver:
            driver.close()
        for thread in threads:
            thread.join(timeout=5)


if __name__ == "__main__":
    main()
