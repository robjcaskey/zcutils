mock_provider "google" {}

run "single_zone_three_node_quickstart" {
  command = plan

  variables {
    project_id          = "example-project"
    operator_cidr       = "203.0.113.10/32"
    expiry_epoch        = "1787875200"
    worker_architecture = "amd64"
  }

  assert {
    condition     = google_container_cluster.quickstart.location == var.zone
    error_message = "The GKE control plane must be zonal."
  }

  assert {
    condition     = google_container_node_pool.quickstart.initial_node_count == 3
    error_message = "The default quickstart pool must contain three workers."
  }

  assert {
    condition     = google_container_cluster.quickstart.location == var.zone
    error_message = "Every GKE worker must remain in the selected zone."
  }

  assert {
    condition     = google_container_node_pool.quickstart.node_config[0].image_type == "UBUNTU_CONTAINERD"
    error_message = "The quickstart needs Ubuntu so its customer-built out-of-tree module is not blocked by CPU COS LoadPin."
  }

  assert {
    condition     = google_container_node_pool.quickstart.node_config[0].machine_type == "e2-standard-2"
    error_message = "The inexpensive amd64 default must remain e2-standard-2."
  }

  assert {
    condition     = google_container_node_pool.quickstart.node_config[0].disk_type == "pd-balanced"
    error_message = "The inexpensive amd64 default must use a balanced persistent boot disk."
  }

  assert {
    condition     = one(google_container_cluster.quickstart.master_authorized_networks_config[0].cidr_blocks).cidr_block == var.operator_cidr
    error_message = "The GKE API must be restricted to the operator /32."
  }

  assert {
    condition     = !contains(google_compute_firewall.internal.source_ranges, "0.0.0.0/0")
    error_message = "The worker firewall must never allow public ingress."
  }
}

run "google_axion_arm64_quickstart" {
  command = plan

  variables {
    project_id          = "example-project"
    name                = "zccusan-gke-axion"
    operator_cidr       = "203.0.113.10/32"
    expiry_epoch        = "1787875200"
    worker_architecture = "arm64"
  }

  assert {
    condition     = google_container_node_pool.quickstart.initial_node_count == 3
    error_message = "The Axion quickstart pool must contain three workers."
  }

  assert {
    condition     = google_container_node_pool.quickstart.node_config[0].machine_type == "c4a-standard-2"
    error_message = "Google Axion tests must use the inexpensive two-vCPU C4A shape."
  }

  assert {
    condition     = google_container_node_pool.quickstart.node_config[0].disk_type == "hyperdisk-balanced"
    error_message = "C4A workers require a Hyperdisk boot disk."
  }

  assert {
    condition     = google_container_node_pool.quickstart.node_config[0].gvnic[0].enabled
    error_message = "C4A workers must enable gVNIC."
  }
}

run "amd64_dual_gvnic_performance_topology" {
  command = plan

  variables {
    project_id               = "example-project"
    operator_cidr            = "203.0.113.10/32"
    expiry_epoch             = "1787875200"
    worker_architecture      = "amd64"
    worker_machine_type      = "c4-standard-48"
    worker_disk_type         = "hyperdisk-balanced"
    worker_gvnic             = true
    worker_additional_nic    = true
    worker_tier_1_networking = true
  }

  assert {
    condition     = google_container_cluster.quickstart.datapath_provider == "ADVANCED_DATAPATH"
    error_message = "GKE multi-networking requires Dataplane V2."
  }

  assert {
    condition     = google_container_cluster.quickstart.enable_multi_networking
    error_message = "The dual-rail topology must enable GKE multi-networking."
  }

  assert {
    condition     = length(google_container_node_pool.quickstart.network_config[0].additional_node_network_configs) == 1
    error_message = "Every performance worker must have exactly one additional node-network."
  }

  assert {
    condition     = google_container_node_pool.quickstart.node_config[0].gvnic[0].enabled
    error_message = "C4 workers require gVNIC."
  }

  assert {
    condition     = google_container_node_pool.quickstart.network_config[0].network_performance_config[0].total_egress_bandwidth_tier == "TIER_1"
    error_message = "The performance topology must request Tier 1 aggregate networking."
  }
}
