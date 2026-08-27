# Kernel module artifact lifecycle

The Kubernetes API has three cluster-scoped resources:

- `ZccusanKernelModuleSource` selects nodes, user-owned regional HTTP(S)
  origins, signed catalogs, and public verification keys.
- `ZccusanKernelModuleCatalog` maps an exact module, architecture, kernel, and
  userspace ABI match to an immutable bundle-manifest digest.
- `ZccusanKernelModuleBundle` describes one module, exact compatibility,
  immutable build inputs, provenance, attestations, and detached signatures.

Source URLs exist only in `ZccusanKernelModuleSource`. Catalog and bundle
objects are portable across regions. Every artifact key is relative and ends
in its SHA-256 digest, so a static origin cannot mutate a previously published
object without detection. The project does not supply a default HTTP service.

`moduleSigner` is deliberately absent. A signer is an observed result of
cryptographically verifying a detached signature against a public key selected
by the consuming region; it is not a claim that a manifest can make about
itself. Likewise, node bootstrap inspects the module ELF and hashes the actual
bytes. The kernel's embedded module-signature enforcement remains a separate,
mandatory production trust boundary; a signed bundle is not a substitute for
the kernel accepting the module.

## Build, inspect, sign, and validate

Build a module for the exact target kernel using the reproducible build
environment your organization owns. The existing image helper packages that
result for the Helm `image` source. For a distribution-neutral workflow that
compiles on an exact-match throwaway node and downloads the result, see
[Build a kernel-module bundle on a throwaway Kubernetes node](../zccusan/docs/BUILD_KERNEL_MODULE_ON_KUBERNETES_NODE.md).

```console
MODULE_FILE=./zcnblk_client_mod.ko \
KERNEL_RELEASE=7.2.0 \
MODULE_ARCH=x86_64 \
IMAGE=registry.example.test/storage/zcnblk-kmod:linux-7.2.0-amd64 \
  zccusan/deploy/zcblock-csi/build-kmod-image.sh
```

Inspect the actual ELF metadata, create an Ed25519 release key, and sign the
canonical manifest bytes. The signing command refuses to overwrite keys or
signatures. Keep the unencrypted PKCS#8 private key in an appropriate signing
system; only the raw public key belongs in the referenced ConfigMap.

```console
cargo run --bin zccusan-kmod-bundle -- inspect ./zcnblk_client_mod.ko
cargo run --bin zccusan-kmod-bundle -- keygen release-private.pk8 release-public.raw
cargo run --bin zccusan-kmod-bundle -- sign bundle.yaml release-private.pk8 bundle.sig
cargo run --bin zccusan-kmod-bundle -- verify bundle.yaml release-public.raw bundle.sig
cargo run --bin zccusan-kmod-bundle -- validate bundle.yaml ./zcnblk_client_mod.ko
```

Sign the final byte representation and do not reformat it afterward. Publish
the module, manifest, signatures, attestations, and catalog under their
content-addressed object paths to every region for which the organization is
responsible. Then apply regional source policy and the portable metadata:

```console
kubectl apply -f zccusan/deploy/zcblock-csi/kernel-module-artifacts.example.yaml
kubectl get zckms,zckmc,zckmb
```

The operator reports semantic acceptance or rejection in status, but it is not
an artifact relay and node bootstrap must not depend on the operator being
available. Kubernetes object admission is also not proof that detached
signatures or module bytes are valid; a consuming node must repeat those checks.

This change establishes the artifact API, validation/signing tool, generated
CRDs, and operator status contract. The current Helm loader continues to use
the explicit `nodeSetup.moduleSource` `image` or `http` settings; applying a
`ZccusanKernelModuleSource` does not silently override a running DaemonSet.
Catalog-driven node resolution should be introduced as an explicit source mode
with a node-local resolver, not by making the loader call or wait for the
operator.

## Regional isolation

A source policy's `nodeSelector` limits which nodes can use it. Topology hints
only order equivalent origins and never confer trust. Regions that do not trust
one another should use different source objects and different public keys, and
RBAC must prevent either regional administrator from changing the other's
cluster-scoped source policy or key ConfigMap.

Kernel modules are node-wide, not namespace-wide. Namespace isolation is not a
safe boundary for mutually distrusting kernel-module publishers on the same
node; use separate node pools or clusters for that case.
