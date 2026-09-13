#!/usr/bin/env bash
# Run only on an already FIPS-enabled RHEL 9 disposable test host.
# Produces a small build artifact and provider evidence, not product validation.
set -euo pipefail
out=${1:?usage: rhel-fips-ec2-probe.sh OUTPUT_DIRECTORY}
mkdir -p "$out"
out=$(realpath "$out")
[[ $(id -u) == 0 ]] || { echo 'Run this probe as root on the disposable RHEL guest.' >&2; exit 1; }
source /etc/os-release
[[ $ID == rhel && $VERSION_ID == 9.* ]]
[[ $(uname -m) == x86_64 ]]
[[ $(cat /proc/sys/crypto/fips_enabled) == 1 ]]
fips-mode-setup --check > "$out/host-fips-mode.txt"
provider_rpms="$out/provider-rpms"
mkdir -p "$provider_rpms"
provider_cdn=https://cdn-ubi.redhat.com/content/public/ubi/dist/ubi9/9/x86_64/baseos/os/Packages/o
for package in openssl-fips-provider openssl-fips-provider-so; do
    curl --fail --location --proto '=https' "$provider_cdn/$package-3.0.7-6.el9_5.x86_64.rpm" \
        --output "$provider_rpms/$package-3.0.7-6.el9_5.x86_64.rpm"
done
for package in openssl openssl-libs; do
    curl --fail --location --proto '=https' "$provider_cdn/$package-3.2.2-6.el9_5.1.x86_64.rpm" \
        --output "$provider_rpms/$package-3.2.2-6.el9_5.1.x86_64.rpm"
done
curl --fail --location --proto '=https' \
    https://cdn-ubi.redhat.com/content/public/ubi/dist/ubi9/9/x86_64/appstream/os/Packages/o/openssl-devel-3.2.2-6.el9_5.1.x86_64.rpm \
    --output "$provider_rpms/openssl-devel-3.2.2-6.el9_5.1.x86_64.rpm"
cat > "$provider_rpms/SHA256SUMS" <<'HASHES'
bd9266695b8238ed6fe436ae5f613cee2e5e1ee5d612ab495f1da2f21f2830aa  openssl-fips-provider-3.0.7-6.el9_5.x86_64.rpm
451372cea98f4993b2a4a2ed5876f1a661450e7487f73f945bbaf34789931437  openssl-fips-provider-so-3.0.7-6.el9_5.x86_64.rpm
f379686df99db814e30568a896b417278775fc96864ac6d2660bf48ef94309e3  openssl-3.2.2-6.el9_5.1.x86_64.rpm
287d11706d44a53455ed8ac62faab4c4a0b8c0fa5e367adf122c7a76c6ddbbb8  openssl-libs-3.2.2-6.el9_5.1.x86_64.rpm
30cd1b3dec089a7da71e9167532693bef7c202a5dbe3c010af2a9387106a0b36  openssl-devel-3.2.2-6.el9_5.1.x86_64.rpm
HASHES
(cd "$provider_rpms" && sha256sum -c SHA256SUMS)
dnf install -y --setopt=localpkg_gpgcheck=1 "$provider_rpms"/*.rpm
dnf install -y --exclude='openssl*' --exclude='fips-provider-next*' gcc podman
rpm -q openssl openssl-libs openssl-fips-provider openssl-fips-provider-so kernel-core podman | tee "$out/packages.txt"
rpm -V openssl-fips-provider openssl-fips-provider-so > "$out/provider-rpm-verification.txt"
openssl list -providers | tee "$out/host-providers.txt"
update-crypto-policies --show > "$out/host-crypto-policy.txt"
cp /etc/os-release "$out/os-release"
uname -a > "$out/kernel.txt"
lscpu > "$out/cpu.txt"

cat > "$out/openssl-fips-probe.c" <<'C'
#include <stdio.h>
#include <string.h>
#include <openssl/core_names.h>
#include <openssl/crypto.h>
#include <openssl/err.h>
#include <openssl/evp.h>
#include <openssl/params.h>
#include <openssl/provider.h>
#include <openssl/rand.h>

int main(void) {
    const unsigned char expected[32] = {
        0xba,0x78,0x16,0xbf,0x8f,0x01,0xcf,0xea,
        0x41,0x41,0x40,0xde,0x5d,0xae,0x22,0x23,
        0xb0,0x03,0x61,0xa3,0x96,0x17,0x7a,0x9c,
        0xb4,0x10,0xff,0x61,0xf2,0x00,0x15,0xad
    };
    OSSL_PROVIDER *provider = OSSL_PROVIDER_load(NULL, "fips");
    char *name = NULL, *version = NULL;
    int status = 0;
    OSSL_PARAM params[] = {
        OSSL_PARAM_construct_utf8_ptr(OSSL_PROV_PARAM_NAME, &name, 0),
        OSSL_PARAM_construct_utf8_ptr(OSSL_PROV_PARAM_VERSION, &version, 0),
        OSSL_PARAM_construct_int(OSSL_PROV_PARAM_STATUS, &status),
        OSSL_PARAM_construct_end()
    };
    if (!provider || !OSSL_PROVIDER_get_params(provider, params) || status != 1 ||
        !name || !version ||
        strcmp(name, "Red Hat Enterprise Linux 9 - OpenSSL FIPS Provider") ||
        strcmp(version, "3.0.7-395c1a240fbfffd8") ||
        !EVP_default_properties_enable_fips(NULL, 1)) return 1;
    EVP_MD *sha = EVP_MD_fetch(NULL, "SHA256", "fips=yes");
    unsigned char digest[EVP_MAX_MD_SIZE], random_bytes[32];
    unsigned int size = 0;
    if (!sha || EVP_MD_get0_provider(sha) != provider ||
        EVP_Digest("abc", 3, digest, &size, sha, NULL) != 1 ||
        size != sizeof(expected) || CRYPTO_memcmp(digest, expected, sizeof(expected))) return 2;
    if (RAND_bytes(random_bytes, sizeof(random_bytes)) != 1) return 3;
    OPENSSL_cleanse(random_bytes, sizeof(random_bytes));
    EVP_MD *md5 = EVP_MD_fetch(NULL, "MD5", "fips=yes");
    if (md5) { EVP_MD_free(md5); return 4; }
    ERR_clear_error();
    printf("{\"provider\":\"%s\",\"version\":\"%s\",\"sha256_known_answer\":true,"
           "\"random_generation\":true,\"md5_unavailable_with_fips_query\":true}\n", name, version);
    EVP_MD_free(sha);
    OSSL_PROVIDER_unload(provider);
    return 0;
}
C
gcc -O2 -Wall -Wextra -Werror "$out/openssl-fips-probe.c" -lcrypto -o "$out/openssl-fips-probe"
ldd "$out/openssl-fips-probe" > "$out/binary-libraries.txt"
"$out/openssl-fips-probe" > "$out/host-probe.json"
(cd "$out" && sha256sum openssl-fips-probe openssl-fips-probe.c) > "$out/artifacts.sha256"

cat > "$out/Containerfile" <<'CONTAINER'
FROM registry.access.redhat.com/ubi9/ubi@sha256:dec374e05cc13ebbc0975c9f521f3db6942d27f8ccdf06b180160490eef8bdbc
COPY provider-rpms/ /tmp/zc-provider-rpms/
RUN cd /tmp/zc-provider-rpms && sha256sum -c SHA256SUMS && \
    dnf install -y --setopt=localpkg_gpgcheck=1 ./*.rpm && \
    dnf clean all && rm -rf /tmp/zc-provider-rpms
COPY openssl-fips-probe /usr/local/bin/openssl-fips-probe
ENTRYPOINT ["/usr/local/bin/openssl-fips-probe"]
CONTAINER
image=localhost/zc-rhel-fips-smoke:local
podman build -f "$out/Containerfile" -t "$image" "$out"
podman image inspect "$image" > "$out/container-image.json"
podman image inspect registry.access.redhat.com/ubi9/ubi@sha256:dec374e05cc13ebbc0975c9f521f3db6942d27f8ccdf06b180160490eef8bdbc > "$out/container-base-image.json"
podman run --rm --network none --entrypoint rpm "$image" -q openssl openssl-libs openssl-fips-provider openssl-fips-provider-so > "$out/container-packages.txt"
podman run --rm --network none --entrypoint rpm "$image" -V openssl-fips-provider openssl-fips-provider-so > "$out/container-provider-rpm-verification.txt"
podman run --rm --network none --entrypoint cat "$image" /proc/sys/crypto/fips_enabled > "$out/container-kernel-fips.txt"
[[ $(cat "$out/container-kernel-fips.txt") == 1 ]]
podman run --rm --network none --entrypoint update-crypto-policies "$image" --show > "$out/container-crypto-policy.txt"
[[ $(cat "$out/container-crypto-policy.txt") == FIPS ]]
podman run --rm --network none --entrypoint openssl "$image" list -providers | tee "$out/container-providers.txt"
podman run --rm --network none "$image" > "$out/container-probe.json"
python3 - "$out" <<'PY'
import json
import pathlib
import sys
p = pathlib.Path(sys.argv[1])
report = {
    'passed': True,
    'host_kernel_fips_enabled': True,
    'container_kernel_fips_enabled': True,
    'podman_container_fips_policy': True,
    'host': json.loads((p / 'host-probe.json').read_text()),
    'container': json.loads((p / 'container-probe.json').read_text()),
    'module_certificate_reference': '4857',
    'module_package_verification_passed': True,
    'container_module_package_verification_passed': True,
    'openssl_package_set_pinned': True,
    'ubi_base_digest_pinned': True,
    'scope': 'small OpenSSL application build and RHEL/UBI mode smoke test',
    'zcutils_build': 'not_run',
    'github_runner_job': 'not_run',
    'deployment_validation_claim': 'none',
}
(p / 'probe-report.json').write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps(report, indent=2))
PY
