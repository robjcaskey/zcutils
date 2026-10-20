#!/usr/bin/env bash
set -euo pipefail

# Public DNS and normal certificate verification are intentional.
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/diskpipe-https-check.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT
for hostname in diskpipe.com www.diskpipe.com; do
  for path in / /site.css /docs/GETTING_STARTED_WITH_ZCCUSAN_ON_KUBERNETES.html; do
    status=$(curl --fail --show-error --silent --connect-timeout 15 --max-time 45 \
      --output "$tmp_dir/body" --write-out '%{http_code}' "https://$hostname$path")
    [[ "$status" == 200 ]] || { echo "Unexpected HTTPS status $status: $hostname$path" >&2; exit 1; }
    [[ -s "$tmp_dir/body" ]] || { echo "Empty response: $hostname$path" >&2; exit 1; }
    printf 'Verified HTTPS 200: https://%s%s\n' "$hostname" "$path"
  done
  curl --show-error --silent --connect-timeout 15 --max-time 45 \
    --dump-header "$tmp_dir/headers" --output /dev/null "http://$hostname/"
  tr -d '\r' < "$tmp_dir/headers" > "$tmp_dir/normalized"
  if ! grep -Eq '^HTTP/[^ ]+ 30[1278] ' "$tmp_dir/normalized" || \
     ! grep -Eiq "^location: https://$hostname/$" "$tmp_dir/normalized"; then
    echo "HTTP did not redirect to HTTPS: $hostname" >&2
    exit 1
  fi
  printf 'Verified HTTP redirect: http://%s/\n' "$hostname"
done
