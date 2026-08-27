mock_provider "azurerm" {}

run "single_zone_three_node_quickstart" {
  command = plan

  variables {
    subscription_id = "00000000-0000-0000-0000-000000000000"
    operator_cidr   = "203.0.113.10/32"
    expiry_epoch    = "1787875200"
  }

  assert {
    condition     = azurerm_kubernetes_cluster.quickstart.default_node_pool[0].node_count == 3
    error_message = "The default quickstart pool must contain three workers."
  }

  assert {
    condition     = azurerm_kubernetes_cluster.quickstart.default_node_pool[0].zones == toset([var.worker_zone])
    error_message = "Every AKS worker must remain in the selected zone."
  }

  assert {
    condition     = !azurerm_kubernetes_cluster.quickstart.default_node_pool[0].node_public_ip_enabled
    error_message = "AKS workers must not receive public IPs."
  }

  assert {
    condition     = azurerm_kubernetes_cluster.quickstart.api_server_access_profile[0].authorized_ip_ranges == toset([var.operator_cidr])
    error_message = "The AKS API must be restricted to the operator /32."
  }

  assert {
    condition     = azurerm_kubernetes_cluster.quickstart.network_profile[0].network_plugin == "azure"
    error_message = "AKS must use the VNet-native Azure network plugin."
  }

  assert {
    condition     = azurerm_kubernetes_cluster.quickstart.network_profile[0].network_plugin_mode == "overlay"
    error_message = "AKS must keep pod addressing inside the private overlay."
  }
}
