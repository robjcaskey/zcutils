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
  description = "Region in which the attestation signing key is held."
  default     = "us-east-1"
}

variable "github_oidc_subjects" {
  type        = set(string)
  description = "Exact GitHub OIDC subjects allowed to use the attestation signing key."
  default     = ["repo:robjcaskey/zcutils:ref:refs/heads/main"]
  validation {
    condition = length(var.github_oidc_subjects) > 0 && alltrue([
      for subject in var.github_oidc_subjects : can(regex("^repo:robjcaskey/zcutils:(ref:refs/heads/main|environment:[A-Za-z0-9_.-]+)$", subject))
    ])
    error_message = "Signer subjects must be the zcutils main branch or an exact zcutils GitHub environment."
  }
}
