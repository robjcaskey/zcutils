#!/usr/bin/env python3
"""Reproduce AWS-LC certificate 5314 Security Policy section 11.1.

This runner deliberately preserves the two build commands printed in the
Security Policy.  It creates evidence; it does not issue or extend a CMVP
validation.  By default it refuses operational environments outside the two
environments listed on certificate 5314.  ``--allow-untested-environment`` is
for build-procedure rehearsal and is recorded as such in the report.
"""
import argparse
import datetime as dt
import hashlib
import json
from pathlib import Path
import platform
import re
import shutil
import socket
import subprocess
import sys
import zipfile


MODULE = "AWS-LC 3 Cryptographic Module (static)"
MODULE_VERSION = "AWS-LC FIPS 3.1.0"
ARCHIVE_SHA256 = "fe408fa438850786396faf79eba9ea4116c3802e60f3a95865f0dd2adb64c9f1"
POLICY_URL = "https://csrc.nist.gov/CSRC/media/projects/cryptographic-module-validation-program/documents/security-policies/140sp5314.pdf"
ALLOWED_PLATFORMS = {
    ("x86_64", "c6i.metal"),
    ("aarch64", "r8g.metal-24xl"),
}


def file_digest(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def canonical_digest(value):
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def read_optional(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def parse_os_release(value):
    return dict(re.findall(r'^([A-Z_]+)=["\']?([^\n"\']*)["\']?$', value or "", re.M))


def environment():
    return {
        "os_release": parse_os_release(read_optional("/etc/os-release")),
        "architecture": platform.machine(),
        "kernel": platform.release(),
        "system_vendor": read_optional("/sys/class/dmi/id/sys_vendor"),
        "product_name": read_optional("/sys/class/dmi/id/product_name"),
        "boot_id": read_optional("/proc/sys/kernel/random/boot_id"),
        "fips_enabled": read_optional("/proc/sys/crypto/fips_enabled"),
    }


def environment_findings(value):
    findings = []
    release = value.get("os_release") or {}
    if release.get("ID") != "amzn" or release.get("VERSION_ID") != "2023":
        findings.append("certificate 5314 lists Amazon Linux 2023")
    platform_pair = (value.get("architecture"), value.get("product_name"))
    if platform_pair not in ALLOWED_PLATFORMS:
        findings.append("architecture and hardware are not a listed certificate 5314 pair")
    return findings


def archive_manifest(archive_path):
    files = {}
    with zipfile.ZipFile(archive_path) as archive:
        roots = {item.filename.split("/", 1)[0] for item in archive.infolist()}
        if len(roots) != 1 or "" in roots:
            raise ValueError("archive must contain exactly one top-level directory")
        root = next(iter(roots))
        for item in archive.infolist():
            if item.is_dir():
                continue
            parts = Path(item.filename).parts
            if not parts or parts[0] != root or ".." in parts or Path(item.filename).is_absolute():
                raise ValueError(f"unsafe archive member: {item.filename!r}")
            mode = item.external_attr >> 16
            if mode & 0o170000 == 0o120000:
                raise ValueError(f"source archive symlink is not accepted: {item.filename!r}")
            relative = Path(*parts[1:]).as_posix()
            if not relative or relative in files:
                raise ValueError(f"duplicate or invalid archive member: {item.filename!r}")
            files[relative] = hashlib.sha256(archive.read(item)).hexdigest()
    if not files:
        raise ValueError("source archive is empty")
    return root, files


def extract_archive(archive_path, destination):
    root, expected = archive_manifest(archive_path)
    destination = Path(destination)
    if destination.exists():
        raise ValueError("work directory already exists; use a new path for each build")
    destination.mkdir(parents=True)
    source_root = destination / root
    with zipfile.ZipFile(archive_path) as archive:
        for item in archive.infolist():
            if item.is_dir():
                continue
            relative = Path(*Path(item.filename).parts[1:])
            output = source_root / relative
            output.parent.mkdir(parents=True, exist_ok=True)
            with output.open("xb") as stream:
                stream.write(archive.read(item))
            if (item.external_attr >> 16) & 0o111:
                output.chmod(0o755)
    actual = {}
    for path in sorted(source_root.rglob("*")):
        if path.is_file():
            actual[path.relative_to(source_root).as_posix()] = file_digest(path)
    if actual != expected:
        raise ValueError("extracted source does not match the archive manifest")
    return source_root, expected


def run(argv, cwd, timeout=3600):
    result = subprocess.run(argv, cwd=cwd, text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        detail = (result.stdout + "\n" + result.stderr)[-4000:]
        raise ValueError(f"{argv[0]} exited {result.returncode}: {detail}")
    return result.stdout.strip()


def command_version(argv):
    result = subprocess.run(argv, text=True, capture_output=True, timeout=30)
    if result.returncode:
        raise ValueError(f"cannot execute required tool {argv[0]!r}")
    return (result.stdout or result.stderr).strip()


def unique_file(root, name):
    values = [path for path in Path(root).rglob(name) if path.is_file()]
    if len(values) != 1:
        raise ValueError(f"expected one {name}, found {len(values)}")
    return values[0]


def build_identity_probe(source_root, build_root, crypto):
    probe_source = build_root / "zc-aws-lc-identity.c"
    probe = build_root / "zc-aws-lc-identity"
    probe_source.write_text(
        "#include <stdio.h>\n#include <openssl/service_indicator.h>\n"
        "int main(void) { puts(awslc_version_string()); return 0; }\n"
    )
    run(["cc", str(probe_source), "-I", str(source_root / "include"), str(crypto),
         "-pthread", "-ldl", "-o", str(probe)], build_root)
    version = run([str(probe)], build_root)
    symbols = run(["nm", "--defined-only", str(probe)], build_root)
    static_symbols = re.findall(r'^\s*[0-9a-fA-F]+\s+T\s+(awslc_version_string)$', symbols, re.M)
    if version != MODULE_VERSION:
        raise ValueError(f"module identity is {version!r}, expected {MODULE_VERSION!r}")
    if static_symbols != ["awslc_version_string"]:
        raise ValueError("identity probe does not contain one static awslc_version_string symbol")
    return probe


def normalize_archive_metadata(data):
    """Normalize GNU ar bookkeeping; preserve object/index bytes and offsets."""
    if not data.startswith(b"!<arch>\n"):
        raise ValueError("expected a regular ar archive")
    result = bytearray(data)
    offset = 8
    count = 0
    while offset < len(data):
        header = data[offset:offset + 60]
        if len(header) != 60 or header[58:] != b"`\n":
            raise ValueError("invalid ar member header")
        size_text = header[48:58].strip()
        if not size_text.isdigit():
            raise ValueError("invalid ar member size")
        size = int(size_text)
        end = offset + 60 + size + size % 2
        if end > len(data):
            raise ValueError("truncated ar member")
        # GNU's long-name table has a blank timestamp; preserve that header.
        if header[:16].strip() != b"//":
            result[offset + 16:offset + 28] = b"0           "
            result[offset + 28:offset + 34] = b"0     "
            result[offset + 34:offset + 40] = b"0     "
            result[offset + 40:offset + 48] = (b"0       " if header[:16].strip() in (b'/', b'/SYM64/')
                                               else b"644     ")
        offset = end
        count += 1
    if not count:
        raise ValueError("empty ar archive")
    return bytes(result)


def provider_identity(report):
    """Stable compilation identity; per-run observations stay in the build record."""
    identity = {key: report[key] for key in (
        "schema", "certificate_number", "module_name", "module_version_string",
        "security_policy", "security_policy_section", "source", "commands", "tools",
        "network_interfaces", "environment_findings", "build_procedure_passed",
        "certificate_profile_environment", "claim", "provider",
    ) if key in report}
    identity["environment"] = {key: value for key, value in report.get("environment", {}).items()
                               if key != "boot_id"}
    identity["artifacts"] = {name: {"sha256": report["artifacts"][name]["sha256"]}
                             for name in ("libcrypto.a", "bcm.o")}
    if "provider" in report:
        identity["provider"] = {key: value for key, value in report["provider"].items()
                                if key != "original_libcrypto_sha256"}
        identity["artifacts"]["libcrypto.a"]["sha256"] = report["provider"]["libcrypto_sha256"]
    return identity


def create_provider(provider_dir, source_root, crypto, bcm, report):
    """Package the already-built module without running another build step."""
    provider = Path(provider_dir).resolve()
    if provider.exists():
        raise ValueError("provider directory already exists")
    provider.mkdir(parents=True)
    shutil.copytree(Path(source_root) / "include", provider / "include")
    lib = provider / "lib"
    lib.mkdir()
    original = Path(crypto).read_bytes()
    (lib / "libcrypto.a").write_bytes(normalize_archive_metadata(original))
    shutil.copy2(bcm, lib / "bcm.o")
    receipt_path = provider / "share/zcutils/fips/provider-receipt.json"
    receipt_path.parent.mkdir(parents=True)
    report["provider"] = {
        "format": "zc-aws-lc-fips-provider-v1",
        "libcrypto": "lib/libcrypto.a",
        "libcrypto_sha256": file_digest(lib / "libcrypto.a"),
        "original_libcrypto_sha256": file_digest(crypto),
        "archive_normalization": "ar-deterministic-metadata-v1",
        "bcm": "lib/bcm.o",
        "bcm_sha256": file_digest(lib / "bcm.o"),
        "headers_manifest_sha256": canonical_digest(tree_manifest(provider / "include")),
        "linkage": "static-unprefixed",
        "ffi_abi": "aws-lc-fips-sys-0.13.11",
    }
    if report["provider"]["original_libcrypto_sha256"] != report["artifacts"]["libcrypto.a"]["sha256"]:
        raise ValueError("packaged libcrypto.a differs from the prescribed build output")
    if report["provider"]["bcm_sha256"] != report["artifacts"]["bcm.o"]["sha256"]:
        raise ValueError("packaged bcm.o differs from the prescribed build output")
    # The application embeds the receipt hash. Keep timestamps, boot identity,
    # and intermediate/tool output hashes in the assembled record, not its code.
    receipt_path.with_name("provider-build-record.json").write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n")
    receipt_path.with_name("libcrypto.original.a").write_bytes(original)
    receipt_path.write_text(json.dumps(provider_identity(report), indent=2, sort_keys=True) + "\n")
    return provider, receipt_path


def tree_manifest(root):
    root = Path(root)
    files = {path.relative_to(root).as_posix(): file_digest(path)
             for path in sorted(root.rglob("*")) if path.is_file()}
    if not files:
        raise ValueError(f"empty packaged tree: {root}")
    return files


def reproduce(args):
    interfaces = sorted(name for _, name in socket.if_nameindex())
    if getattr(args, "require_offline", False) and any(name != "lo" for name in interfaces):
        raise ValueError("offline module build has a non-loopback network interface")
    archive = Path(args.archive).resolve()
    work = Path(args.work_dir).resolve()
    report_path = Path(args.report).resolve()
    if report_path == work or work in report_path.parents:
        raise ValueError("report must be outside the new work directory")
    provider_path = Path(args.provider_dir).resolve() if args.provider_dir else None
    if provider_path and (provider_path == work or work in provider_path.parents
                          or provider_path == report_path):
        raise ValueError("provider directory must be separate from the work directory and report")
    if file_digest(archive) != ARCHIVE_SHA256:
        raise ValueError("archive SHA-256 does not match Security Policy section 11.1")
    observed_environment = environment()
    env_findings = environment_findings(observed_environment)
    if env_findings and not args.allow_untested_environment:
        raise ValueError("; ".join(env_findings))
    for tool in ("cmake3", "make", "go", "cc", "nm"):
        if shutil.which(tool) is None:
            raise ValueError(f"required tool is unavailable: {tool}")

    source_root, manifest = extract_archive(archive, work)
    build_root = source_root / "build"
    build_root.mkdir()
    commands = [
        {"argv": ["cmake3", "-DFIPS=1", ".."], "cwd": str(build_root.relative_to(work))},
        {"argv": ["make"], "cwd": str(build_root.relative_to(work))},
    ]
    run(commands[0]["argv"], build_root)
    run(commands[1]["argv"], build_root)

    bssl = build_root / "tool" / "bssl"
    if not bssl.is_file():
        raise ValueError("section 11.1 output tool/bssl was not produced")
    if run([str(bssl), "isfips"], build_root) != "1":
        raise ValueError("tool/bssl isfips did not return 1")
    crypto = unique_file(build_root, "libcrypto.a")
    bcm = unique_file(build_root, "bcm.o")
    probe = build_identity_probe(source_root, build_root, crypto)
    cache = build_root / "CMakeCache.txt"
    if not cache.is_file() or not re.search(r'^FIPS:(?:BOOL|STRING|UNINITIALIZED)=(?:1|ON|TRUE)$', cache.read_text(), re.M):
        raise ValueError("CMake cache does not record FIPS=1")

    eligible = not env_findings
    report = {
        "schema": 1,
        "certificate_number": 5314,
        "module_name": MODULE,
        "module_version_string": MODULE_VERSION,
        "security_policy": POLICY_URL,
        "security_policy_section": "11.1",
        "network_interfaces": interfaces,
        "completed_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "source": {
            "archive": archive.name,
            "archive_sha256": file_digest(archive),
            "manifest_sha256": canonical_digest(manifest),
            "file_count": len(manifest),
        },
        "environment": observed_environment,
        "environment_findings": env_findings,
        "commands": commands,
        "tools": {
            "cmake3": command_version(["cmake3", "--version"]),
            "make": command_version(["make", "--version"]),
            "go": command_version(["go", "version"]),
            "cc": command_version(["cc", "--version"]),
        },
        "artifacts": {
            "bcm.o": {"path": str(bcm.relative_to(work)), "sha256": file_digest(bcm)},
            "libcrypto.a": {"path": str(crypto.relative_to(work)), "sha256": file_digest(crypto)},
            "bssl": {"path": str(bssl.relative_to(work)), "sha256": file_digest(bssl), "isfips": 1},
            "identity_probe": {"path": str(probe.relative_to(work)), "sha256": file_digest(probe),
                               "module_version_string": MODULE_VERSION,
                               "static_identity_symbol": "awslc_version_string"},
            "cmake_cache": {"path": str(cache.relative_to(work)), "sha256": file_digest(cache)},
        },
        "build_procedure_passed": True,
        "certificate_profile_environment": eligible,
        "claim": "certificate-5314 build-procedure evidence" if eligible else "untested-environment rehearsal only",
    }
    provider_receipt = None
    if provider_path:
        provider_path, provider_receipt = create_provider(
            provider_path, source_root, crypto, bcm, report)
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(json.dumps({"build_procedure_passed": True,
                      "certificate_profile_environment": eligible,
                      "provider_dir": str(provider_path) if provider_path else None,
                      "provider_receipt": str(provider_receipt) if provider_receipt else None,
                      "report": str(report_path)}, indent=2))
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--require-offline", action="store_true", help="reject non-loopback network interfaces")
    parser.add_argument("--archive", required=True, help="unchanged AWS-LC-FIPS-3.1.0.zip")
    parser.add_argument("--work-dir", required=True, help="new directory for this build")
    parser.add_argument("--report", required=True, help="evidence JSON outside work-dir")
    parser.add_argument("--provider-dir",
                        help="new package directory receiving the unchanged library, bcm.o, headers, and receipt")
    parser.add_argument("--allow-untested-environment", action="store_true",
                        help="run as a rehearsal while recording that the certificate profile is not met")
    args = parser.parse_args()
    try:
        return reproduce(args)
    except (OSError, ValueError, KeyError, TypeError, subprocess.TimeoutExpired, zipfile.BadZipFile) as error:
        print(json.dumps({"build_procedure_passed": False, "error": str(error)}, indent=2), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
