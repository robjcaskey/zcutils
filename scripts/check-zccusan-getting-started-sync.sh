#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
doc="${repo_root}/zccusan/docs/GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md"
sources=(
  zccusan/deploy/zcblock-csi/getting-started/media-grant.yaml
  zccusan/deploy/zcblock-csi/getting-started/storage-profile.yaml
  zccusan/deploy/zcblock-csi/getting-started/mirror-pvc.yaml
  zccusan/deploy/zcblock-csi/getting-started/mirror-fio.yaml
  zccusan/deploy/zcblock-csi/getting-started/mirror-pgbench.yaml
)

extract_document_block() {
  local source="$1"
  awk -v source="${source}" '
    BEGIN {
      begin = "<!-- BEGIN FILE: " source " -->"
      end = "<!-- END FILE: " source " -->"
    }
    $0 == begin {
      if (seen != 0 || state != 0) exit 10
      seen = 1
      if ((getline <= 0) || $0 != "```yaml") exit 11
      state = 1
      next
    }
    state == 1 && $0 == "```" {
      if ((getline <= 0) || $0 != end) exit 12
      state = 2
      complete = 1
      next
    }
    state == 1 { print }
    END {
      if (seen != 1 || complete != 1 || state != 2) exit 13
    }
  ' "${doc}"
}

marker_count="$(grep -c '^<!-- BEGIN FILE: ' "${doc}")"
if [[ "${marker_count}" -ne "${#sources[@]}" ]]; then
  printf 'expected %d synchronized YAML blocks in %s, found %d\n' \
    "${#sources[@]}" "${doc}" "${marker_count}" >&2
  exit 1
fi

for source in "${sources[@]}"; do
  source_path="${repo_root}/${source}"
  if ! cmp -s "${source_path}" <(extract_document_block "${source}"); then
    printf 'documentation YAML drifted from %s\n' "${source}" >&2
    diff -u "${source_path}" <(extract_document_block "${source}") || true
    exit 1
  fi
done

media="zccusan/deploy/zcblock-csi/getting-started/media-grant.yaml"
profile="zccusan/deploy/zcblock-csi/getting-started/storage-profile.yaml"
combined="zccusan/deploy/zcblock-csi/getting-started/mirror-ram.yaml"
if ! cmp -s "${repo_root}/${combined}" \
  <(sed -n '1,$p' "${repo_root}/${media}"; printf '%s\n' '---'; sed -n '1,$p' "${repo_root}/${profile}"); then
  printf '%s must remain the exact aggregate of %s and %s\n' \
    "${combined}" "${media}" "${profile}" >&2
  exit 1
fi

printf 'GETTING_STARTED_YAML_SYNC_PASS blocks=%d\n' "${#sources[@]}"
