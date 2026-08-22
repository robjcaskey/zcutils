# Getting started with cross-region replication on Kubernetes

`CrossRegionReplication` reconciles an authenticated AES-256 encrypted
checkpoint transfer directly between two node backplane addresses. It creates
one short-lived sender Pod and one receiver Pod; it does not create a Service
and does not require multicast.

This first executable contract is deliberately `AsynchronousCheckpoint`, not
live WAL replication. It therefore cannot affect the source volume's local
flush/FUA acknowledgement. `automaticFailover: true` and every other mode are
rejected until the encrypted live-WAL and lease-fencing contracts are joined.

## Prepare and apply

Create the source checkpoint file on its source node and edit the node names,
paths, byte count, and secret in the template:

```bash
cp zccusan/deploy/zcblock-csi/getting-started/cross-region-checkpoint.template.yaml \
  /tmp/cross-region-checkpoint.yaml
$EDITOR /tmp/cross-region-checkpoint.yaml
kubectl apply -f /tmp/cross-region-checkpoint.yaml
```

Generate a bounded-lifetime `zct1` credential with `zcrepl token`, store it in
the Secret, and rotate it before expiry. Arbitrary non-expiring bearer strings
are rejected. The operator never
reads it or copies it into the CRD, Pod arguments, logs, or status. Kubernetes
injects it only as the `ZCREPL_TOKEN` environment variable from the named
Secret.

Watch the transfer:

```bash
kubectl get crossregionreplication example-us-to-uk-checkpoint -w
kubectl get pods -l storage.zcutils.io/cross-region-replication=example-us-to-uk-checkpoint -o wide
```

The phases progress through `StartingReceiver`, `Replicating`, and `Ready`.
`Ready` is published only after both programs succeed and the receiver calls
`sync_data` on the target. At that point `acceptedHwm`, `remoteDurableHwm`, and
`remoteAppliedHwm` all equal the declared byte count. While a transfer is in
flight all three remain zero; the controller does not infer progress from a
running process.

Changing the CRD creates generation-specific Pods and removes older ones.
Disabling or deleting the CRD also removes its Pods. `allowTargetOverwrite`
must be explicit because applying a checkpoint changes the target host file.
Host files are terminal media selected after the userspace replication policy;
they are not mirror or stripe primitives.

## Security and failover boundary

The payload stream is authenticated and encrypted with AES-256. Its native
framing is not TLS, so this is the high-performance encrypted protocol rather
than the optional check-the-box TLS mode. Promotion remains a separate,
quorum-fenced act. This CRD never claims that a completed checkpoint grants a
writer lease or constitutes zero-RPO failover.

The existing disposable QEMU failover matrix still tests clean and explicitly
declared-loss live failover separately:

```bash
ZCGLOBAL_REPLICATION_MODE=async ZCGLOBAL_SCENARIO=clean \
  scripts/zcglobal-kubernetes-failover-qemu.sh
```

See [global transport security](../../docs/GLOBAL_TRANSPORT_SECURITY.md) and
[global volume failover](../../docs/global-volume-failover.md) for those
contracts.
