variable "aws_account_id" {
  description = "Expected AWS account; the provider refuses to use another account."
  type        = string
  validation {
    condition     = can(regex("^[0-9]{12}$", var.aws_account_id))
    error_message = "aws_account_id must be a 12-digit account ID."
  }
}

variable "aws_region" {
  description = "Only this region is authorized for worker resources and schedules."
  type        = string
  default     = "us-east-1"
}

variable "name_prefix" {
  description = "Role/name prefix and immutable RunnerPool tag value for this pool."
  type        = string
  default     = "zcutils-fips-runner"
  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{2,35}$", var.name_prefix))
    error_message = "Use 3-36 lowercase letters, digits, or hyphens, starting with a letter."
  }
}

variable "github_oidc_subjects" {
  description = "Exact GitHub OIDC sub claims to trust. Check the repository's actual subject format."
  type        = set(string)
  validation {
    condition = length(var.github_oidc_subjects) > 0 && alltrue([
      for subject in var.github_oidc_subjects :
      can(regex("^repo:[^:*?]+/[^:*?]+:(ref:refs/(heads|tags)/[^*?]+|environment:[^*?]+)$", subject))
    ])
    error_message = "Provide exact repository branch, tag, or environment subjects; wildcards and pull_request subjects are forbidden."
  }
}

variable "existing_github_oidc_provider_arn" {
  description = "Reuse the account's GitHub provider if it already exists; null creates it."
  type        = string
  default     = null
  validation {
    condition = var.existing_github_oidc_provider_arn == null || can(regex(
      "^arn:aws:iam::${var.aws_account_id}:oidc-provider/token[.]actions[.]githubusercontent[.]com$",
      var.existing_github_oidc_provider_arn
    ))
    error_message = "The existing GitHub provider must belong to aws_account_id in the commercial AWS partition."
  }
}

variable "allowed_subnet_ids" {
  description = "Existing subnets to use when create_public_network is false."
  type        = set(string)
  default     = []
  validation {
    condition     = (var.create_public_network ? length(var.allowed_subnet_ids) == 0 : length(var.allowed_subnet_ids) > 0) && alltrue([for id in var.allowed_subnet_ids : can(regex("^subnet-[0-9a-f]{8}([0-9a-f]{9})?$", id))])
    error_message = "Leave subnet IDs empty for the dedicated network, or supply concrete existing IDs when create_public_network is false."
  }
}

variable "allowed_security_group_ids" {
  description = "Existing security groups to use when create_public_network is false; the controller cannot edit their rules."
  type        = set(string)
  default     = []
  validation {
    condition     = (var.create_public_network ? length(var.allowed_security_group_ids) == 0 : length(var.allowed_security_group_ids) > 0) && alltrue([for id in var.allowed_security_group_ids : can(regex("^sg-[0-9a-f]{8}([0-9a-f]{9})?$", id))])
    error_message = "Leave security group IDs empty for the dedicated network, or supply concrete existing IDs when create_public_network is false."
  }
}

variable "create_public_network" {
  description = "Create a separate VPC, public subnet, internet gateway, and outbound-only security group (no hourly charge). Never changes existing VPC routing."
  type        = bool
  default     = true
}

variable "public_subnet_cidr" {
  description = "Unused IPv4 CIDR within the selected VPC for the new runner subnet."
  type        = string
  default     = "10.84.0.0/24"
  validation {
    condition     = can(cidrnetmask(var.public_subnet_cidr))
    error_message = "public_subnet_cidr must be an IPv4 CIDR."
  }
}

variable "max_volume_gib" {
  description = "Maximum size of each encrypted gp3 volume; runtime must also bound volume/instance counts."
  type        = number
  default     = 64
  validation {
    condition     = var.max_volume_gib >= 10 && var.max_volume_gib <= 256 && floor(var.max_volume_gib) == var.max_volume_gib
    error_message = "max_volume_gib must be a whole number between 10 and 256."
  }
}

variable "enable_key_fallback" {
  description = "Optional IAM user whose only permission is assuming the controller role. Prefer OIDC."
  type        = bool
  default     = false
  validation {
    condition     = !var.enable_key_fallback || try(length(trimspace(var.key_pgp_public_key_base64)) > 0, false)
    error_message = "Key fallback requires key_pgp_public_key_base64 so no plaintext AWS secret is written to Terraform state."
  }
}

variable "key_pgp_public_key_base64" {
  description = "Base64 of a binary OpenPGP public key (gpg --export, not ASCII armor). Required only for key fallback."
  type        = string
  default     = null
}
