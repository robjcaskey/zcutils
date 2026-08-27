output "resource_group_name" {
  value = azurerm_resource_group.quickstart.name
}

output "cluster_name" {
  value = azurerm_kubernetes_cluster.quickstart.name
}

output "node_resource_group" {
  value = azurerm_kubernetes_cluster.quickstart.node_resource_group
}

output "cluster_version" {
  value = azurerm_kubernetes_cluster.quickstart.kubernetes_version
}

output "worker_vm_size" {
  value = var.worker_vm_size
}

output "data_path" {
  value = "same-zone VNet InternalIP; workers have no public IPs"
}

output "get_credentials_command" {
  value = "az aks get-credentials --resource-group ${azurerm_resource_group.quickstart.name} --name ${azurerm_kubernetes_cluster.quickstart.name}"
}

output "kube_config" {
  description = "Ephemeral administrative kubeconfig for CI/testing when Azure CLI is unavailable."
  value       = azurerm_kubernetes_cluster.quickstart.kube_config_raw
  sensitive   = true
}
