# Disposable GKE quickstart cluster

This root creates one zonal GKE Standard cluster and a separately managed pool
of three workers in that same zone. The default is `e2-standard-2` on amd64.
Set `worker_architecture = "arm64"` to select `c4a-standard-2` on Google's Axion
processor, including its required Hyperdisk boot disk and gVNIC. It uses the
Regular release channel and `COS_CONTAINERD`, matching the GKE ABI families
tracked by the zccusan kernel-module matrix.

GKE reserves `system-node-critical` and `system-cluster-critical` Pods to
namespaces with an explicit scoped ResourceQuota. The live acceptance harness
therefore clears the chart's DaemonSet priority class instead of granting a
user storage namespace permission to impersonate GKE system components.

Nodes use VPC internal addresses for the zccusan TCP data path. They retain
ephemeral external addresses only for outbound image pulls, avoiding a standing
Cloud NAT charge; the VPC has no rule allowing public ingress to workers. The
Kubernetes API accepts public traffic only from `operator_cidr`.

Authenticate application-default credentials first, then create a short-lived
variable file:

```bash
gcloud auth application-default login
PUBLIC_IP="$(curl -fsS https://checkip.amazonaws.com | tr -d '\n')"
cat > quickstart.auto.tfvars <<EOF
project_id    = "your-project"
operator_cidr = "${PUBLIC_IP}/32"
expiry_epoch  = "$(date -u -d '+2 hours' +%s)"
EOF
terraform init
terraform apply
```

Fetch credentials and run the provider-neutral quickstart acceptance test:

```bash
gcloud container clusters get-credentials zccusan-gke-quickstart \
  --zone us-central1-a --project your-project
../test-quickstart.sh
```

Always destroy the stack after collecting the result:

```bash
terraform destroy
```

The stack leaves the Compute and Kubernetes Engine project APIs enabled on
destroy because disabling a shared project API can break unrelated resources.
