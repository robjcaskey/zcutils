#!/usr/bin/env python3
"""Collect exact-source service review inputs; check review completeness and key budgets.

Structural checks and declared bounds are not reachability proofs or a validation.
The caller must authenticate the image, reviewer, and supporting records separately.
"""
import argparse
import datetime as dt
import hashlib
import json
from pathlib import Path
import re


RULES = {
    "tls-construction": r"(?:ClientConfig|ServerConfig|Client|ClientBuilder|HttpConnector|HttpsConnector)::(?:builder\w*|new|try_default)|\.tls_config\(|\.use_preconfigured_tls\(|\.danger_accept_invalid_(?:certs|hostnames)\(",
    "alternate-provider": r"\bring::|openssl::|sodiumoxide::|rust_crypto|\bsha2::|\bSha(?:256|512)::",
    "external-random": r"/dev/(?:u)?random|\bgetrandom\b",
    "restricted-native-api": r"\b(?:PKCS7_\w+|X509_verify_cert|X509_V_FLAG_CRL_CHECK|EVP_aead_aes_\d+_ccm)\b",
}
CRYPTO_PACKAGES = {"ring", "rustls", "openssl", "openssl-sys", "native-tls", "sha2", "aws-lc-rs", "aws-lc-sys", "aws-lc-fips-sys", "rustls-webpki"}
SECTIONS = {"tls_service_coverage", "dependency_reachability", "entropy_and_credentials", "self_test_failure_handling", "operating_conditions"}
DISPOSITIONS = {"approved-service", "non-security-use", "unreachable-in-release", "diagnostic-only"}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def inventory(root, metadata):
    root = Path(root)
    sources, findings = {}, []
    for path in sorted((root / "src").rglob("*.rs")):
        if path.is_symlink():
            raise ValueError("source symlink is not permitted")
        name = path.relative_to(root).as_posix()
        sources[name] = digest(path.read_bytes())
        for line, text in enumerate(path.read_text().splitlines(), 1):
            for rule, pattern in RULES.items():
                if re.search(pattern, text):
                    findings.append({"rule": rule, "file": name, "line": line, "text": text.strip()})
    if not sources:
        raise ValueError("empty source inventory")
    for name in ("Cargo.toml", "Cargo.lock", "build.rs"):
        sources[name] = digest((root / name).read_bytes())
    resolve = metadata.get("resolve") or {}
    nodes = {n["id"]: n for n in resolve.get("nodes", [])}
    packages = {p["id"]: p for p in metadata["packages"]}
    root_id = resolve.get("root")
    if root_id not in nodes or "fips" not in nodes[root_id].get("features", []):
        raise ValueError("resolved root does not enable fips")
    # Record all target-filtered paths through the resolved graph, including
    # transitive alternate providers. This does not claim executable reachability.
    paths = {root_id: [root_id]}
    pending = [root_id]
    for current in pending:
        for dependency in nodes[current].get("dependencies", []):
            if dependency not in nodes or dependency not in packages:
                raise ValueError("incomplete resolved dependency graph")
            if dependency not in paths:
                paths[dependency] = paths[current] + [dependency]
                pending.append(dependency)
    for package_id in sorted(paths):
        package = packages[package_id]
        if package["name"] in CRYPTO_PACKAGES:
            features = sorted(nodes[package_id].get("features", []))
            if package["name"] == "aws-lc-rs" and "fips" not in features:
                raise ValueError("reachable aws-lc-rs does not enable fips")
            if package["name"] == "rustls" and package["version"].startswith("0.23.") and "fips" not in features:
                raise ValueError("reachable Rustls 0.23 does not enable fips")
            findings.append({"rule": "dependency-service-review", "name": package["name"],
                             "version": package["version"], "source": package.get("source"),
                             "features": features, "dependency_path": paths[package_id]})
    if not any(f.get("name") == "aws-lc-fips-sys" for f in findings):
        raise ValueError("resolved graph lacks the FIPS native provider")
    for finding in findings:
        finding["id"] = digest(canonical(finding))
    report = {"schema": 1, "source_files": sources, "metadata_sha256": digest(canonical(metadata)),
              "findings": findings, "scope": "syntactic source inventory and resolved dependency paths; semantic reachability remains reviewed"}
    report["inventory_sha256"] = digest(canonical(report))
    return report


def supporting_record(entry, directory):
    if not isinstance(entry, dict):
        return False
    name = entry.get("path", "")
    if not isinstance(name, str) or not name or Path(name).is_absolute():
        return False
    path = (directory / name).resolve()
    return (path.is_relative_to(directory.resolve()) and path.is_file()
            and path.stat().st_size > 0 and digest(path.read_bytes()) == entry.get("sha256"))


def validate_review(report, review, directory, expected_image_digest, today=None):
    errors = []
    directory = Path(directory)
    today = today or dt.date.today()
    content = {k: v for k, v in report.items() if k != "inventory_sha256"}
    if report.get("schema") != 1 or report.get("inventory_sha256") != digest(canonical(content)):
        errors.append("inventory integrity check failed")
    if review.get("schema") != 1 or not isinstance(review.get("reviewer"), str) or not review["reviewer"].strip():
        errors.append("named reviewer and schema are mandatory")
    if review.get("inventory_sha256") != report.get("inventory_sha256"):
        errors.append("review does not bind this source and dependency inventory")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", str(review.get("image_digest", ""))):
        errors.append("exact image digest is missing")
    if review.get("image_digest") != expected_image_digest:
        errors.append("review does not identify the image being accepted")
    try:
        start, end = (dt.date.fromisoformat(review[k]) for k in ("reviewed_at", "expires_at"))
        if not start <= today <= end:
            raise ValueError()
    except (KeyError, TypeError, ValueError):
        errors.append("review date is invalid, expired or future-dated")
    entries = review.get("findings", {})
    if not isinstance(entries, dict) or set(entries) != {f["id"] for f in report["findings"]}:
        errors.append("review must cover exactly the current findings")
        entries = {} if not isinstance(entries, dict) else entries
    for entry in entries.values():
        if (not isinstance(entry, dict) or entry.get("disposition") not in DISPOSITIONS
                or not str(entry.get("rationale", "")).strip()
                or not supporting_record(entry.get("record"), directory)):
            errors.append("finding lacks disposition, rationale or unchanged supporting record")
    sections = review.get("sections", {})
    if not isinstance(sections, dict) or set(sections) != SECTIONS:
        errors.append("operational review sections are incomplete or unknown")
    else:
        for name, entry in sections.items():
            if not supporting_record(entry, directory):
                errors.append(name + ": supporting record missing, altered or outside review bundle")
    budgets = review.get("gcm_key_budgets", [])
    if not isinstance(budgets, list) or not budgets:
        errors.append("aggregate GCM key budget inventory is missing")
        budgets = []
    seen = set()
    for budget in budgets:
        if not isinstance(budget, dict):
            errors.append("invalid GCM key budget")
            continue
        key = budget.get("key_scope")
        if not isinstance(key, str) or not key.strip() or key in seen:
            errors.append("missing or duplicate GCM key scope")
        else:
            seen.add(key)
        # Explicit hard bounds, never observed averages. Attempt rate includes
        # empty frames, retries and failures; instances include restarts/reuse.
        values = [budget.get(k) for k in ("max_encrypting_instances", "max_attempts_per_second_per_instance", "max_key_lifetime_seconds", "prior_attempts")]
        if any(type(value) is not int or value < (0 if index == 3 else 1) for index, value in enumerate(values)):
            errors.append("GCM budget bounds must be integers, positive except prior attempts")
        elif values[0] * values[1] * values[2] + values[3] > 2**32:
            errors.append("aggregate GCM invocation budget exceeds 2^32")
        if not supporting_record(budget.get("enforcement_record"), directory):
            errors.append("GCM bound lacks unchanged enforcement justification")
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    collect = sub.add_parser("collect")
    collect.add_argument("--source-root", type=Path, required=True)
    collect.add_argument("--cargo-metadata", type=Path, required=True)
    collect.add_argument("--out", type=Path, required=True)
    check = sub.add_parser("check")
    check.add_argument("--inventory", type=Path, required=True)
    check.add_argument("--review", type=Path, required=True)
    check.add_argument("--image-digest", required=True)
    args = parser.parse_args()
    try:
        if args.command == "collect":
            args.out.unlink(missing_ok=True)
            report = inventory(args.source_root, json.loads(args.cargo_metadata.read_text()))
            args.out.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        else:
            errors = validate_review(json.loads(args.inventory.read_text()), json.loads(args.review.read_text()), args.review.parent, args.image_digest)
            print(json.dumps({"passed": not errors, "errors": errors, "scope": "record completeness and declared arithmetic bounds; reviewer identity and semantic conclusions must be independently verified"}, indent=2))
            return int(bool(errors))
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(json.dumps({"passed": False, "errors": [str(error)]}))
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
