output "cluster_name" {
  value = google_container_cluster.quickstart.name
}

output "cluster_location" {
  value = google_container_cluster.quickstart.location
}

output "cluster_version" {
  value = google_container_cluster.quickstart.master_version
}

output "worker_node_pool" {
  value = google_container_node_pool.quickstart.name
}

output "worker_version" {
  value = google_container_node_pool.quickstart.version
}

output "worker_machine_type" {
  value = local.worker_machine_type
}

output "worker_architecture" {
  value = var.worker_architecture
}

output "worker_disk_type" {
  value = local.worker_disk_type
}

output "worker_image_type" {
  value = var.worker_image_type
}

output "data_path" {
  value = "same-zone VPC InternalIP; public ingress is not permitted"
}

output "get_credentials_command" {
  value = "gcloud container clusters get-credentials ${google_container_cluster.quickstart.name} --zone ${var.zone} --project ${var.project_id}"
}
