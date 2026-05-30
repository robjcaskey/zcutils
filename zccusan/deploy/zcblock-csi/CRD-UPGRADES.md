# Snapshot CRD Upgrade Contract

The snapshot CRDs are shared cluster-scoped API objects. They must be installed
once per cluster, even when multiple zcblock CSI driver instances are installed
to simulate regions.

Current contract:

- External snapshotter release: `v8.3.0`
- Snapshot CRD API group: `snapshot.storage.k8s.io`
- Current required storage version: `v1`
- Current supported storage versions: `v1`
- Stair-step upgrades are required before any future storage-version change.

The installer annotates each snapshot CRD with:

- `zcutils.io/snapshotter-version`
- `zcutils.io/snapshot-crd-storage-version`
- `zcutils.io/snapshot-crd-supported-versions`
- `zcutils.io/snapshot-crd-n-minus-1`
- `zcutils.io/snapshot-crd-stair-step-required=true`

When a future CRD storage version appears, zcblock releases must follow this
path:

1. Ship release `N` that supports both the currently installed CRD storage
   version and the next storage version.
2. Roll release `N` everywhere while the CRD still stores objects at the old
   version.
3. Apply the new CRDs and run any required Kubernetes storage-version migration.
4. Confirm every cluster stores the new version.
5. Only in release `N+1` may support for `N-1` be removed.

The practical rule is: a release that changes the expected CRD storage version
must also support the previous storage version. If the installer sees an
installed snapshot CRD storage version outside its supported set, it stops
instead of applying the CRD so operators can upgrade through an intermediate
release.
