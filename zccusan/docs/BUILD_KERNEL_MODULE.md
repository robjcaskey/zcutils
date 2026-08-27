# Build a zccusan kernel module

A kernel module must be built against the prepared build tree for the exact
kernel that will load it. The commands below are one concrete example for an
Amazon Linux 2023 machine; adapt the package-manager step for other Linux
systems, but keep the exact `uname -r` check.

From a reviewed zcutils checkout, run:

```bash
set -euo pipefail

kernel="$(uname -r)"
kernel_series="$(printf '%s\n' "$kernel" | cut -d. -f1,2)"
case "$kernel_series" in
  *[!0-9.]*) printf 'unexpected kernel release: %s\n' "$kernel" >&2; exit 1 ;;
esac

sudo dnf install -y \
  gcc make elfutils-libelf-devel kmod \
  "kernel${kernel_series}-devel-${kernel}"

test -r "/lib/modules/${kernel}/build/Makefile"
scripts/zccusan-build-kmod-on-linux-host.sh \
  --output ./dist/zccusan-kmods
```

The final line begins with `ZCCUSAN_KMOD_BUILD_READY`. Inspect the exact
artifact rather than selecting a nearby kernel:

```bash
module="./dist/zccusan-kmods/$(uname -m)/$(uname -r)/zcnblk_client_mod.ko"
test -r "$module"
sha256sum -c "${module}.sha256"
test "$(modinfo -F name "$module")" = zcnblk_client_mod
case "$(modinfo -F vermagic "$module")" in
  "$(uname -r) "*) ;;
  *) printf 'module does not match the running kernel\n' >&2; exit 1 ;;
esac
```

To package it for the Helm chart, choose a repository you control and publish
the resulting image by immutable digest:

```bash
export MODULE_FILE="$module"
export MODULE_ARCH="$(uname -m)"
export KERNEL_RELEASE="$(uname -r)"
export IMAGE="registry.example.com/storage/zccusan-kmod:${KERNEL_RELEASE}-${MODULE_ARCH}"

zccusan/deploy/zcblock-csi/build-kmod-image.sh
docker push "$IMAGE"
docker inspect --format '{{json .RepoDigests}}' "$IMAGE"
```

Record both the OCI digest and the module SHA-256 in the values supplied to
Helm. If the kernel enforces module signatures, sign the final module with a
key that kernel trusts before calculating its final checksum and packaging it.

Fetch these instructions for inspection without piping them into a shell:

```bash
curl --fail --location --proto '=https' \
  --output BUILD_KERNEL_MODULE.md \
  https://raw.githubusercontent.com/robjcaskey/zcutils/main/zccusan/docs/BUILD_KERNEL_MODULE.md
less BUILD_KERNEL_MODULE.md
```

The rendered guide is on
[GitHub](https://github.com/robjcaskey/zcutils/blob/main/zccusan/docs/BUILD_KERNEL_MODULE.md).
