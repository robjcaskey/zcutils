#!/usr/bin/env bash
set -euo pipefail

REGIONS=()

add_region() {
  local region="$1"
  local existing
  for existing in "${REGIONS[@]}"; do
    if [ "$existing" = "$region" ]; then
      return
    fi
  done
  REGIONS+=("$region")
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    -a) add_region a ;;
    -b) add_region b ;;
    -c) add_region c ;;
    --region)
      shift
      add_region "${1:?--region requires a value}"
      ;;
    --regions)
      shift
      for region in ${1:?--regions requires a value}; do
        add_region "$region"
      done
      ;;
    -h|--help)
      echo "usage: uninstall-local-regions.sh [-a] [-b] [-c] [--region NAME ...]"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 1
      ;;
  esac
  shift
done

if [ "${#REGIONS[@]}" -eq 0 ]; then
  REGIONS=(a b c)
fi

for region in "${REGIONS[@]}"; do
  kubectl delete volumesnapshotclass "zcblock-${region}" --ignore-not-found=true
  kubectl delete storageclass "zcfile-${region}" "zcbrd-${region}" "zcraw-${region}" --ignore-not-found=true
  kubectl delete csidriver "io.zcutils.zcblock.${region}" --ignore-not-found=true
  kubectl delete clusterrolebinding "zcblock-csi-${region}-provisioner" --ignore-not-found=true
  kubectl delete clusterrole "zcblock-csi-${region}-provisioner" --ignore-not-found=true
  kubectl delete namespace "zcblock-csi-${region}" --ignore-not-found=true
done
