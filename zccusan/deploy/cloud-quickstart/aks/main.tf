locals {
  tags = {
    managed-by   = "terraform"
    purpose      = "zccusan-quickstart-test"
    adhoc        = "true"
    expiry-epoch = var.expiry_epoch
  }
}

resource "azurerm_resource_group" "quickstart" {
  name     = "${var.name}-rg"
  location = var.location
  tags     = local.tags
}

resource "azurerm_virtual_network" "quickstart" {
  name                = "${var.name}-vnet"
  location            = azurerm_resource_group.quickstart.location
  resource_group_name = azurerm_resource_group.quickstart.name
  address_space       = ["10.87.0.0/16"]
  tags                = local.tags
}

resource "azurerm_subnet" "workers" {
  name                 = "workers"
  resource_group_name  = azurerm_resource_group.quickstart.name
  virtual_network_name = azurerm_virtual_network.quickstart.name
  address_prefixes     = ["10.87.0.0/20"]
}

resource "azurerm_network_security_group" "workers" {
  name                = "${var.name}-workers"
  location            = azurerm_resource_group.quickstart.location
  resource_group_name = azurerm_resource_group.quickstart.name
  tags                = local.tags

  security_rule {
    name                       = "allow-vnet-internal"
    priority                   = 100
    direction                  = "Inbound"
    access                     = "Allow"
    protocol                   = "*"
    source_port_range          = "*"
    destination_port_range     = "*"
    source_address_prefix      = "VirtualNetwork"
    destination_address_prefix = "VirtualNetwork"
  }
}

resource "azurerm_subnet_network_security_group_association" "workers" {
  subnet_id                 = azurerm_subnet.workers.id
  network_security_group_id = azurerm_network_security_group.workers.id
}

resource "azurerm_user_assigned_identity" "cluster" {
  name                = "${var.name}-identity"
  location            = azurerm_resource_group.quickstart.location
  resource_group_name = azurerm_resource_group.quickstart.name
  tags                = local.tags
}

resource "azurerm_role_assignment" "subnet" {
  scope                = azurerm_subnet.workers.id
  role_definition_name = "Network Contributor"
  principal_id         = azurerm_user_assigned_identity.cluster.principal_id
}

resource "azurerm_kubernetes_cluster" "quickstart" {
  name                = var.name
  location            = azurerm_resource_group.quickstart.location
  resource_group_name = azurerm_resource_group.quickstart.name
  dns_prefix          = var.name
  kubernetes_version  = var.kubernetes_version
  sku_tier            = "Free"

  role_based_access_control_enabled = true
  local_account_disabled            = false
  oidc_issuer_enabled               = true
  workload_identity_enabled         = true

  default_node_pool {
    name                         = "system"
    node_count                   = var.worker_node_count
    vm_size                      = var.worker_vm_size
    zones                        = [var.worker_zone]
    vnet_subnet_id               = azurerm_subnet.workers.id
    type                         = "VirtualMachineScaleSets"
    os_sku                       = "Ubuntu"
    os_disk_type                 = "Managed"
    os_disk_size_gb              = var.worker_disk_size_gb
    node_public_ip_enabled       = false
    only_critical_addons_enabled = false
    temporary_name_for_rotation  = "systemtemp"

    node_labels = {
      "storage.zcutils.io/quickstart" = "true"
    }

    upgrade_settings {
      max_surge = "1"
    }
  }

  identity {
    type         = "UserAssigned"
    identity_ids = [azurerm_user_assigned_identity.cluster.id]
  }

  api_server_access_profile {
    authorized_ip_ranges = [var.operator_cidr]
  }

  network_profile {
    network_plugin      = "azure"
    network_plugin_mode = "overlay"
    load_balancer_sku   = "standard"
    outbound_type       = "loadBalancer"
    pod_cidr            = "10.88.0.0/16"
    service_cidr        = "10.89.0.0/16"
    dns_service_ip      = "10.89.0.10"
  }

  tags = local.tags

  depends_on = [
    azurerm_role_assignment.subnet,
    azurerm_subnet_network_security_group_association.workers,
  ]
}
