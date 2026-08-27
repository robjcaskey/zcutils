variable "project_id" {
  description = "GCP project in which to create the disposable GKE cluster."
  type        = string
}

variable "name" {
  description = "Name prefix for the disposable cluster and network resources."
  type        = string
  default     = "zccusan-gke-quickstart"

  validation {
    condition     = length(var.name) <= 22 && can(regex("^[a-z][a-z0-9-]*[a-z0-9]$", var.name))
    error_message = "name must be a lowercase GCP name no longer than 22 characters."
  }
}

variable "region" {
  description = "GCP region containing the selected worker zone."
  type        = string
  default     = "us-central1"
}

variable "zone" {
  description = "The one zone used by both the GKE control plane and all workers."
  type        = string
  default     = "us-central1-a"
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
  description = "Unix epoch after which this disposable cluster should no longer exist; retained as a cloud label for auditing."
  type        = string

  validation {
    condition     = can(tonumber(var.expiry_epoch)) && tonumber(var.expiry_epoch) > 0
    error_message = "expiry_epoch must be a positive Unix epoch represented as a string."
  }
}

variable "kubernetes_version" {
  description = "Optional exact GKE control-plane/node version. Null uses the current Regular-channel default."
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

variable "worker_machine_type" {
  description = "Optional GCE worker type. Null selects e2-standard-2 for amd64 or c4a-standard-2 for Google Axion arm64."
  type        = string
  default     = null
  nullable    = true
}

variable "worker_architecture" {
  description = "Worker CPU architecture: amd64 or Google Axion arm64."
  type        = string
  default     = "amd64"

  validation {
    condition     = contains(["amd64", "arm64"], var.worker_architecture)
    error_message = "worker_architecture must be amd64 or arm64."
  }
}

variable "worker_disk_type" {
  description = "Optional boot-disk type. Null selects pd-balanced for amd64 or the Hyperdisk required by C4A."
  type        = string
  default     = null
  nullable    = true
}

variable "worker_image_type" {
  description = "GKE node image. Ubuntu is required for customer-built out-of-tree modules; CPU COS enforces LoadPin."
  type        = string
  default     = "UBUNTU_CONTAINERD"

  validation {
    condition     = contains(["UBUNTU_CONTAINERD", "COS_CONTAINERD"], var.worker_image_type)
    error_message = "worker_image_type must be UBUNTU_CONTAINERD or COS_CONTAINERD."
  }
}

variable "worker_spot" {
  description = "Use Spot workers. Disabled by default so a short acceptance run is not interrupted."
  type        = bool
  default     = false
}

variable "worker_disk_size_gb" {
  description = "Balanced persistent boot disk size for each worker."
  type        = number
  default     = 30

  validation {
    condition     = var.worker_disk_size_gb >= 20 && var.worker_disk_size_gb <= 100
    error_message = "worker_disk_size_gb must be between 20 and 100 GiB."
  }
}

variable "worker_gvnic" {
  description = "Enable gVNIC on amd64 workers. C4 and other high-bandwidth machine series require this."
  type        = bool
  default     = false
}

variable "worker_additional_nic" {
  description = "Attach a second gVNIC from a dedicated VPC/subnet to every worker for explicit dual-rail tests."
  type        = bool
  default     = false
}

variable "worker_tier_1_networking" {
  description = "Request Tier 1 aggregate VM networking where the selected machine type supports it."
  type        = bool
  default     = false
}
