#!/usr/bin/env python3
"""Build zcblock-csi images and emit deterministic, image-bound SBOM attestations."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import uuid


AUTHORITY = "Rob J. Caskey"
SSM_KMS_PARAMETER = "/zcutils/build-attestation/signing-authority/Rob-J-Caskey/kms-key-arn"
CACHE_AUTHORITY_ANNOTATION = "signingAuthority"
CACHE_BUILDER_ANNOTATION = "builderIdentity"
SPDX_PREDICATE = "https://spdx.dev/Document"
CYCLONEDX_PREDICATE = "https://cyclonedx.org/bom"
STATEMENT_TYPE = "https://in-toto.io/Statement/v1"
SHA256_RE = re.compile(r"^sha256:([0-9a-f]{64})$")
REGISTRY_REF_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:/@-]{2,511}$")
ROOT = Path(__file__).resolve().parents[1]


def run(command: list[str], *, env: dict[str, str] | None = None) -> str:
    completed = subprocess.run(
        command,
        cwd=ROOT,
        env=env,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    return completed.stdout


def canonical_write(path: Path, value: object) -> None:
    path.write_text(
        json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n",
        encoding="utf-8",
    )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def repository_without_tag(reference: str) -> str:
    if REGISTRY_REF_RE.fullmatch(reference) is None:
        raise SystemExit(f"unsafe registry reference: {reference!r}")
    base = reference.split("@", 1)[0]
    slash = base.rfind("/")
    colon = base.rfind(":")
    if colon > slash:
        base = base[:colon]
    if not base or base.endswith("/"):
        raise SystemExit(f"invalid registry reference: {reference!r}")
    return base


def is_loopback_registry(reference: str) -> bool:
    repository = repository_without_tag(reference)
    host = repository.split("/", 1)[0].rsplit(":", 1)[0]
    return host in {"localhost", "127.0.0.1", "[::1]"}


def cosign_registry_args(reference: str, allow_loopback: bool, *, verify: bool) -> list[str]:
    if not is_loopback_registry(reference):
        if allow_loopback:
            raise SystemExit("--allow-insecure-loopback-registry is valid only for a loopback cache")
        return []
    if not allow_loopback:
        return []
    result = ["--allow-insecure-registry"]
    if verify:
        # A loopback-only registry cannot be independently queried by Rekor.
        # The pinned public/KMS key still cryptographically verifies the signature.
        result.append("--insecure-ignore-tlog=true")
    else:
        result.append("--tlog-upload=false")
    return result


def manifest_digest(engine: str, reference: str) -> str:
    raw = run(
        [
            engine,
            "buildx",
            "imagetools",
            "inspect",
            reference,
            "--format",
            "{{json .Manifest}}",
        ]
    )
    try:
        manifest = json.loads(raw)
    except json.JSONDecodeError as error:
        raise SystemExit("cache reference did not resolve to JSON manifest metadata") from error
    digest = manifest.get("digest") or manifest.get("Digest")
    if not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
        raise SystemExit("cache reference did not resolve to an immutable sha256 manifest digest")
    return digest


def cache_verification_key_id(key: str, trusted_sha256: str | None) -> str:
    if key.startswith(("awskms://", "hashivault://", "gcpkms://", "azurekms://")):
        if trusted_sha256:
            raise SystemExit("a public-key SHA-256 pin requires a local cache verification key")
        return key
    path = Path(key)
    if not path.is_file():
        raise SystemExit("cache verification key must be an independently trusted key file or KMS URI")
    actual = sha256_file(path)
    if trusted_sha256 and actual != trusted_sha256.lower():
        raise SystemExit("cache verification public-key SHA-256 does not match the trusted pin")
    return "sha256:" + actual


def resolve_and_verify_cache(args: argparse.Namespace) -> tuple[dict | None, list[dict] | None]:
    if not args.cache_from:
        return None, None
    if args.engine != "docker":
        raise SystemExit("verified registry cache import requires Docker Buildx")
    if not args.cache_verification_key:
        raise SystemExit("--cache-from requires --cache-verification-key")
    if not args.cache_builder_identity:
        raise SystemExit("--cache-from requires --cache-builder-identity")
    if shutil.which(args.cosign) is None:
        raise SystemExit(f"required executable is unavailable: {args.cosign}")
    expected_digest = None
    if "@" in args.cache_from:
        supplied_digest = args.cache_from.rsplit("@", 1)[1]
        if SHA256_RE.fullmatch(supplied_digest) is None:
            raise SystemExit("cache digest must be sha256")
        expected_digest = supplied_digest
    resolved_digest = manifest_digest(args.engine, args.cache_from)
    if expected_digest and resolved_digest != expected_digest:
        raise SystemExit("resolved cache manifest digest does not match the supplied digest")
    pinned_ref = f"{repository_without_tag(args.cache_from)}@{resolved_digest}"
    key_id = cache_verification_key_id(
        args.cache_verification_key, args.cache_trusted_public_key_sha256
    )
    verification_raw = run(
        [
            args.cosign,
            "verify",
            "--key",
            args.cache_verification_key,
            *cosign_registry_args(
                pinned_ref, args.allow_insecure_loopback_registry, verify=True
            ),
            "-a",
            f"{CACHE_AUTHORITY_ANNOTATION}={AUTHORITY}",
            "-a",
            f"{CACHE_BUILDER_ANNOTATION}={args.cache_builder_identity}",
            "--output",
            "json",
            pinned_ref,
        ]
    )
    try:
        verified = json.loads(verification_raw)
    except json.JSONDecodeError as error:
        raise SystemExit("cosign cache verification did not return JSON") from error
    if not isinstance(verified, list) or not verified:
        raise SystemExit("cosign returned no verified cache signatures")
    for claim in verified:
        critical = claim.get("critical") or claim.get("Critical") or {}
        image = critical.get("image") or critical.get("Image") or {}
        claim_digest = image.get("docker-manifest-digest") or image.get("Docker-manifest-digest")
        optional = claim.get("optional") or claim.get("Optional") or {}
        if claim_digest != resolved_digest:
            raise SystemExit("verified cache signature digest does not match the resolved manifest")
        if optional.get(CACHE_AUTHORITY_ANNOTATION) != AUTHORITY:
            raise SystemExit(f"verified cache signature authority is not {AUTHORITY}")
        if optional.get(CACHE_BUILDER_ANNOTATION) != args.cache_builder_identity:
            raise SystemExit("verified cache signature builder identity is not the trusted builder")
    info = {
        "requestedRef": args.cache_from,
        "resolvedRef": pinned_ref,
        "digest": {"sha256": resolved_digest.removeprefix("sha256:")},
        "signingAuthority": AUTHORITY,
        "builderIdentity": args.cache_builder_identity,
        "verification": {
            "method": "cosign verify --key with required signature annotation",
            "trustedKey": key_id,
            "outputSha256": sha256_bytes(verification_raw.encode("utf-8")),
            "transparencyLogVerified": not args.allow_insecure_loopback_registry,
        },
    }
    return info, verified


def validate_cache_export_ref(reference: str) -> None:
    if REGISTRY_REF_RE.fullmatch(reference) is None:
        raise SystemExit("cache export reference contains unsupported characters")
    if "@" in reference:
        raise SystemExit("cache export must use a dedicated mutable tag, not a digest target")
    slash = reference.rfind("/")
    if reference.rfind(":") <= slash:
        raise SystemExit("cache export requires an explicit dedicated tag")


def cache_export_receipt(
    engine: str,
    reference: str,
    builder_identity: str,
    *,
    signing_key: str | None = None,
    allow_insecure_loopback_registry: bool = False,
) -> dict:
    digest = manifest_digest(engine, reference)
    pinned_ref = f"{repository_without_tag(reference)}@{digest}"
    key = signing_key or "awskms:///alias/zcutils-build-attestation-signing-authority-Rob-J-Caskey"
    command = " ".join(
        shlex.quote(value)
        for value in (
            "cosign", "sign", "--yes", "--key", key,
            *cosign_registry_args(
                pinned_ref, allow_insecure_loopback_registry, verify=False
            ),
            "-a", f"{CACHE_AUTHORITY_ANNOTATION}={AUTHORITY}",
            "-a", f"{CACHE_BUILDER_ANNOTATION}={builder_identity}", pinned_ref,
        )
    )
    return {
        "exportRef": reference,
        "resolvedRef": pinned_ref,
        "digest": {"sha256": digest.removeprefix("sha256:")},
        "trusted": False,
        "signingAuthorityRequired": AUTHORITY,
        "builderIdentity": builder_identity,
        "postBuildSignCommand": command,
        "note": "Do not import this cache until the digest-pinned ref is signed and independently verified.",
    }


def sign_and_verify_cache_export(
    args: argparse.Namespace, receipt: dict, signing_key: str
) -> tuple[dict, list[dict]]:
    pinned_ref = receipt["resolvedRef"]
    run(
        [
            args.cosign,
            "sign",
            "--yes",
            "--key",
            signing_key,
            *cosign_registry_args(
                pinned_ref, args.allow_insecure_loopback_registry, verify=False
            ),
            "-a",
            f"{CACHE_AUTHORITY_ANNOTATION}={AUTHORITY}",
            "-a",
            f"{CACHE_BUILDER_ANNOTATION}={args.cache_builder_identity}",
            pinned_ref,
        ]
    )
    verification_args = argparse.Namespace(
        cache_from=pinned_ref,
        cache_verification_key=signing_key,
        cache_trusted_public_key_sha256=None,
        cache_builder_identity=args.cache_builder_identity,
        engine=args.engine,
        cosign=args.cosign,
        allow_insecure_loopback_registry=args.allow_insecure_loopback_registry,
    )
    verified_info, claims = resolve_and_verify_cache(verification_args)
    assert verified_info is not None and claims is not None
    receipt["trusted"] = True
    receipt["verification"] = verified_info["verification"]
    receipt["note"] = "Signed and immediately verified; later imports still repeat verification."
    return receipt, claims


def sign_and_verify_image(
    args: argparse.Namespace, signing_key: str, image: str, digest: str
) -> tuple[dict, list[dict]]:
    pinned_ref = f"{repository_without_tag(image)}@sha256:{digest}"
    run(
        [
            args.cosign,
            "sign",
            "--yes",
            "--key",
            signing_key,
            "-a",
            f"{CACHE_AUTHORITY_ANNOTATION}={AUTHORITY}",
            "-a",
            f"{CACHE_BUILDER_ANNOTATION}={args.cache_builder_identity}",
            pinned_ref,
        ]
    )
    verification_raw = run(
        [
            args.cosign,
            "verify",
            "--key",
            signing_key,
            "-a",
            f"{CACHE_AUTHORITY_ANNOTATION}={AUTHORITY}",
            "-a",
            f"{CACHE_BUILDER_ANNOTATION}={args.cache_builder_identity}",
            "--output",
            "json",
            pinned_ref,
        ]
    )
    claims = json.loads(verification_raw)
    if not isinstance(claims, list) or not claims:
        raise SystemExit("cosign returned no verified image signatures")
    for claim in claims:
        critical = claim.get("critical") or claim.get("Critical") or {}
        image_claim = critical.get("image") or critical.get("Image") or {}
        if image_claim.get("docker-manifest-digest") != f"sha256:{digest}":
            raise SystemExit("verified image signature digest does not match the pushed image")
    return {
        "resolvedRef": pinned_ref,
        "signingAuthority": AUTHORITY,
        "builderIdentity": args.cache_builder_identity,
        "verificationOutputSha256": sha256_bytes(verification_raw.encode("utf-8")),
    }, claims


def python_iso_timestamp(value: str) -> str:
    normalized = value[:-1] + "+00:00" if value.endswith("Z") else value
    # Docker uses RFC 3339 Nano timestamps, while Python 3.9's ISO parser
    # accepts at most six fractional digits.  Attestations record whole seconds.
    return re.sub(r"(\.\d{6})\d+(?=[+-]\d{2}:\d{2}$)", r"\1", normalized)


def inspect_image(engine: str, image: str) -> tuple[str, str]:
    inspected = json.loads(run([engine, "image", "inspect", image]))[0]
    repo_digests = sorted(inspected.get("RepoDigests") or [])
    digest = ""
    for candidate in repo_digests:
        if "@sha256:" in candidate:
            digest = "sha256:" + candidate.rsplit("@sha256:", 1)[1]
            break
    if not digest:
        digest = inspected.get("Digest", "") or inspected.get("Id", "")
    if re.fullmatch(r"[0-9a-f]{64}", digest):
        digest = "sha256:" + digest
    match = SHA256_RE.fullmatch(digest)
    if not match:
        raise SystemExit(f"image inspection did not return a sha256 digest: {digest!r}")
    created = inspected.get("Created", "")
    if created:
        parsed = dt.datetime.fromisoformat(python_iso_timestamp(created))
        if parsed.tzinfo is None or parsed.utcoffset() is None:
            raise ValueError(f"image creation timestamp has no UTC offset: {created!r}")
        created = parsed.astimezone(dt.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")
    else:
        created = "1970-01-01T00:00:00Z"
    return match.group(1), created


def append_build_cache_args(
    command: list[str], args: argparse.Namespace, cache_import: dict | None
) -> None:
    if cache_import:
        command.extend(
            ["--cache-from", f"type=registry,ref={cache_import['resolvedRef']}"]
        )
    if args.cache_export_ref:
        command.extend(
            [
                "--cache-to",
                (
                    f"type=registry,ref={args.cache_export_ref},mode=max,"
                    "oci-mediatypes=true,image-manifest=true"
                ),
            ]
        )


def build_image(args: argparse.Namespace, cache_import: dict | None = None) -> None:
    use_buildx = bool(cache_import or args.cache_export_ref)
    if use_buildx and args.engine != "docker":
        raise SystemExit("registry cache import/export requires Docker Buildx")
    build_prefix = [args.engine, "buildx", "build", "--load"] if use_buildx else [args.engine, "build"]
    if args.variant == "nonfips":
        command = [
                *build_prefix,
                "--file",
                str(ROOT / "zccusan/deploy/zcblock-csi/Dockerfile"),
                "--tag",
                args.image,
                "--build-arg",
                "IMAGE_VARIANT=nonfips",
        ]
        append_build_cache_args(command, args, cache_import)
        command.append(str(ROOT))
        run(command)
        return
    distro = os.environ.get("FIPS_DISTRO", "amzn2023")
    build_distro = os.environ.get("FIPS_BUILD_DISTRO", distro)
    provider_root = os.environ.get(
        "FIPS_PROVIDER_ROOT", "zccusan/deploy/zcblock-csi/fips/provider"
    )
    if Path(provider_root).is_absolute() or ".." in Path(provider_root).parts:
        raise SystemExit("FIPS_PROVIDER_ROOT must stay inside the repository build context")
    if not (ROOT / provider_root / "lib/libcrypto.a").is_file():
        raise SystemExit(f"FIPS provider is missing: {ROOT / provider_root}")
    command = [
        *build_prefix,
        "--file",
        str(ROOT / "zccusan/deploy/zcblock-csi/Dockerfile.fips"),
        "--tag",
        args.image,
    ]
    build_args = {
        "FIPS_DISTRO": distro,
        "FIPS_BUILD_DISTRO": build_distro,
        "FIPS_PROVIDER_ROOT": provider_root,
        "BUILD_JOBS": os.environ.get("BUILD_JOBS", "4"),
        "FIPS_REBUILD_NONCE": str(uuid.uuid4()),
        "FIPS_COMPILED_STAGE": "reproducibility-check" if os.environ.get("FIPS_COMPARE_ONLINE") == "1" else "offline-build",
    }
    for name in ("AL2023_IMAGE", "UBI_IMAGE", "RHEL_IMAGE", "UBUNTU_IMAGE", "RUST_IMAGE", "KMOD_BUNDLE_ROOT"):
        if os.environ.get(name):
            build_args[name] = os.environ[name]
    for name, value in build_args.items():
        command.extend(["--build-arg", f"{name}={value}"])
    append_build_cache_args(command, args, cache_import)
    command.append(str(ROOT))
    run(command)
    check = [args.engine, "run", "--rm", "--network", "none"]
    if args.provider_only_fips_check:
        check.extend(["--env", "ZC_REQUIRE_HOST_FIPS=0"])
    check.extend([
        "--entrypoint", "/usr/local/bin/zc-fips-check", args.image, "--require-fips"
    ])
    run(check)


def normalize_spdx(
    document: dict,
    image: str,
    variant: str,
    digest: str,
    created: str,
    cache: dict | None = None,
) -> None:
    document["name"] = f"zcblock-csi-{variant}"
    document["documentNamespace"] = f"https://zcutils.invalid/sbom/{variant}/sha256-{digest}"
    creation = document.setdefault("creationInfo", {})
    creation["created"] = created
    creators = [value for value in creation.get("creators", []) if value != f"Person: {AUTHORITY}"]
    creators.append(f"Person: {AUTHORITY}")
    creation["creators"] = creators
    creation["comment"] = (
        f"Signing authority: {AUTHORITY}; image: {image}; variant: {variant}; "
        f"subject sha256: {digest}"
    )
    if cache and cache.get("import"):
        imported = cache["import"]
        creation["comment"] += (
            f"; verified build cache: {imported['resolvedRef']}; cache authority: "
            f"{imported['signingAuthority']}; verification output sha256: "
            f"{imported['verification']['outputSha256']}; cache builder identity: "
            f"{imported['builderIdentity']}"
        )
    if cache and cache.get("export"):
        exported = cache["export"]
        creation["comment"] += (
            f"; build cache export: {exported['resolvedRef']}; trusted after verification: "
            f"{str(exported['trusted']).lower()}; required cache signing authority: "
            f"{exported['signingAuthorityRequired']}"
        )
        if exported.get("verification"):
            creation["comment"] += (
                f"; export verification output sha256: "
                f"{exported['verification']['outputSha256']}"
            )


def normalize_cyclonedx(
    document: dict,
    image: str,
    variant: str,
    digest: str,
    created: str,
    cache: dict | None = None,
) -> None:
    document["serialNumber"] = "urn:uuid:" + str(
        uuid.uuid5(uuid.NAMESPACE_URL, f"zcutils:{variant}:sha256:{digest}")
    )
    metadata = document.setdefault("metadata", {})
    metadata["timestamp"] = created
    metadata["authors"] = [{"name": AUTHORITY}]
    component = metadata.setdefault("component", {})
    component.update({"type": "container", "name": "zcblock-csi", "version": variant})
    component["bom-ref"] = f"{image}@sha256:{digest}"
    properties = [
        item for item in metadata.get("properties", [])
        if not str(item.get("name", "")).startswith("io.zcutils.attestation.")
    ]
    properties.extend(
        [
            {"name": "io.zcutils.attestation.signing-authority", "value": AUTHORITY},
            {"name": "io.zcutils.attestation.image", "value": image},
            {"name": "io.zcutils.attestation.variant", "value": variant},
            {"name": "io.zcutils.attestation.subject-sha256", "value": digest},
        ]
    )
    if cache and cache.get("import"):
        imported = cache["import"]
        properties.extend(
            [
                {"name": "io.zcutils.attestation.cache.import-ref", "value": imported["resolvedRef"]},
                {"name": "io.zcutils.attestation.cache.import-authority", "value": imported["signingAuthority"]},
                {"name": "io.zcutils.attestation.cache.import-builder-identity", "value": imported["builderIdentity"]},
                {"name": "io.zcutils.attestation.cache.verification-method", "value": imported["verification"]["method"]},
                {"name": "io.zcutils.attestation.cache.verification-output-sha256", "value": imported["verification"]["outputSha256"]},
            ]
        )
    if cache and cache.get("export"):
        exported = cache["export"]
        properties.extend(
            [
                {"name": "io.zcutils.attestation.cache.export-ref", "value": exported["resolvedRef"]},
                {"name": "io.zcutils.attestation.cache.export-trusted", "value": str(exported["trusted"]).lower()},
                {"name": "io.zcutils.attestation.cache.export-signing-authority-required", "value": exported["signingAuthorityRequired"]},
                {"name": "io.zcutils.attestation.cache.export-builder-identity", "value": exported["builderIdentity"]},
            ]
        )
        if exported.get("verification"):
            properties.append({
                "name": "io.zcutils.attestation.cache.export-verification-output-sha256",
                "value": exported["verification"]["outputSha256"],
            })
    metadata["properties"] = properties


def make_statement(image: str, digest: str, predicate_type: str, predicate: dict) -> dict:
    return {
        "_type": STATEMENT_TYPE,
        "subject": [{"name": image, "digest": {"sha256": digest}}],
        "predicateType": predicate_type,
        "predicate": predicate,
    }


def resolve_signing_key(args: argparse.Namespace) -> str | None:
    if args.cosign_key and args.kms_key_parameter:
        raise SystemExit("use either --cosign-key or --kms-key-parameter, not both")
    if not args.kms_key_parameter:
        return args.cosign_key
    if shutil.which(args.aws) is None:
        raise SystemExit(f"required executable is unavailable: {args.aws}")
    command = [args.aws]
    if args.aws_profile:
        command.extend(["--profile", args.aws_profile])
    command.extend(
        [
            "ssm",
            "get-parameter",
            "--name",
            args.kms_key_parameter,
            "--no-with-decryption",
            "--query",
            "Parameter.Value",
            "--output",
            "text",
        ]
    )
    key_id = run(command).strip()
    if not (key_id.startswith("arn:aws:kms:") or key_id.startswith("alias/")):
        raise SystemExit("SSM KMS reference must contain a KMS key ARN or alias")
    return "awskms:///" + key_id


def verify_offline_image(args):
    """Verify publication uses the offline payload recorded during compilation."""
    with tempfile.TemporaryDirectory(prefix="zc-fips-published-payload-") as temp:
        root = Path(temp)
        container = run([args.engine, "create", args.image]).strip()
        try:
            run([args.engine, "cp", container + ":/usr/local/bin", str(root / "bin")])
            run([args.engine, "cp", container + ":/usr/share/zcutils/fips/offline-build.json",
                 str(root / "offline-build.json")])
        finally:
            run([args.engine, "rm", container])
        run([sys.executable, str(ROOT / "scripts/fips-build-reproducibility.py"), "verify",
             "--binaries", str(root / "bin"), "--report", str(root / "offline-build.json")])
        args.output_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(root / "offline-build.json", args.output_dir / "offline-build.json")


def generate(args: argparse.Namespace) -> None:
    for executable in (args.engine, args.syft):
        if shutil.which(executable) is None:
            raise SystemExit(f"required executable is unavailable: {executable}")
    if args.skip_build and (args.cache_from or args.cache_export_ref):
        raise SystemExit("cache import/export options require a build")
    if args.cache_export_ref:
        validate_cache_export_ref(args.cache_export_ref)
    if (args.cache_from or args.cache_export_ref) and not args.cache_builder_identity:
        raise SystemExit("cache import/export requires --cache-builder-identity")
    if args.sign_cache_export and not args.cache_export_ref:
        raise SystemExit("--sign-cache-export requires --cache-export-ref")
    if args.sign_image and not args.cache_builder_identity:
        raise SystemExit("--sign-image requires --cache-builder-identity")
    if args.provider_only_fips_check and args.variant != "fips-aspiring":
        raise SystemExit("--provider-only-fips-check applies only to fips-aspiring builds")
    if args.allow_insecure_loopback_registry:
        for reference in (args.cache_from, args.cache_export_ref):
            if reference and not is_loopback_registry(reference):
                raise SystemExit(
                    "--allow-insecure-loopback-registry cannot authorize a non-loopback registry"
                )
    cache_import, cache_verification = resolve_and_verify_cache(args)
    if not args.skip_build:
        build_image(args, cache_import)
    if args.variant == "fips-aspiring":
        verify_offline_image(args)
    signing_key = resolve_signing_key(args)
    cache: dict[str, dict] = {}
    if cache_import:
        cache["import"] = cache_import
    if args.cache_export_ref:
        cache["export"] = cache_export_receipt(
            args.engine,
            args.cache_export_ref,
            args.cache_builder_identity,
            signing_key=signing_key,
            allow_insecure_loopback_registry=args.allow_insecure_loopback_registry,
        )
        if args.sign_cache_export:
            if signing_key is None:
                raise SystemExit("--sign-cache-export requires --cosign-key or --kms-key-parameter")
            cache["export"], export_verification = sign_and_verify_cache_export(
                args, cache["export"], signing_key
            )
        else:
            export_verification = None
    else:
        export_verification = None
    if args.push_image:
        run([args.engine, "push", args.image])
    digest, created = inspect_image(args.engine, args.image)
    if args.source_date_epoch is not None:
        created = dt.datetime.fromtimestamp(args.source_date_epoch, dt.timezone.utc).isoformat().replace("+00:00", "Z")

    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    prefix = f"zcblock-csi-{args.variant}"
    spdx = output / f"{prefix}.spdx.json"
    cyclonedx = output / f"{prefix}.cyclonedx.json"
    cache_files: list[Path] = []
    if cache_verification is not None:
        verification_path = output / f"{prefix}.cache-import-cosign-verification.json"
        canonical_write(verification_path, cache_verification)
        cache["import"]["verification"]["outputSha256"] = sha256_file(verification_path)
        cache_files.append(verification_path)
    if cache.get("export"):
        export_path = output / f"{prefix}.cache-export-signing-receipt.json"
        canonical_write(export_path, cache["export"])
        cache_files.append(export_path)
    if export_verification is not None:
        export_verification_path = output / f"{prefix}.cache-export-cosign-verification.json"
        canonical_write(export_verification_path, export_verification)
        cache["export"]["verification"]["outputSha256"] = sha256_file(
            export_verification_path
        )
        canonical_write(export_path, cache["export"])
        cache_files.append(export_verification_path)
    image_signature = None
    image_signature_verification = None
    if args.sign_image:
        if signing_key is None:
            raise SystemExit("--sign-image requires --cosign-key or --kms-key-parameter")
        image_signature, image_signature_verification = sign_and_verify_image(
            args, signing_key, args.image, digest
        )
        image_signature_path = output / f"{prefix}.image-signature-verification.json"
        canonical_write(image_signature_path, image_signature_verification)
        image_signature["verificationOutputSha256"] = sha256_file(image_signature_path)
        cache_files.append(image_signature_path)
    run([args.syft, "scan", args.image, "-o", f"spdx-json={spdx}", "-o", f"cyclonedx-json={cyclonedx}"])

    spdx_doc = json.loads(spdx.read_text(encoding="utf-8"))
    cyclonedx_doc = json.loads(cyclonedx.read_text(encoding="utf-8"))
    normalize_spdx(spdx_doc, args.image, args.variant, digest, created, cache or None)
    normalize_cyclonedx(cyclonedx_doc, args.image, args.variant, digest, created, cache or None)
    canonical_write(spdx, spdx_doc)
    canonical_write(cyclonedx, cyclonedx_doc)

    statements: list[Path] = []
    for sbom, predicate_type in ((spdx, SPDX_PREDICATE), (cyclonedx, CYCLONEDX_PREDICATE)):
        statement = output / f"{sbom.name}.intoto.json"
        canonical_write(statement, make_statement(args.image, digest, predicate_type, json.loads(sbom.read_text())))
        statements.append(statement)

    bundles: list[Path] = []
    public_key: Path | None = None
    if signing_key:
        if shutil.which(args.cosign) is None:
            raise SystemExit(f"required executable is unavailable: {args.cosign}")
        public_key = output / f"{prefix}.signing-public-key.pem"
        public_key.write_text(
            run([args.cosign, "public-key", "--key", signing_key]),
            encoding="utf-8",
        )
        for statement in statements:
            bundle = Path(str(statement) + ".cosign.bundle")
            run(
                [
                    args.cosign,
                    "sign-blob",
                    "--yes",
                    "--key",
                    signing_key,
                    "--bundle",
                    str(bundle),
                    str(statement),
                ]
            )
            bundles.append(bundle)

    files = [spdx, cyclonedx, *statements, *bundles, *cache_files]
    if args.variant == "fips-aspiring":
        files.append(output / "offline-build.json")
    if public_key is not None:
        files.append(public_key)
    manifest = {
        "schema": 1,
        "signingAuthority": AUTHORITY,
        "variant": args.variant,
        "signed": bool(signing_key),
        "subject": {"name": args.image, "digest": {"sha256": digest}},
        "files": {path.name: {"sha256": sha256_file(path)} for path in files},
    }
    if args.variant == "fips-aspiring":
        manifest["fipsRuntimeCheck"] = (
            "not-run" if args.skip_build
            else "provider-only" if args.provider_only_fips_check
            else "provider-and-host-fips"
        )
    if cache:
        manifest["buildCache"] = cache
    if image_signature:
        manifest["imageSignature"] = image_signature
    canonical_write(output / f"{prefix}.attestation-manifest.json", manifest)
    verify_directory(
        output,
        prefix,
        cosign=args.cosign,
        cosign_verification_key=str(public_key) if public_key is not None else None,
        require_signature=bool(signing_key),
    )
    print(f"wrote {len(files) + 1} verified files for {args.image}@sha256:{digest} to {output}")


def verify_directory(
    output: Path,
    prefix: str,
    *,
    cosign: str | None = None,
    cosign_verification_key: str | None = None,
    require_signature: bool = False,
    trusted_public_key_sha256: str | None = None,
) -> None:
    manifest_path = output / f"{prefix}.attestation-manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("signingAuthority") != AUTHORITY:
        raise SystemExit("attestation manifest has the wrong signing authority")
    expected_subject = manifest["subject"]
    for name, record in manifest["files"].items():
        path = output / name
        if sha256_file(path) != record["sha256"]:
            raise SystemExit(f"checksum mismatch: {name}")
        if name.endswith(".intoto.json"):
            statement = json.loads(path.read_text(encoding="utf-8"))
            if statement.get("_type") != STATEMENT_TYPE or statement.get("subject") != [expected_subject]:
                raise SystemExit(f"invalid in-toto subject: {name}")
            sbom_name = name.removesuffix(".intoto.json")
            expected_type = CYCLONEDX_PREDICATE if sbom_name.endswith(".cyclonedx.json") else SPDX_PREDICATE
            if statement.get("predicateType") != expected_type:
                raise SystemExit(f"invalid predicate type: {name}")
            if statement.get("predicate") != json.loads((output / sbom_name).read_text(encoding="utf-8")):
                raise SystemExit(f"predicate does not match SBOM: {name}")
    signed = manifest.get("signed") is True
    if (require_signature or cosign_verification_key is not None) and not signed:
        raise SystemExit("signature required, but the evidence manifest is unsigned")
    if trusted_public_key_sha256 is not None:
        if cosign_verification_key is None:
            raise SystemExit("--trusted-public-key-sha256 requires a verification key file")
        key_path = Path(cosign_verification_key)
        if not key_path.is_file():
            raise SystemExit("a pinned public-key hash can only verify a local key file")
        if sha256_file(key_path) != trusted_public_key_sha256.lower():
            raise SystemExit("verification public-key SHA-256 does not match the trusted pin")
    if signed:
        bundles = sorted(output.glob(f"{prefix}.*.intoto.json.cosign.bundle"))
        if len(bundles) != 2:
            raise SystemExit("signed evidence must contain exactly two cosign bundles")
        if cosign_verification_key is None:
            raise SystemExit("signed evidence requires an independently trusted cosign verification key")
        if not cosign or shutil.which(cosign) is None:
            raise SystemExit(f"required executable is unavailable: {cosign}")
        for bundle in bundles:
            statement = Path(str(bundle).removesuffix(".cosign.bundle"))
            run(
                [
                    cosign,
                    "verify-blob",
                    "--key",
                    cosign_verification_key,
                    "--bundle",
                    str(bundle),
                    str(statement),
                ]
            )


def verify(args: argparse.Namespace) -> None:
    verify_directory(
        args.output_dir.resolve(),
        f"zcblock-csi-{args.variant}",
        cosign=args.cosign,
        cosign_verification_key=args.cosign_verification_key,
        require_signature=args.require_signature,
        trusted_public_key_sha256=args.trusted_public_key_sha256,
    )
    print(f"verified {args.variant} attestations in {args.output_dir.resolve()}")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    create = commands.add_parser("generate")
    create.add_argument("--variant", choices=("nonfips", "fips-aspiring"), required=True)
    create.add_argument("--image", required=True)
    create.add_argument("--output-dir", type=Path, default=ROOT / "target/image-attestations")
    create.add_argument("--engine", default="docker")
    create.add_argument("--syft", default="syft")
    create.add_argument("--cosign", default="cosign")
    create.add_argument("--cosign-key", help="cosign key URI/path; use awskms:///ARN for AWS KMS")
    create.add_argument("--aws", default="aws")
    create.add_argument("--aws-profile")
    create.add_argument(
        "--kms-key-parameter",
        nargs="?",
        const=SSM_KMS_PARAMETER,
        help=f"read a KMS ARN/alias from SSM (default when flag has no value: {SSM_KMS_PARAMETER})",
    )
    create.add_argument("--source-date-epoch", type=int)
    create.add_argument("--skip-build", action="store_true")
    create.add_argument("--push-image", action="store_true", help="push the final image before scanning it")
    create.add_argument("--sign-image", action="store_true", help="sign and immediately verify the pushed image digest")
    create.add_argument(
        "--provider-only-fips-check",
        action="store_true",
        help="exercise the validated provider while recording that host FIPS mode was not verified",
    )
    create.add_argument("--cache-from", help="registry cache tag/ref to resolve and verify before import")
    create.add_argument("--cache-verification-key", help="independently trusted cosign key path or KMS URI")
    create.add_argument("--cache-trusted-public-key-sha256", help="optional trusted SHA-256 pin for a local cache verification key")
    create.add_argument("--cache-export-ref", help="dedicated mutable registry ref for an untrusted dev-cache export")
    create.add_argument(
        "--cache-builder-identity",
        help="exact independently reviewed builder identity bound into the cache signature",
    )
    create.add_argument(
        "--sign-cache-export",
        action="store_true",
        help="sign and immediately verify the exported cache before marking it trusted",
    )
    create.add_argument(
        "--allow-insecure-loopback-registry",
        action="store_true",
        help="allow plain HTTP and no transparency log only for localhost/loopback cache refs",
    )
    create.set_defaults(handler=generate)
    check = commands.add_parser("verify")
    check.add_argument("--variant", choices=("nonfips", "fips-aspiring"), required=True)
    check.add_argument("--output-dir", type=Path, default=ROOT / "target/image-attestations")
    check.add_argument("--cosign", default="cosign")
    check.add_argument(
        "--cosign-verification-key",
        "--cosign-public-key",
        dest="cosign_verification_key",
        help="independently trusted public-key path or KMS URI",
    )
    check.add_argument("--trusted-public-key-sha256", help="trusted SHA-256 pin for a local public-key file")
    check.add_argument("--require-signature", action="store_true")
    check.set_defaults(handler=verify)
    return result


def main() -> int:
    args = parser().parse_args()
    args.handler(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
