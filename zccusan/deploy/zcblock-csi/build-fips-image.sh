#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
FIPS_DISTRO="${FIPS_DISTRO:-amzn2023}"
case "$FIPS_DISTRO" in amzn2023|ubi9|rhel9|ubuntu) ;; *) echo 'FIPS_DISTRO must be amzn2023, ubi9, rhel9 or ubuntu' >&2; exit 2 ;; esac
FIPS_BUILD_DISTRO="${FIPS_BUILD_DISTRO:-$FIPS_DISTRO}"
case "${FIPS_ACCEPTANCE:-0}" in 0|1) ;; *) echo 'FIPS_ACCEPTANCE must be 0 or 1' >&2; exit 2 ;; esac
case "$FIPS_BUILD_DISTRO:$FIPS_DISTRO" in
    amzn2023:amzn2023|ubuntu:ubuntu|ubi9:ubi9|rhel9:rhel9|rhel9:ubi9) ;;
    *) echo "Unsupported builder/runtime pair: $FIPS_BUILD_DISTRO:$FIPS_DISTRO" >&2; exit 2 ;;
esac
IMAGE="${IMAGE:-localhost/zcblock-csi-fips-${FIPS_DISTRO}:dev}"
engine=("${CONTAINER_ENGINE:-podman}")
if [[ "${FIPS_ACCEPTANCE:-0}" == 1 && "${CONTAINER_ENGINE:-podman}" != podman ]]; then
    echo 'FIPS acceptance requires local Podman on the node being checked' >&2
    exit 2
fi
if [[ -n "${FIPS_PODMAN_STORAGE:-}" ]]; then
    [[ "${CONTAINER_ENGINE:-podman}" == podman ]] || { echo 'FIPS_PODMAN_STORAGE requires podman' >&2; exit 2; }
    engine+=(--root "$FIPS_PODMAN_STORAGE/root" --runroot "$FIPS_PODMAN_STORAGE/run")
fi
FIPS_PROVIDER_ROOT="${FIPS_PROVIDER_ROOT:-zccusan/deploy/zcblock-csi/fips/provider}"
case "$FIPS_PROVIDER_ROOT" in /*|../*|*/../*|*/..) echo 'FIPS_PROVIDER_ROOT must stay inside the repository build context' >&2; exit 2 ;; esac
[[ -f "$ROOT/$FIPS_PROVIDER_ROOT/lib/libcrypto.a" ]] || {
    echo "FIPS provider is missing: $ROOT/$FIPS_PROVIDER_ROOT" >&2
    echo 'Create it with scripts/fips-recompile-aws-lc.py --provider-dir inside the build context.' >&2
    exit 2
}
args=(--build-arg "FIPS_DISTRO=$FIPS_DISTRO" --build-arg "FIPS_BUILD_DISTRO=$FIPS_BUILD_DISTRO" \
      --build-arg "FIPS_PROVIDER_ROOT=$FIPS_PROVIDER_ROOT" --build-arg "BUILD_JOBS=${BUILD_JOBS:-4}")
for name in AL2023_IMAGE UBI_IMAGE RHEL_IMAGE UBUNTU_IMAGE RUST_IMAGE KMOD_BUNDLE_ROOT; do
    [[ -z "${!name:-}" ]] || args+=(--build-arg "$name=${!name}")
done
"${engine[@]}" build -f "$ROOT/zccusan/deploy/zcblock-csi/Dockerfile.fips" -t "$IMAGE" "${args[@]}" "$ROOT"
# Only provider mode is checked on the build host. Guest checks separately
# require kernel FIPS mode; this result must never be called validation.
"${engine[@]}" run --rm --network none --entrypoint /usr/local/bin/zc-fips-check "$IMAGE" --require-fips
"${engine[@]}" image inspect "$IMAGE" --format '{{.Id}}'
if [[ "${FIPS_ACCEPTANCE:-0}" == 1 ]]; then
    acceptance=(python3 "$ROOT/scripts/fips-acceptance.py" check --image "$IMAGE"
        --report "${FIPS_ACCEPTANCE_REPORT:-$ROOT/target/fips-acceptance/report.json}")
    [[ -z "${FIPS_PODMAN_STORAGE:-}" ]] || acceptance+=(--storage "$FIPS_PODMAN_STORAGE")
    [[ -z "${FIPS_VALIDATED_SOURCE:-}" ]] || acceptance+=(--validated-source "$FIPS_VALIDATED_SOURCE")
    [[ -z "${FIPS_ACCEPTANCE_REVIEW:-}" ]] || acceptance+=(--review "$FIPS_ACCEPTANCE_REVIEW")
    "${acceptance[@]}"
fi
printf 'Built %s (builder=%s, runtime=%s; FIPS-aspiring; certificate coverage is not asserted)\n' "$IMAGE" "$FIPS_BUILD_DISTRO" "$FIPS_DISTRO"
