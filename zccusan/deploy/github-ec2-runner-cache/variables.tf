variable "aws_account_id" {
  type        = string
  description = "Expected AWS account ID."
  validation {
    condition     = can(regex("^[0-9]{12}$", var.aws_account_id))
    error_message = "aws_account_id must be 12 digits."
  }
}

variable "aws_region" {
  type        = string
  description = "Region containing the dedicated runner network."
  default     = "us-east-1"
}

variable "availability_zone" {
  type        = string
  description = "One exact AZ shared by the static volume and ephemeral runner."
}

variable "runner_pool" {
  type        = string
  description = "Exact RunnerPool tag required on an attach target."
  default     = "zcutils-fips-runner"
}

variable "controller_role_name" {
  type        = string
  description = "Existing ephemeral-runner controller role that receives only the narrow attach policy."
  default     = "zcutils-fips-runner-controller"
  validation {
    condition     = can(regex("^[A-Za-z0-9+=,.@_-]{1,64}$", var.controller_role_name))
    error_message = "controller_role_name must be one exact IAM role name."
  }
}

variable "kms_key_id" {
  type        = string
  description = "Optional symmetric KMS key ARN for EBS encryption; null uses the account EBS default."
  default     = null
}
