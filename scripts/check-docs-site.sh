#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ $# -gt 1 ]]; then
  printf 'usage: %s [OUTPUT_DIRECTORY]\n' "$0" >&2
  exit 2
fi
build_dir="$(mktemp -d "${TMPDIR:-/tmp}/zc-md.XXXXXXXX")"
trap 'rm -rf -- "${build_dir}"' EXIT
rustc --edition=2024 --test "${repo_root}/scripts/zc-md.rs" -o "${build_dir}/tests"
"${build_dir}/tests"
rustc --edition=2024 -O "${repo_root}/scripts/zc-md.rs" -o "${build_dir}/zc-md"
"${build_dir}/zc-md" --site "${repo_root}" "${1:-${repo_root}/docs-site/dist}"

"${build_dir}/zc-md" --diskpipe-site "${repo_root}" "${1:-${repo_root}/docs-site/dist}/diskpipe"
