#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
doc="${repo_root}/zccusan/docs/GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.md"
ha_doc="${repo_root}/zccusan/docs/VALIDATING_SINGLE_REGION_HA_ON_KUBERNETES.md"
sources=(
  zccusan/deploy/zcblock-csi/getting-started/media-grant.yaml
  zccusan/deploy/zcblock-csi/getting-started/storage-profile.yaml
  zccusan/deploy/zcblock-csi/getting-started/mirror-pvc.yaml
  zccusan/deploy/zcblock-csi/getting-started/mirror-fio.yaml
  zccusan/deploy/zcblock-csi/getting-started/mirror-pgbench.yaml
)
ha_sources=(
  zccusan/deploy/zcblock-csi/getting-started/single-region-ha-canary.yaml
  zccusan/deploy/zcblock-csi/getting-started/single-region-ha-hostpath-comparator.yaml
)

extract_document_block() {
  local document="$1"
  local source="$2"
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
  ' "${document}"
}

marker_count="$(grep -c '^<!-- BEGIN FILE: ' "${doc}")"
if [[ "${marker_count}" -ne "${#sources[@]}" ]]; then
  printf 'expected %d synchronized YAML blocks in %s, found %d\n' \
    "${#sources[@]}" "${doc}" "${marker_count}" >&2
  exit 1
fi

for source in "${sources[@]}"; do
  source_path="${repo_root}/${source}"
  if ! cmp -s "${source_path}" <(extract_document_block "${doc}" "${source}"); then
    printf 'documentation YAML drifted from %s\n' "${source}" >&2
    diff -u "${source_path}" <(extract_document_block "${doc}" "${source}") || true
    exit 1
  fi
done

for source in "${ha_sources[@]}"; do
  source_path="${repo_root}/${source}"
  [[ -s "${source_path}" ]] || { printf 'missing HA example %s\n' "${source}" >&2; exit 1; }
done

chart_version="$(sed -n 's/^version: //p' \
  "${repo_root}/zccusan/charts/zccusan-chaos-toolbox/Chart.yaml")"
[[ -n "${chart_version}" ]] || { printf 'chaos chart has no version\n' >&2; exit 1; }
grep -q -- "--version ${chart_version}" "${ha_doc}" || {
  printf 'HA guide is not pinned to chaos chart version %s\n' "${chart_version}" >&2
  exit 1
}
grep -q 'zccusan-validate-single-region-ha.sh' "${ha_doc}" || {
  printf 'HA guide does not invoke the single-region runner\n' >&2
  exit 1
}
bash -n "${repo_root}/scripts/zccusan-validate-single-region-ha.sh"

media="zccusan/deploy/zcblock-csi/getting-started/media-grant.yaml"
profile="zccusan/deploy/zcblock-csi/getting-started/storage-profile.yaml"
combined="zccusan/deploy/zcblock-csi/getting-started/mirror-ram.yaml"
if ! cmp -s "${repo_root}/${combined}" \
  <(sed -n '1,$p' "${repo_root}/${media}"; printf '%s\n' '---'; sed -n '1,$p' "${repo_root}/${profile}"); then
  printf '%s must remain the exact aggregate of %s and %s\n' \
    "${combined}" "${media}" "${profile}" >&2
  exit 1
fi

printf 'GETTING_STARTED_YAML_SYNC_PASS embedded_blocks=%d standalone_ha_examples=%d\n' \
  "${#sources[@]}" "${#ha_sources[@]}"
