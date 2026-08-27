# zccusan kernel-module fast-path profile.
# This file is sourced only by scripts/zccusan-install-kmod-fastpath.sh from
# the checked-out repository; it is not an environment-variable interface.
ZCCUSAN_KMOD_PROFILE_FORMAT=1
ZCCUSAN_KMOD_PROFILE_NAME=al2023-6.12.100-125.179-x86_64
ZCCUSAN_KMOD_OS_IMAGE_PATTERN='Amazon Linux 2023'
ZCCUSAN_KMOD_NODE_ARCH=amd64
ZCCUSAN_KMOD_MACHINE_ARCH=x86_64
ZCCUSAN_KMOD_KERNEL_RELEASE=6.12.100-125.179.amzn2023.x86_64
ZCCUSAN_KMOD_VERMAGIC='6.12.100-125.179.amzn2023.x86_64 SMP preempt mod_unload modversions '
ZCCUSAN_KMOD_IMAGE_REPOSITORY=docker.io/robjcaskey/zcblock-csi
ZCCUSAN_KMOD_IMAGE_DIGEST=sha256:e05c6bb339d35ff3444a55f95dcbaba5d3776d08e03ce7ee22f8a9aafd767bd2
ZCCUSAN_KMOD_MODULE_SHA256=3c9207090a167e38b42e731c4567964219a9943426768e9a645dd03bdb3bfa55
ZCCUSAN_KMOD_MODULE_SIZE_BYTES=1626440
ZCCUSAN_KMOD_VALIDATED_AT=2026-08-27T01:03:00Z
ZCCUSAN_KMOD_VALIDATION='matching Amazon Linux 2023 machine: public OCI pull by digest, packaged checksum, vermagic, insmod, shm block/control devices, direct null-backend reads, and rmmod'
