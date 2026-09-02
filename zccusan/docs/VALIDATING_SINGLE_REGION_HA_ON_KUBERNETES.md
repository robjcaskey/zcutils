# Optional next step: validate single-region reliability

This short exercise keeps writing to the mirrored volume from the getting
started guide while real failures are introduced. It gives you one clear
`PASS` or `FAIL` result; you do not need to learn zccusan's internal processes
or ports to run it.

Use a disposable three-node cluster. The RAM storage from the getting started
guide is perfect for testing recovery behavior, but it does not retain data
after a node is powered off. Repeat a production durability test with your own
dedicated persistent devices.

This page covers one Kubernetes cluster in one region. Cross-region recovery
is the [next, separate tutorial](GETTING_STARTED_WITH_CROSS_REGION_REPLICATION_ON_KUBERNETES.md).

## 1. Install the optional chaos toolbox

The toolbox is a separate chart and is never installed with the CSI. Label only
the three disposable nodes used by this example:

```bash
kubectl label node some-node-a chaos.zcutils.io/allowed=true
kubectl label node some-node-b chaos.zcutils.io/allowed=true
kubectl label node some-node-c chaos.zcutils.io/allowed=true
```

Install the pinned chart from Docker Hub:

```bash
helm install zccusan-chaos \
  oci://registry-1.docker.io/robjcaskey/zccusan-chaos-toolbox \
  --version 0.1.0 \
  --namespace zccusan-chaos \
  --create-namespace
```

The toolbox has no Kubernetes API credentials. It can affect only opted-in
nodes, and node poweroff is disabled in this installation.

## 2. Run the check

Finish the main getting started guide through pgbench, then delete pgbench so
its `ReadWriteOnce` volume can be mounted by the reliability canary:

```bash
kubectl delete -f mirror-pgbench.yaml --ignore-not-found
```

Run the check from your `zcutils` checkout. Repeating the current context in the
command is an intentional safety confirmation:

```bash
scripts/zccusan-validate-single-region-ha.sh \
  --confirm-context "$(kubectl config current-context)"
```

The check takes about a minute. It performs five easy-to-read checks:

1. The mirrored-volume canary can write, synchronize, and read back an
   increasing sequence.
2. Restarting the zccusan controller does not stop the established data path.
3. A host-path control workload proves the process fault really reached the
   selected node.
4. One storage worker is terminated and Kubernetes restarts it.
5. One exact storage connection is cut for five seconds and automatically
   restored.

A healthy run ends like this:

```text
PASS  preflight                volume=... storage_worker=... mirror=...
PASS  baseline                 sequence 1 -> 2
PASS  control-restart          sequence 2 -> 3
PASS  comparator               fault reached host-path-backed control workload
PASS  storage-process          sequence 3 -> 4
PASS  network-link             sequence 4 -> 5

RESULT: PASS — every injected fault was observed and the mirrored-volume canary resumed valid commits.
```

A failed check is useful evidence, not something to waive. The script leaves
the fault bounded, prints which behavior failed, and removes its two test Pods.
It does not delete your mirrored PVC.

## Optional: power off a disposable server

Process and link loss are the safe first test. A real node shutdown is more
disruptive, so it has two additional opt-ins and requires the node name to be
repeated exactly. If you intend to continue to this step, rerun the validation
with `--keep-test-pods`, then enable this test only when losing one example
server is safe:

```bash
helm upgrade zccusan-chaos \
  oci://registry-1.docker.io/robjcaskey/zccusan-chaos-toolbox \
  --version 0.1.0 \
  --namespace zccusan-chaos \
  --reuse-values \
  --set faults.nodePoweroff.enabled=true \
  --set faults.nodePoweroff.acknowledgeRisk=true
```

Choose the node hosting the host-path comparator and resolve the toolbox on
that same node:

```bash
TARGET_NODE="$(kubectl -n zccusan get pod zc-single-region-ha-hostpath \
  -o jsonpath='{.spec.nodeName}')"
TOOLBOX="$(kubectl -n zccusan-chaos get pod \
  -l app.kubernetes.io/name=zccusan-chaos-toolbox \
  --field-selector "spec.nodeName=${TARGET_NODE}" \
  -o jsonpath='{.items[0].metadata.name}')"
kubectl -n zccusan-chaos exec "${TOOLBOX}" -- \
  zccusan-chaos-toolbox node-poweroff --confirm-node "${TARGET_NODE}"
```

The command connection should end because that node powered off. The host-path
control Pod becomes unavailable with its node; the mirrored canary on the
client node should either continue or visibly fail the reliability claim. Watch
its strictly increasing committed sequence with:

```bash
kubectl -n zccusan logs --follow zc-single-region-ha-canary
```

## Clean up

Remove the test workloads, toolbox, and node opt-in labels:

```bash
kubectl -n zccusan delete pod zc-single-region-ha-canary \
  zc-single-region-ha-hostpath --ignore-not-found
helm uninstall zccusan-chaos --namespace zccusan-chaos
kubectl delete namespace zccusan-chaos
kubectl label node some-node-a chaos.zcutils.io/allowed-
kubectl label node some-node-b chaos.zcutils.io/allowed-
kubectl label node some-node-c chaos.zcutils.io/allowed-
```

Return to the main guide's storage tear-down when you are finished.
