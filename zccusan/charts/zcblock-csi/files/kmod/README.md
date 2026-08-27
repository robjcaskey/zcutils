# Bundled zcnblk client source

These four build inputs are vendored from the repository's canonical client
edge implementation so Helm can install on a node without a source checkout:

- `kmods/zcnblk_client_mod.c`
- `kmods/zcnblk_shm_abi.h`
- `zccusan/deploy/zcblock-csi/zcnblk-client-only.mk` as `Makefile`
- `zccusan/deploy/zcblock-csi/zcnblk-client-only.kbuild` as `Kbuild`, for
  kernels that generate an external-module output `Makefile`

Before packaging the chart, update these copies and verify them with `cmp`.
They must remain byte-for-byte identical to the canonical inputs.
