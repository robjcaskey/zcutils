# zccusan chaos toolbox

This optional chart installs bounded, cloud-neutral Linux fault primitives. It
is intentionally separate from the zccusan CSI chart and receives no
Kubernetes API credentials. A test harness chooses a fault and checks the
workload; the toolbox only performs that fault.

The default chart can terminate an exact PID or executable and temporarily
blackhole an exact TCP/UDP peer and port. It schedules only on nodes explicitly
labeled `chaos.zcutils.io/allowed=true`. Node shutdown is off by default and
requires both `faults.nodePoweroff.enabled=true` and
`faults.nodePoweroff.acknowledgeRisk=true`.

```bash
kubectl label node some-test-node chaos.zcutils.io/allowed=true
helm install zccusan-chaos \
  oci://registry-1.docker.io/robjcaskey/zccusan-chaos-toolbox \
  --version 0.1.0 --namespace zccusan-chaos --create-namespace
```

The [single-region reliability guide](../../docs/VALIDATING_SINGLE_REGION_HA_ON_KUBERNETES.md)
uses this chart and keeps the fault commands out of the happy-path storage
installation.

Every process fault accepts an exact PID, executable basename, or full
Kubernetes container ID from its cgroup. Broad executable matches are refused
unless `--all` is explicit. Every network fault requires a reusable experiment
ID, an exact port, a duration of at most one hour, and optionally an exact peer
IP. The tool removes its nftables table when the duration ends or it receives
SIGTERM/SIGINT. After an uncatchable container or node loss, remove a remaining
table explicitly with:

```bash
zccusan-chaos-toolbox network-restore --experiment EXPERIMENT_ID
```

This chart is for disposable reliability tests. Remove the opt-in node label
and uninstall the chart when the test is finished.
