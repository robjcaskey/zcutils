#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
lambda_dir="${script_dir}/../lambda"
build_dir="${script_dir}/.terraform/lambda-package"
stamp_file="${build_dir}/.source-sha256"

source_hash="$({
  sha256sum "${lambda_dir}/main.py"
  sha256sum "${lambda_dir}/dashboard.html"
  sha256sum "${lambda_dir}/requirements-lambda.txt"
} | sha256sum | cut -d' ' -f1)"

if [[ -r "${stamp_file}" ]] && [[ "$(<"${stamp_file}")" == "${source_hash}" ]]; then
  exit 0
fi

mkdir -p "${build_dir}"
find "${build_dir}" -mindepth 1 -delete

python3 -m pip install \
  --disable-pip-version-check \
  --no-compile \
  --only-binary=:all: \
  --platform manylinux2014_x86_64 \
  --implementation cp \
  --python-version 3.12 \
  --requirement "${lambda_dir}/requirements-lambda.txt" \
  --target "${build_dir}"

install -m 0644 "${lambda_dir}/main.py" "${build_dir}/main.py"
install -m 0644 "${lambda_dir}/dashboard.html" "${build_dir}/dashboard.html"
find "${build_dir}" -type d -name __pycache__ -prune -exec find {} -depth -delete \;
printf '%s\n' "${source_hash}" > "${stamp_file}"
