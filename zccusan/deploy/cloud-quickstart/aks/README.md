# Disposable AKS quickstart cluster

This root creates an AKS Free-tier control plane and three
`Standard_D2s_v5` Ubuntu workers in availability zone `1`. Azure controls the
managed control plane's placement; the worker pool itself is explicitly
single-zone.

Workers have no public IPs. zccusan uses their VNet internal addresses, while
the managed Standard Load Balancer provides outbound HTTPS for pinned image
pulls. The stack does not create an inbound application load balancer, and the
Kubernetes API accepts public traffic only from `operator_cidr`.

Authenticate with Azure CLI or standard `ARM_*` provider variables, then create
a short-lived variable file:

```bash
az login
PUBLIC_IP="$(curl -fsS https://checkip.amazonaws.com | tr -d '\n')"
cat > quickstart.auto.tfvars <<EOF
subscription_id = "your-subscription-id"
operator_cidr   = "${PUBLIC_IP}/32"
expiry_epoch    = "$(date -u -d '+2 hours' +%s)"
EOF
terraform init
terraform apply
```

Fetch credentials and run the provider-neutral quickstart acceptance test:

```bash
az aks get-credentials --resource-group zccusan-aks-quickstart-rg \
  --name zccusan-aks-quickstart
../test-quickstart.sh
```

Without Azure CLI, keep the sensitive file outside the checkout:

```bash
KUBECONFIG_FILE="$(mktemp)"
terraform output -raw kube_config >"${KUBECONFIG_FILE}"
chmod 600 "${KUBECONFIG_FILE}"
KUBECONFIG="${KUBECONFIG_FILE}" ../test-quickstart.sh
```

Always destroy the stack after collecting the result:

```bash
terraform destroy
```
