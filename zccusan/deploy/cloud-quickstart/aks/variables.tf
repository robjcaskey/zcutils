variable "subscription_id" {
  description = "Azure subscription in which to create the disposable AKS cluster."
  type        = string
  sensitive   = true
}

variable "name" {
  description = "Name prefix for the disposable AKS and network resources."
  type        = string
  default     = "zccusan-aks-quickstart"

  validation {
    condition     = length(var.name) <= 40 && can(regex("^[a-zA-Z][a-zA-Z0-9-]*[a-zA-Z0-9]$", var.name))
    error_message = "name must be an Azure-compatible name no longer than 40 characters."
  }
}

variable "location" {
  description = "Azure region containing the selected worker zone."
  type        = string
  default     = "East US 2"
}

variable "worker_zone" {
  description = "The one availability zone used by all workers."
  type        = string
  default     = "1"
}

variable "operator_cidr" {
  description = "Operator public IPv4 address as a /32; this is the only public source allowed to reach the Kubernetes API."
  type        = string

  validation {
    condition = (
      can(cidrhost(var.operator_cidr, 0)) &&
      can(tonumber(split("/", var.operator_cidr)[1])) &&
      tonumber(split("/", var.operator_cidr)[1]) == 32
    )
    error_message = "operator_cidr must be one IPv4 /32, for example 203.0.113.10/32."
  }
}

variable "expiry_epoch" {
  description = "Unix epoch after which this disposable cluster should no longer exist; retained as an Azure tag for auditing."
  type        = string

  validation {
    condition     = can(tonumber(var.expiry_epoch)) && tonumber(var.expiry_epoch) > 0
    error_message = "expiry_epoch must be a positive Unix epoch represented as a string."
  }
}

variable "kubernetes_version" {
  description = "Optional exact AKS Kubernetes version. Null uses the current service default."
  type        = string
  default     = null
  nullable    = true
}

variable "worker_node_count" {
  description = "Worker count. The narrated quickstart requires at least three nodes."
  type        = number
  default     = 3

  validation {
    condition     = var.worker_node_count >= 3 && var.worker_node_count <= 8
    error_message = "worker_node_count must be between three and eight."
  }
}

variable "worker_vm_size" {
  description = "Small zonal VM SKU used by the AKS quickstart workers."
  type        = string
  default     = "Standard_D2s_v5"
}

variable "worker_disk_size_gb" {
  description = "Managed OS disk size for each worker."
  type        = number
  default     = 30

  validation {
    condition     = var.worker_disk_size_gb >= 30 && var.worker_disk_size_gb <= 128
    error_message = "worker_disk_size_gb must be between 30 and 128 GiB."
  }
}
