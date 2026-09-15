#!/usr/bin/env python3
"""Collect build evidence and enforce the three FIPS acceptance gates.

Python 3.9+, standard library only. `check` uses local Podman, never creates
cloud resources, and returns 1 for FAIL or BLOCKED. A report is not a CMVP
certificate. Policy files and review records must come from trusted review.
"""
import argparse
import datetime as dt
import hashlib
import importlib.util
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import tempfile
import urllib.request
import zipfile


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_PROFILE = ROOT / "zccusan/deploy/zcblock-csi/fips/acceptance-5314.json"
GATES = ("module", "services", "operation")
SERVICE_EXPECTATIONS = {
    "sha256_known_answer": True, "hmac_sha256_known_answer": True,
    "module_random": True, "aes256_gcm_internal_nonce_encrypt": True,
    "aes256_gcm_decrypt": True, "external_nonce_is_not_approved": False,
    "standalone_sha2_is_outside_module": False,
}
REVIEW_SECTIONS = ("build_procedure", "crypto_service_map", "operating_policy", "deployment_scope")
APPLICATION_SERVICES = {"native_token_rng", "native_byte_rng", "secret_lifecycle_rng", "native_key_derivation", "native_lane_key_derivation",
                        "native_frame_encrypt", "native_frame_decrypt", "global_rpc_encrypt", "global_rpc_decrypt", "zcnblk_key_derivation", "global_rpc_key_derivation"}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_digest(path):
    result = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def invoke(argv, timeout=60):
    return subprocess.run([str(arg) for arg in argv], text=True, capture_output=True, timeout=timeout)


def required_command(argv, timeout=60):
    result = invoke(argv, timeout)
    if result.returncode:
        raise ValueError(f"{argv[0]} exited {result.returncode}: {result.stderr[-1500:]}")
    return result.stdout


def read_optional(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def os_release(value):
    return dict(re.findall(r'^([A-Z_]+)=["\']?([^\n"\']*)["\']?$', value or "", re.M))


def environment():
    try:
        virt = invoke(["systemd-detect-virt", "--vm"])
        virtualization = virt.stdout.strip() if virt.returncode in (0, 1) else "unknown"
    except OSError:
        virtualization = "unknown"
    return {"os_release": os_release(read_optional("/etc/os-release")),
            "kernel": platform.release(), "architecture": platform.machine(),
            "node": platform.node(), "boot_id": read_optional("/proc/sys/kernel/random/boot_id"),
            "fips_enabled": read_optional("/proc/sys/crypto/fips_enabled"),
            "virtualization": virtualization,
            "system_vendor": read_optional("/sys/class/dmi/id/sys_vendor"),
            "product_name": read_optional("/sys/class/dmi/id/product_name"),
            "cpu_info": sorted(set(re.findall(r'^(?:model name|CPU implementer|CPU part)\s*:\s*(.+)$',
                                              read_optional("/proc/cpuinfo") or "", re.M)))}


def tree_files(root):
    root = Path(root)
    files = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"source symlink is not covered by the receipt: {path}")
        if path.is_file():
            files[path.relative_to(root).as_posix()] = file_digest(path)
    if not files:
        raise ValueError(f"empty source tree: {root}")
    return files


def source_files(root):
    files = {name: file_digest(root / name) for name in ("Cargo.toml", "Cargo.lock", "build.rs")}
    files.update({"src/" + name: value for name, value in tree_files(root / "src").items()})
    # Bind the build recipe and collector as well as the Rust inputs.
    for name in (".github/workflows/fips-aws-lc-5314.yml",
                 "zccusan/deploy/zcblock-csi/Dockerfile.fips", "scripts/fips-acceptance.py",
                 "scripts/github-ec2-runner-smoke.py",
                 "scripts/fips-recompile-aws-lc.py",
                 "scripts/fips-build-reproducibility.py", "scripts/fips-reproducible-provider.py",
                 "zccusan/deploy/zcblock-csi/fips/AWS-LC-RECOMPILATION.md",
                 "zccusan/deploy/zcblock-csi/fips/acceptance-review-history.json"):
        files[name] = file_digest(root / name)
    files.update({"vendor/aws-lc-fips-sys-provider/" + name: value for name, value in
                  tree_files(root / "vendor/aws-lc-fips-sys-provider").items()})
    return files


def cache_value(cache, name):
    match = re.search(r'^' + re.escape(name) + r':[^=]*=(.*)$', cache, re.M)
    return match.group(1).strip() if match else None


def provider_evidence(provider_dir):
    errors, result = [], {}
    if not provider_dir:
        return result, ["AWS_LC_FIPS_SYS_SYSTEM_DIR was not set"]
    root = Path(provider_dir).resolve()
    receipt_path = root / "share/zcutils/fips/provider-receipt.json"
    crypto, bcm = root / "lib/libcrypto.a", root / "lib/bcm.o"
    try:
        receipt_bytes = receipt_path.read_bytes()
        receipt = json.loads(receipt_bytes)
        result = {"root": str(root), "receipt": receipt,
                  "receipt_sha256": digest(receipt_bytes),
                  "libcrypto_sha256": file_digest(crypto), "bcm_sha256": file_digest(bcm),
                  "headers_manifest_sha256": digest(canonical(tree_files(root / "include")))}
        expected_commands = [
            {"argv": ["cmake3", "-DFIPS=1", ".."], "cwd": "aws-lc-AWS-LC-FIPS-3.1.0/build"},
            {"argv": ["make"], "cwd": "aws-lc-AWS-LC-FIPS-3.1.0/build"},
        ]
        provider = receipt.get("provider") or {}
        if receipt.get("schema") != 1 or receipt.get("certificate_number") != 5314:
            errors.append("provider receipt has the wrong schema or certificate")
        if receipt.get("module_version_string") != "AWS-LC FIPS 3.1.0":
            errors.append("provider receipt has the wrong module identity")
        if receipt.get("build_procedure_passed") is not True:
            errors.append("provider did not pass the section 11.1 build procedure")
        if receipt.get("certificate_profile_environment") is not True:
            errors.append("provider was not built on a certificate-5314 tested environment")
        if (receipt.get("source") or {}).get("archive_sha256") != \
                "fe408fa438850786396faf79eba9ea4116c3802e60f3a95865f0dd2adb64c9f1":
            errors.append("provider source archive hash is not the section 11.1 hash")
        if receipt.get("commands") != expected_commands:
            errors.append("provider receipt does not contain only the literal section 11.1 build commands")
        if provider.get("format") != "zc-aws-lc-fips-provider-v1" \
                or provider.get("linkage") != "static-unprefixed" \
                or provider.get("ffi_abi") != "aws-lc-fips-sys-0.13.11":
            errors.append("provider format, linkage, or FFI ABI is not the reviewed profile")
        if result["libcrypto_sha256"] != provider.get("libcrypto_sha256") \
                or result["libcrypto_sha256"] != (receipt.get("artifacts") or {}).get("libcrypto.a", {}).get("sha256"):
            errors.append("installed libcrypto.a does not match the prescribed build output")
        if result["bcm_sha256"] != provider.get("bcm_sha256") \
                or result["bcm_sha256"] != (receipt.get("artifacts") or {}).get("bcm.o", {}).get("sha256"):
            errors.append("installed bcm.o does not match the prescribed build output")
        if result["headers_manifest_sha256"] != provider.get("headers_manifest_sha256"):
            errors.append("installed provider headers differ from the provider receipt")
        if provider.get('archive_normalization'):
            if provider['archive_normalization'] != 'ar-deterministic-metadata-v1':
                errors.append('unrecognized provider archive normalization')
            else:
                original = receipt_path.with_name('libcrypto.original.a').read_bytes()
                record = json.loads(receipt_path.with_name('provider-build-record.json').read_text())
                spec = importlib.util.spec_from_file_location('archive_normalizer', Path(__file__).with_name('fips-recompile-aws-lc.py'))
                normalizer = importlib.util.module_from_spec(spec)
                spec.loader.exec_module(normalizer)
                if (digest(original) != record['artifacts']['libcrypto.a']['sha256'] or
                    digest(original) != record['provider']['original_libcrypto_sha256'] or
                    normalizer.normalize_archive_metadata(original) != crypto.read_bytes()):
                    errors.append('normalized provider differs from the prescribed archive beyond archive metadata')
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
        errors.append(f"provider evidence is unavailable: {error}")
    return result, errors


def cargo_recompilation_assessment(package, cache_files, system_link, provider_errors=None):
    """Classify the known sys-crate path against policy 5314 section 11.1.

    A positive classification requires the external provider receipt and the
    reviewed local 0.13.11-compatible adapter. Binary linkage is checked later.
    """
    reasons = []
    if system_link:
        reasons.extend(provider_errors or [])
        if cache_files:
            reasons.append("provider build unexpectedly produced a Cargo-owned AWS-LC CMake cache")
        if package.get("name") != "aws-lc-fips-sys" or package.get("version") != "0.13.11":
            reasons.append("resolved provider adapter is not the reviewed 0.13.11 FFI target")
        return {
            "security_policy_section": "5314:11.1",
            "status": "section-11.1-linked" if not reasons else "provider-evidence-invalid",
            "reasons": reasons,
            "review_override_allowed": False,
        }
    if not cache_files:
        reasons.append("native CMake cache is missing")
    for name, cache in sorted(cache_files.items()):
        home = cache_value(cache, "CMAKE_HOME_DIRECTORY")
        if not home or Path(home).name != "aws-lc":
            reasons.append(f"{name}: CMake source directory is the sys-crate wrapper, not the AWS-LC root")
        prefix = cache_value(cache, "BORINGSSL_PREFIX")
        if prefix:
            reasons.append(f"{name}: BORINGSSL_PREFIX={prefix} renames module symbols")
        build_type = cache_value(cache, "CMAKE_BUILD_TYPE")
        if build_type:
            reasons.append(f"{name}: CMAKE_BUILD_TYPE={build_type} is an added configuration option")
        for option, policy_default in (("BUILD_TESTING", "ON"), ("BUILD_TOOL", "ON"),
                                       ("BUILD_LIBSSL", "ON")):
            value = cache_value(cache, option)
            if value is not None and value.upper() != policy_default:
                reasons.append(f"{name}: {option}={value}, while the section 11.1 command leaves the {policy_default} default")
        fips = cache_value(cache, "FIPS")
        if not fips or fips.upper() not in ("1", "ON", "TRUE"):
            reasons.append(f"{name}: FIPS=1 is not recorded")
    if package.get("name") != "aws-lc-fips-sys" or package.get("version") != "0.13.11":
        reasons.append("resolved sys crate is not the reviewed 0.13.11 integration target")
    # Even an unusually configured 0.13.11 cache entered through its custom
    # Rust build script.  A future external-link collector must bind the exact
    # native artifact and may then emit a distinct positive status.
    if not reasons:
        reasons.append("aws-lc-fips-sys 0.13.11 Cargo invocation is not the literal section 11.1 procedure")
    return {
        "security_policy_section": "5314:11.1",
        "status": "not-section-11.1-linked",
        "reasons": reasons,
        "review_override_allowed": False,
    }


def collect_build(args):
    root, binaries, out = Path(args.source_root).resolve(), Path(args.binaries), Path(args.out)
    host = re.search(r'^host: (.+)$', required_command(["rustc", "-vV"]), re.M).group(1)
    metadata = json.loads(required_command(["cargo", "metadata", "--manifest-path", root / "Cargo.toml",
                                           "--offline", "--locked", "--features", "fips",
                                           "--filter-platform", host, "--format-version", "1"]))
    packages = [p for p in metadata["packages"] if p["name"] == "aws-lc-fips-sys"]
    if len(packages) != 1:
        raise ValueError("expected exactly one resolved aws-lc-fips-sys package")
    package = packages[0]
    expected_adapter = (root / "vendor/aws-lc-fips-sys-provider/Cargo.toml").resolve()
    module_root = Path(package["manifest_path"]).parent / "aws-lc"
    # A system-linked library needs separate provenance; the vendored source
    # receipt is deliberately unavailable in that configuration.
    system_link = os.environ.get("AWS_LC_FIPS_SYS_SYSTEM_DIR")
    module_files = None if system_link else tree_files(module_root)
    provider, provider_errors = provider_evidence(system_link)
    if package.get("source") is not None or Path(package["manifest_path"]).resolve() != expected_adapter:
        provider_errors.append("resolved aws-lc-fips-sys is not the reviewed local provider adapter")
    files = source_files(root)
    binary_records = {}
    for path in sorted(binaries.iterdir()):
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"unexpected binary entry: {path}")
        symbols = required_command(["nm", "--defined-only", path])
        identity_symbols = re.findall(r'^\s*[0-9a-fA-F]+ T (\S*awslc_version_string)$', symbols, re.M)
        prefixed_symbols = re.findall(r'^\s*[0-9a-fA-F]+\s+[A-Za-z]\s+(aws_lc(?:_fips)?_[0-9_]+_\S+)$', symbols, re.M)
        dynamic = required_command(["readelf", "-d", path])
        needed_crypto = re.findall(r'\(NEEDED\).*\[(?:lib)?(?:crypto|ssl)[^]]*\]', dynamic)
        binary_records[path.name] = {"sha256": file_digest(path),
                                     "static_identity_symbols": identity_symbols,
                                     "prefixed_aws_lc_symbols": prefixed_symbols,
                                     "dynamic_crypto_dependencies": needed_crypto}
    cache_files = {}
    for cache in (root / "target/release/build").glob("aws-lc-fips-sys-*/out/**/CMakeCache.txt"):
        cache_files[cache.relative_to(root).as_posix()] = cache.read_text()
    nodes = {node["id"]: node["features"] for node in metadata["resolve"]["nodes"]}
    receipt = {"schema": 1, "source_files": files, "source_sha256": digest(canonical(files)),
               "binaries": binary_records, "builder": environment(),
               "module_package": {"name": package["name"], "version": package["version"], "source": package["source"]},
               "module_source_tree_sha256": digest(canonical(module_files)) if module_files else None,
               "system_link": bool(system_link), "provider": provider, "cmake_caches": cache_files,
               "recompilation_assessment": cargo_recompilation_assessment(
                   {"name": package["name"], "version": package["version"]}, cache_files,
                   bool(system_link), provider_errors),
               "dependencies": [{"name": p["name"], "version": p["version"], "features": nodes.get(p["id"], [])}
                                for p in metadata["packages"]],
               "tools": {tool: required_command(command) for tool, command in {
                   "rustc": ["rustc", "-vV"], "cargo": ["cargo", "-V"], "cc": ["cc", "--version"],
                   "nm": ["nm", "--version"], "readelf": ["readelf", "--version"]}.items()},
               "validation_claim": "none; build provenance requires Security Policy review"}
    write_json(out / "build-receipt.json", receipt)
    return 0


class PageText(HTMLParser):
    def __init__(self):
        super().__init__()
        self.parts = []

    def handle_data(self, data):
        self.parts.extend(data.split())


def certificate_status(page, profile, today):
    parser = PageText()
    parser.feed(page)
    text = " ".join(parser.parts)
    return (f"Certificate #{profile['certificate_number']}" in text
            and profile["module_name"] in text
            and re.search(r'\bStatus Active\b', text) is not None
            and re.search(r'\bSunset Date ' + re.escape(profile["sunset_display"]) + r'\b', text) is not None
            and today <= dt.date.fromisoformat(profile["sunset_date"]))


def archive_tree(path):
    # Hash in place, never extract an untrusted path or execute archive content.
    files = {}
    with zipfile.ZipFile(path) as archive:
        roots = {item.filename.split("/")[0] for item in archive.infolist()}
        if len(roots) != 1:
            raise ValueError("validated archive must contain exactly one root")
        for item in archive.infolist():
            if item.is_dir():
                continue
            name = item.filename.split("/", 1)[1]
            if name in files or ".." in Path(name).parts or Path(name).is_absolute():
                raise ValueError("duplicate or unsafe source archive entry")
            files[name] = digest(archive.read(item))
    if not files:
        raise ValueError("empty validated archive")
    return digest(canonical(files))


def service_errors(probe):
    errors = []
    if probe.get("schema") != 2 or probe.get("passed") is not True:
        errors.append("missing or failed schema-2 executable evidence")
    for key in ("fips_feature", "aws_lc_fips_mode", "tls_provider_fips", "tls_client_config_fips"):
        if probe.get(key) is not True:
            errors.append(key + " is not true")
    module = probe.get("module") or {}
    if module.get("self_test") is not True or module.get("integrity_test") is not True:
        errors.append("module self-test/integrity test did not pass")
    for field in ("provider_receipt_sha256", "provider_libcrypto_sha256", "provider_bcm_sha256"):
        if not re.fullmatch(r'[0-9a-f]{64}', module.get(field, "")):
            errors.append(f"module {field} is missing or malformed")
    tests = module.get("services", [])
    names = [test.get("name") for test in tests]
    expected = set(SERVICE_EXPECTATIONS) | {"rejects_modified_ciphertext"}
    if len(names) != len(expected) or set(names) != expected:
        errors.append("missing, duplicate or unknown service controls")
    for test in tests:
        name = test.get("name")
        if test.get("passed") is not True:
            errors.append(f"{name}: control failed")
        if name in SERVICE_EXPECTATIONS:
            approved = SERVICE_EXPECTATIONS[name]
            before, after = test.get("indicator_before"), test.get("indicator_after")
            if (type(before) is not int or type(after) is not int or before < 0 or after < 0
                    or (before != after) is not approved or test.get("approved") is not approved
                    or test.get("functional") is not True or test.get("expected_approved") is not approved):
                errors.append(f"{name}: inconsistent approved-service indicator")
    return errors


def environment_errors(node, container_release, profile, require_host_fips=True):
    errors = []
    host_os, image_os = node.get("os_release", {}), os_release(container_release)
    for label, release in (("host", host_os), ("container", image_os)):
        if release.get("ID") != profile["os_id"] or release.get("VERSION_ID") != profile["os_version"]:
            errors.append(f"{label} OS is not a listed operational environment")
    if require_host_fips and profile["require_host_fips"] and node.get("fips_enabled") != "1":
        errors.append("kernel FIPS mode is not enabled (project deployment requirement)")
    if node.get("virtualization") != "none":
        errors.append("virtualized or unknown host does not match the selected bare-metal environment")
    if not any(node.get("architecture") == row["architecture"] and node.get("product_name") == row["product_name"]
               for row in profile["platforms"]):
        errors.append("CPU/platform identity does not match a listed test platform")
    if node.get("system_vendor") != "Amazon EC2":
        errors.append("hardware vendor is not established as Amazon EC2")
    if node.get("architecture") == "x86_64" and not any("8375C" in value for value in node.get("cpu_info", [])):
        errors.append("tested Intel Xeon Platinum 8375C CPU not established")
    if not node.get("kernel") or not node.get("boot_id"):
        errors.append("missing kernel/boot identity")
    return errors


def application_errors(probe):
    errors = []
    if (probe.get("schema") != 1 or probe.get("passed") is not True
            or probe.get("fips_feature") is not True or probe.get("aws_lc_fips_mode") is not True):
        errors.append("application crypto acceptance failed or is unavailable")
    checks = probe.get("checks", [])
    names = [check.get("name") for check in checks]
    if len(names) != len(APPLICATION_SERVICES) + 1 or set(names) != APPLICATION_SERVICES | {"native_tamper_rejected"}:
        errors.append("missing, duplicate or unknown application crypto checks")
    for check in checks:
        if check.get("passed") is not True:
            errors.append(f"{check.get('name')}: did not pass")
        if check.get("name") in APPLICATION_SERVICES:
            before, after = check.get("indicator_before"), check.get("indicator_after")
            if (type(before) is not int or type(after) is not int or before < 0 or after < 0 or before == after
                    or check.get("approved_service_observed") is not True or check.get("functional") is not True):
                errors.append(f"{check['name']}: approved service not established")
    return errors


def source_findings(root, dependencies):
    """Conservative inventory, not a proof that other crypto is absent."""
    findings = []
    rules = {"standalone-sha2": r'\b(?:sha2::|Sha256::|Sha512::)',
             "external-random": r'/dev/(?:u)?random|\bgetrandom\b',
             "external-gcm-nonce": r'Nonce::(?:try_)?assume_unique_for_key',
             "alternate-provider": r'\bring::|openssl::|sodiumoxide::|rust_crypto',
             "restricted-native-api": r'\b(?:PKCS7_verify|X509_verify_cert|X509_V_FLAG_CRL_CHECK|EVP_aead_aes_\d+_ccm)\b',
             "tls-config": r'(?:ClientConfig|ServerConfig)::builder|\.tls_config\('}
    for path in sorted((root / "src").rglob("*.rs")):
        relative = path.relative_to(root).as_posix()
        for number, line in enumerate(path.read_text().splitlines(), 1):
            for rule, pattern in rules.items():
                if re.search(pattern, line):
                    finding = {"rule": rule, "file": relative, "line": number, "text": line.strip()}
                    finding["id"] = digest(canonical(finding))
                    findings.append(finding)
    for dep in dependencies:
        if dep["name"] in ("ring", "sha2") or (dep["name"] == "rustls" and dep["version"].startswith("0.19.")):
            finding = {"rule": "dependency-crypto-review", "dependency": dep}
            finding["id"] = digest(canonical(finding))
            findings.append(finding)
    for dep in dependencies:
        if (dep["name"], dep["version"]) in (("aws-lc-fips-sys", "0.13.11"), ("aws-lc-sys", "0.36.0")):
            finding = {"rule": "pinned-module-advisory-review", "dependency": dep,
                       "advisories": ["GHSA-9f94-5g5w-gf6r", "GHSA-vw5v-4f2q-w9xf",
                                      "GHSA-hfpc-8r3f-gw53", "GHSA-65p9-r9h6-22vj"],
                       "requirement": "document affected API reachability and mitigation; a CMVP certificate is not a vulnerability waiver"}
            if dep["name"] == "aws-lc-sys":
                finding["advisories"].append("GHSA-394x-vwmw-crm3")
            finding["id"] = digest(canonical(finding))
            findings.append(finding)
    return findings


def known_service_blockers(root):
    # These are confirmed security-sensitive key derivations, not general
    # checksum uses of sha2. They cannot be waved through by a review record.
    blockers = []
    for relative, functions in {
        "src/global_secure_rpc.rs": ("frame_cipher",),
        "src/lib.rs": ("zc_aes256_cipher", "zc_aes256_lane_cipher", "zcnblk_aes256_lane_cipher", "zcnblk_payload_aes256_cipher"),
    }.items():
        source = (root / relative).read_text()
        for name in functions:
            for body in re.finditer(r'\bfn ' + name + r'\([^\n]*[\s\S]*?\n}', source):
                # Only an immediately attached, exact rustc exclusion counts.
                prefix = source[:body.start()].rstrip()
                prefix = re.sub(r'pub(?:\(crate\))?$', '', prefix).rstrip()
                if re.search(r'(?:^|\n)#\[cfg\(not\(feature = "fips"\)\)\]$', prefix):
                    continue
                if re.search(r'\bSha256::', body.group(0)):
                    blockers.append(f"{relative}:{name}: security key derivation uses standalone SHA-256")
    return blockers


class Podman:
    def __init__(self, storage=None):
        # A remote engine would test a different node from our host collector.
        self.command = ["podman", "--remote=false"]
        if storage:
            self.command += ["--root", str(Path(storage) / "root"), "--runroot", str(Path(storage) / "run")]

    def run(self, image, entrypoint, arguments):
        with tempfile.TemporaryDirectory(prefix="zc-fips-acceptance-") as directory:
            cidfile = Path(directory) / "cid"
            try:
                return invoke(self.command + ["run", "--cidfile", str(cidfile), "--rm", "--pull=never",
                              "--network=none", "--read-only", "--cap-drop=ALL", "--security-opt=no-new-privileges",
                              "--pids-limit=64", "--memory=256m", "--cpus=1",
                              "--entrypoint", entrypoint, image] + arguments)
            finally:
                if cidfile.exists():
                    cid = cidfile.read_text().strip()
                    if re.fullmatch(r'[0-9a-f]{64}', cid):
                        cleanup = invoke(self.command + ["rm", "--force", "--ignore", cid], timeout=15)
                        if cleanup.returncode:
                            raise ValueError("failed to remove acceptance container " + cid)

    def inspect(self, image):
        return json.loads(required_command(self.command + ["image", "inspect", "--", image]))[0]


def review_errors(review, expected, findings, directory, today):
    errors = []
    if review.get("schema") != 1 or not str(review.get("reviewer", "")).strip():
        errors.append("missing named reviewer/schema")
    for key, value in expected.items():
        if review.get(key) != value:
            errors.append(f"review does not bind the current {key}")
    try:
        reviewed = dt.date.fromisoformat(review["reviewed_at"])
        expires = dt.date.fromisoformat(review["expires_at"])
        if not reviewed <= today <= expires or (expires - reviewed).days > 90:
            errors.append("review is future-dated, expired or valid for more than 90 days")
    except (KeyError, TypeError, ValueError):
        errors.append("missing valid review dates")
    entries = review.get("findings", {})
    if set(entries) != {finding["id"] for finding in findings}:
        errors.append("review does not cover exactly the current source/dependency findings")
    for entry in entries.values():
        if (entry.get("disposition") not in ("approved-service", "non-security-use", "unreachable-in-release", "diagnostic-only")
                or not entry.get("rationale", "").strip()):
            errors.append("finding lacks a supported disposition and rationale")
    for section in REVIEW_SECTIONS:
        artifact = review.get("sections", {}).get(section, {})
        name = artifact.get("path", "")
        path = (directory / name).resolve()
        if (not name or Path(name).is_absolute() or not path.is_relative_to(directory.resolve())
                or not path.is_file() or not path.stat().st_size
                or file_digest(path) != artifact.get("sha256")):
            errors.append(section + ": missing, empty, outside-bundle or changed review artifact")
    return errors


class Acceptance:
    def __init__(self):
        self.checks = []

    def add(self, gate, name, errors=(), missing=False):
        self.checks.append({"gate": gate, "name": name,
                            "status": "BLOCKED" if missing else "FAIL" if errors else "PASS",
                            "details": list(errors)})

    def report(self):
        gates = {}
        for gate in GATES:
            statuses = [check["status"] for check in self.checks if check["gate"] == gate]
            gates[gate] = "FAIL" if "FAIL" in statuses else "BLOCKED" if not statuses or "BLOCKED" in statuses else "PASS"
        return {"schema": 1, "gates": gates, "checks": self.checks,
                "accepted": all(status == "PASS" for status in gates.values()),
                "validation_claim": "none; project acceptance evidence is not a CMVP certificate"}


def check(args):
    suite, evidence = Acceptance(), {}
    profile_path, root = Path(args.profile), Path(args.source_root).resolve()
    profile = json.loads(profile_path.read_text())
    today = dt.date.today()
    if profile.get("schema") != 1:
        raise ValueError("unsupported acceptance profile")
    evidence["profile"] = profile
    profile_hash = file_digest(profile_path)
    node = environment()
    evidence["node"] = node
    engine = Podman(args.storage)
    metadata = engine.inspect(args.image)
    image_id = metadata["Id"]
    if not re.fullmatch(r'(?:sha256:)?[0-9a-f]{64}', image_id):
        raise ValueError("engine did not return an immutable image ID")
    image_id = "sha256:" + image_id.removeprefix("sha256:")
    image_digest = metadata.get("Digest")
    evidence.update(image_id=image_id, image_digest=image_digest)
    suite.add("module", "immutable_image_digest", [] if re.fullmatch(r'sha256:[0-9a-f]{64}', image_digest or "") else ["missing OCI digest"])

    if args.offline:
        suite.add("module", "certificate_status", ["online CMVP status check disabled"], missing=True)
    else:
        try:
            url = f"https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/{int(profile['certificate_number'])}"
            request = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0 zcutils-fips-acceptance"})
            with urllib.request.urlopen(request, timeout=20) as response:
                page = response.read(2_000_000).decode()
            evidence["certificate_page_sha256"] = digest(page.encode())
            suite.add("module", "certificate_status", [] if certificate_status(page, profile, today) else ["certificate identity/status/sunset was not verified"])
        except (OSError, ValueError) as error:
            suite.add("module", "certificate_status", [str(error)], missing=True)

    raw_receipt = engine.run(image_id, "/bin/cat", ["/usr/share/zcutils/fips/build-receipt.json"])
    receipt = {}
    try:
        if raw_receipt.returncode:
            raise ValueError("image lacks build-receipt.json; rebuild with the acceptance collector")
        receipt = json.loads(raw_receipt.stdout)
        if receipt.get("schema") != 1:
            raise ValueError("unsupported build receipt")
        current_sources = source_files(root)
        suite.add("module", "source_binding", [] if receipt.get("source_files") == current_sources
                  and receipt.get("source_sha256") == digest(canonical(current_sources)) else ["checkout/build recipe differs from the image build inputs"])
        caches = receipt.get("cmake_caches", {})
        provider = receipt.get("provider") or {}
        provider_receipt = provider.get("receipt") or {}
        native_tools = provider_receipt.get("tools") or {}
        native_fips = (not caches and provider_receipt.get("build_procedure_passed") is True
                       and all(native_tools.get(tool) for tool in ("cmake3", "go", "make", "cc")))
        suite.add("module", "native_build_record", [] if native_fips
                  and all(receipt.get("tools", {}).get(tool) for tool in ("rustc", "cargo", "cc", "nm", "readelf"))
                  else ["missing prescribed provider or application toolchain evidence"])
        recompilation = receipt.get("recompilation_assessment") or {}
        recompilation_errors = [] if recompilation.get("status") == "section-11.1-linked" else (
            recompilation.get("reasons") or ["build receipt does not bind the application to a Security Policy section 11.1 module artifact"])
        suite.add("module", "security_policy_build_procedure", recompilation_errors)
        builder = receipt.get("builder", {})
        suite.add("module", "builder_environment", environment_errors(
            builder, "\n".join(f'{key}={value}' for key, value in builder.get("os_release", {}).items()),
            profile, require_host_fips=False))
    except (ValueError, KeyError, TypeError) as error:
        suite.add("module", "build_receipt", [str(error)], missing=True)
        receipt = {}
    evidence["build_receipt"] = receipt
    suite.add("module", "binary_inventory", [] if set(receipt.get("binaries", {})) == set(profile["binaries"]) else ["build receipt does not inventory exactly the release executables"])
    installed = engine.run(image_id, "/bin/sh", ["-c", "ls -A1 /usr/local/bin"])
    suite.add("module", "installed_binary_inventory", [] if installed.returncode == 0
              and set(installed.stdout.splitlines()) == set(profile["binaries"]) else ["installed executable inventory differs from the release profile"])
    if not args.validated_source:
        suite.add("module", "validated_source", ["supply the Security Policy source archive using --validated-source"], missing=True)
    else:
        archive = Path(args.validated_source)
        errors = []
        if file_digest(archive) != profile["source_archive_sha256"]:
            errors.append("source archive hash differs from Security Policy section 11.1")
        elif ((receipt.get("provider") or {}).get("receipt") or {}).get("source", {}).get("manifest_sha256") \
                != archive_tree(archive):
            errors.append("compiled module source tree is not the unchanged validated archive")
        suite.add("module", "validated_source", errors)

    evidence["executables"] = {}
    for binary in profile["binaries"]:
        if not re.fullmatch(r'[a-zA-Z0-9_-]+', binary):
            raise ValueError("unsafe binary name in trusted profile")
        try:
            result = engine.run(image_id, "/usr/local/bin/" + binary, ["--fips-evidence"])
            probe = json.loads(result.stdout)
            evidence["executables"][binary] = probe
            suite.add("services", binary + ":controls", service_errors(probe) + ([] if result.returncode == 0 else [f"executable exited {result.returncode}"]))
            identity = (probe.get("module") or {}).get("module_version_string")
            suite.add("module", binary + ":identity", [] if identity == profile["module_version_string"] else [f"linked identity {identity!r}; policy requires {profile['module_version_string']!r}"])
            module = probe.get("module") or {}
            suite.add("module", binary + ":provider", [] if
                      module.get("provider_receipt_sha256") == (receipt.get("provider") or {}).get("receipt_sha256")
                      and module.get("provider_libcrypto_sha256") == (receipt.get("provider") or {}).get("libcrypto_sha256")
                      and module.get("provider_bcm_sha256") == (receipt.get("provider") or {}).get("bcm_sha256")
                      else ["runtime provider identity does not match the build receipt"])
            suite.add("operation", binary + ":environment", environment_errors(node, probe.get("container_os_release"), profile)
                      + ([] if probe.get("host_fips_enabled") is True else ["executable did not observe host FIPS mode"])
                      + ([] if probe.get("architecture") == node.get("architecture") == receipt.get("builder", {}).get("architecture")
                         else ["binary, builder and node architectures do not match"]))
            actual_hash = engine.run(image_id, "/usr/bin/sha256sum", ["/usr/local/bin/" + binary])
            built = receipt.get("binaries", {}).get(binary, {})
            suite.add("module", binary + ":build_binding", [] if actual_hash.returncode == 0
                      and actual_hash.stdout.split()[0] == built.get("sha256")
                      and built.get("static_identity_symbols") == ["awslc_version_string"]
                      and not built.get("prefixed_aws_lc_symbols")
                      and not built.get("dynamic_crypto_dependencies")
                      else ["binary hash/static provider linkage does not match build receipt"])
        except (OSError, ValueError, TypeError, KeyError, IndexError, subprocess.TimeoutExpired) as error:
            for gate in GATES:
                suite.add(gate, binary + ":probe", [f"executable evidence unavailable: {error}"], missing=True)

    try:
        application = engine.run(image_id, "/usr/local/bin/zcutils", ["--fips-application-evidence"])
        application_probe = json.loads(application.stdout)
        evidence["application_crypto"] = application_probe
        suite.add("services", "application_crypto", application_errors(application_probe)
                  + ([] if application.returncode == 0 else [f"application probe exited {application.returncode}"]))
    except (OSError, ValueError, TypeError, KeyError, subprocess.TimeoutExpired) as error:
        suite.add("services", "application_crypto", [str(error)], missing=True)

    dependencies = receipt.get("dependencies", [])
    findings = source_findings(root, dependencies)
    evidence["findings"] = findings
    suite.add("services", "known_application_blockers", known_service_blockers(root))
    suite.add("services", "resolved_dependencies", [] if dependencies else ["no resolved dependency/feature inventory"], missing=not dependencies)
    expected_review = {"certificate_number": profile["certificate_number"], "profile_sha256": profile_hash,
                       "source_sha256": receipt.get("source_sha256"), "image_id": image_id, "image_digest": image_digest,
                       "build_receipt_sha256": digest(raw_receipt.stdout.encode()), "environment_sha256": digest(canonical(node))}
    review_template = {"schema": 1, **expected_review, "reviewer": "", "reviewed_at": "", "expires_at": "",
                       "sections": {section: {"path": "", "sha256": ""} for section in REVIEW_SECTIONS},
                       "findings": {finding["id"]: {"disposition": "", "rationale": ""} for finding in findings}}
    write_json(Path(args.report).with_suffix(".review-template.json"), review_template)
    if args.review:
        review_path = Path(args.review)
        try:
            errors = review_errors(json.loads(review_path.read_text()), expected_review, findings, review_path.parent, today)
        except (OSError, ValueError, TypeError, AttributeError) as error:
            errors = [str(error)]
        for gate in GATES:
            suite.add(gate, "independent_review", errors)
    else:
        for gate in GATES:
            suite.add(gate, "independent_review", ["exact build, crypto service map and operating/deployment policy review required"], missing=True)
    report = suite.report()
    report["evidence"] = evidence
    report["checked_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
    write_json(args.report, report)
    print(json.dumps({"accepted": report["accepted"], "gates": report["gates"], "report": str(args.report)}, indent=2))
    return int(not report["accepted"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    build = sub.add_parser("collect-build", help="run in the trusted builder after linking all release binaries")
    build.add_argument("--source-root", default=ROOT)
    build.add_argument("--binaries", required=True)
    build.add_argument("--out", required=True)
    build.set_defaults(func=collect_build)
    verify = sub.add_parser("check", help="run on the actual deployment node/guest against a local immutable image")
    verify.add_argument("--source-root", default=ROOT)
    verify.add_argument("--profile", default=DEFAULT_PROFILE)
    verify.add_argument("--image", required=True)
    verify.add_argument("--storage", help="isolated local Podman storage")
    verify.add_argument("--validated-source", help="unchanged ZIP identified by the selected Security Policy")
    verify.add_argument("--review", help="trusted review JSON bound to this image/source/node/profile")
    verify.add_argument("--offline", action="store_true", help="skip CMVP network lookup; blocks acceptance")
    verify.add_argument("--report", required=True)
    verify.set_defaults(func=check)
    args = parser.parse_args()
    try:
        return args.func(args)
    except (OSError, ValueError, KeyError, TypeError, AttributeError, IndexError, subprocess.TimeoutExpired, zipfile.BadZipFile) as error:
        report = {"schema": 1, "accepted": False, "gates": dict.fromkeys(GATES, "BLOCKED"),
                  "error": str(error), "validation_claim": "none; acceptance collection failed"}
        if args.command == "check":
            write_json(args.report, report)
        print(json.dumps(report, indent=2))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
