# Disposable managed-Kubernetes quickstart clusters

These Terraform roots create the smallest topology that exercises the narrated
zccusan Kubernetes quickstart without putting storage traffic on a public path:

- one worker zone;
- exactly three small workers by default;
- no worker public ingress;
- node-to-node and Pod-to-node traffic over private addresses; and
- a public Kubernetes API restricted to the operator's single IPv4 `/32`.

The GKE root uses a zonal control plane and worker pool. AKS manages control
plane placement itself, so its explicit single-zone guarantee applies to the
worker pool. Both roots label every resource as disposable and require an
expiry epoch, but Terraform destruction remains the authoritative cleanup.

The workers need outbound HTTPS to pull the pinned public chart and container
images. The GKE test deliberately leaves external addresses on the nodes rather
than paying for Cloud NAT; no firewall rule permits public ingress to them. AKS
uses the managed Standard Load Balancer outbound path and does not give workers
public IPs. Neither stack creates a workload-facing public load balancer.

Use the provider-specific README:

- [GKE](gke/README.md)
- [AKS](aks/README.md)

After either stack has produced a kubeconfig, run the same acceptance script:

```bash
KUBECONFIG=/path/to/disposable-kubeconfig \
  zccusan/deploy/cloud-quickstart/test-quickstart.sh
```

The script selects three Ready nodes, labels two as example servers and the
third as the example client, installs chart `0.1.6`, and runs the checked-in fio
and pgbench quickstart resources. It writes evidence under `bench-results/` and
cleans up the test resources. Destroy the Terraform root even if the acceptance
test fails.
