output "volume_id" {
  value       = aws_ebs_volume.build_cache.id
  description = "Static 20 GiB gp3 cache volume; attach only in its availability zone."
}

output "attach_policy_arn" {
  value       = aws_iam_policy.attach_cache.arn
  description = "Administrator-reviewed policy attached to the configured runner controller."
}

output "availability_zone" {
  value       = aws_ebs_volume.build_cache.availability_zone
  description = "Runner launches using this cache must use a subnet in this exact AZ."
}
