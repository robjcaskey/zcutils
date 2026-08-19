#!/usr/bin/env bash
set -euo pipefail

environment="${1:-}"
case "${environment}" in
  dev)
    tfvars="development.tfvars"
    ;;
  prod)
    tfvars="production.tfvars"
    ;;
  *)
    echo "usage: $0 dev|prod [plan|apply|output]" >&2
    exit 64
    ;;
esac

action="${2:-plan}"
case "${action}" in
  plan|apply|output) ;;
  *)
    echo "action must be plan, apply, or output" >&2
    exit 64
    ;;
esac

shift "$(( $# >= 2 ? 2 : $# ))"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "${script_dir}"

"${script_dir}/build-lambda-package.sh"

aws_profile="${AWS_PROFILE:-tf}"
state_bucket="${ZCCUSAN_TERRAFORM_STATE_BUCKET:-caskey-terraform-state-storage}"
state_region="${ZCCUSAN_TERRAFORM_STATE_REGION:-us-east-1}"
state_key="zcutils/community-survey/${environment}/terraform.tfstate"

AWS_PROFILE="${aws_profile}" terraform init -reconfigure \
  -backend-config="bucket=${state_bucket}" \
  -backend-config="key=${state_key}" \
  -backend-config="region=${state_region}" \
  -backend-config="profile=${aws_profile}" \
  -backend-config="encrypt=true" \
  -backend-config="use_lockfile=true"

if [[ "${action}" == "output" ]]; then
  AWS_PROFILE="${aws_profile}" terraform output
else
  AWS_PROFILE="${aws_profile}" terraform "${action}" -var-file="${tfvars}" "$@"
fi
