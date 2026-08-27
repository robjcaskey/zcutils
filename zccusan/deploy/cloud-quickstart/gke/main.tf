locals {
  worker_machine_type = coalesce(
    var.worker_machine_type,
    var.worker_architecture == "arm64" ? "c4a-standard-2" : "e2-standard-2",
  )
  worker_disk_type = coalesce(
    var.worker_disk_type,
    var.worker_architecture == "arm64" ? "hyperdisk-balanced" : "pd-balanced",
  )
  labels = {
    managed_by   = "terraform"
    purpose      = "zccusan-quickstart-test"
    adhoc        = "true"
    expiry_epoch = var.expiry_epoch
  }
}

resource "google_project_service" "required" {
  for_each = toset([
    "compute.googleapis.com",
    "container.googleapis.com",
  ])

  project            = var.project_id
  service            = each.value
  disable_on_destroy = false
}

resource "google_compute_network" "quickstart" {
  name                    = var.name
  auto_create_subnetworks = false
  routing_mode            = "REGIONAL"

  depends_on = [google_project_service.required]
}

resource "google_compute_subnetwork" "quickstart" {
  name                     = var.name
  region                   = var.region
  network                  = google_compute_network.quickstart.id
  ip_cidr_range            = "10.84.0.0/20"
  private_ip_google_access = true

  secondary_ip_range {
    range_name    = "${var.name}-pods"
    ip_cidr_range = "10.85.0.0/16"
  }

  secondary_ip_range {
    range_name    = "${var.name}-services"
    ip_cidr_range = "10.86.0.0/20"
  }
}

resource "google_compute_network" "rail1" {
  count = var.worker_additional_nic ? 1 : 0

  name                    = "${var.name}-rail1"
  auto_create_subnetworks = false
  routing_mode            = "REGIONAL"

  depends_on = [google_project_service.required]
}

resource "google_compute_subnetwork" "rail1" {
  count = var.worker_additional_nic ? 1 : 0

  name                     = "${var.name}-rail1"
  region                   = var.region
  network                  = google_compute_network.rail1[0].id
  ip_cidr_range            = "10.87.0.0/20"
  private_ip_google_access = true
}

resource "google_compute_firewall" "rail1_internal" {
  count = var.worker_additional_nic ? 1 : 0

  name      = "${var.name}-rail1-internal"
  network   = google_compute_network.rail1[0].name
  direction = "INGRESS"
  priority  = 900

  source_ranges = [google_compute_subnetwork.rail1[0].ip_cidr_range]

  allow {
    protocol = "tcp"
  }

  allow {
    protocol = "udp"
  }

  allow {
    protocol = "icmp"
  }

  target_tags = [var.name]
}

# GKE creates the control-plane and kubelet rules. This explicit rule documents
# and permits the zccusan userspace data path only within this disposable VPC.
resource "google_compute_firewall" "internal" {
  name      = "${var.name}-internal"
  network   = google_compute_network.quickstart.name
  direction = "INGRESS"
  priority  = 900

  source_ranges = [
    google_compute_subnetwork.quickstart.ip_cidr_range,
    "10.85.0.0/16",
    "10.86.0.0/20",
  ]

  allow {
    protocol = "tcp"
  }

  allow {
    protocol = "udp"
  }

  allow {
    protocol = "icmp"
  }

  target_tags = [var.name]
}

resource "google_service_account" "nodes" {
  project      = var.project_id
  account_id   = "${var.name}-nodes"
  display_name = "Disposable zccusan GKE quickstart nodes"

  depends_on = [google_project_service.required]
}

resource "google_project_iam_member" "nodes" {
  project = var.project_id
  role    = "roles/container.defaultNodeServiceAccount"
  member  = "serviceAccount:${google_service_account.nodes.email}"
}

resource "google_container_cluster" "quickstart" {
  name     = var.name
  location = var.zone

  deletion_protection      = false
  remove_default_node_pool = true
  initial_node_count       = 1
  min_master_version       = var.kubernetes_version

  network    = google_compute_network.quickstart.id
  subnetwork = google_compute_subnetwork.quickstart.id

  networking_mode         = "VPC_NATIVE"
  datapath_provider       = var.worker_additional_nic ? "ADVANCED_DATAPATH" : null
  enable_multi_networking = var.worker_additional_nic
  ip_allocation_policy {
    cluster_secondary_range_name  = "${var.name}-pods"
    services_secondary_range_name = "${var.name}-services"
  }

  release_channel {
    channel = "REGULAR"
  }

  master_authorized_networks_config {
    cidr_blocks {
      cidr_block   = var.operator_cidr
      display_name = "terraform-operator"
    }
  }

  addons_config {
    http_load_balancing {
      disabled = true
    }
  }

  resource_labels = local.labels

  depends_on = [
    google_compute_firewall.internal,
    google_compute_firewall.rail1_internal,
    google_project_service.required,
    google_project_iam_member.nodes,
  ]
}

resource "google_container_node_pool" "quickstart" {
  name               = "default-pool"
  location           = var.zone
  cluster            = google_container_cluster.quickstart.name
  initial_node_count = var.worker_node_count
  version            = var.kubernetes_version

  node_config {
    machine_type    = local.worker_machine_type
    image_type      = var.worker_image_type
    disk_type       = local.worker_disk_type
    disk_size_gb    = var.worker_disk_size_gb
    spot            = var.worker_spot
    service_account = google_service_account.nodes.email
    oauth_scopes    = ["https://www.googleapis.com/auth/cloud-platform"]

    labels = {
      "storage.zcutils.io/quickstart" = "true"
    }

    tags = [var.name]

    metadata = {
      disable-legacy-endpoints = "true"
    }

    shielded_instance_config {
      enable_secure_boot          = false
      enable_integrity_monitoring = true
    }

    dynamic "gvnic" {
      for_each = var.worker_architecture == "arm64" || var.worker_gvnic ? [true] : []
      content {
        enabled = true
      }
    }
  }

  dynamic "network_config" {
    for_each = var.worker_additional_nic || var.worker_tier_1_networking ? [true] : []
    content {
      dynamic "additional_node_network_configs" {
        for_each = var.worker_additional_nic ? [true] : []
        content {
          network    = google_compute_network.rail1[0].id
          subnetwork = google_compute_subnetwork.rail1[0].id
        }
      }

      dynamic "network_performance_config" {
        for_each = var.worker_tier_1_networking ? [true] : []
        content {
          total_egress_bandwidth_tier = "TIER_1"
        }
      }
    }
  }

  depends_on = [
    google_compute_firewall.internal,
    google_compute_firewall.rail1_internal,
    google_project_iam_member.nodes,
  ]
}
