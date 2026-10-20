#!/usr/bin/env bash
# The destination is a dedicated deployment-only GitHub Pages repository.
set -euo pipefail
[[ $# == 2 ]] || { printf 'usage: %s SITE_DIR DESTINATION_CHECKOUT\n' "$0" >&2; exit 2; }
site_dir="$(cd "$1" && pwd)"
target_dir="$(cd "$2" && pwd)"
files=(index.html kubernetes.html standalone.html day-2.html universe.html site.css)
for file in "${files[@]}"; do
  [[ -f "${site_dir}/${file}" && ! -L "${site_dir}/${file}" ]] || {
    printf 'missing regular Diskpipe site file: %s\n' "${file}" >&2; exit 1;
  }
done
[[ "$(git -C "${target_dir}" rev-parse --show-toplevel)" == "${target_dir}" ]] || exit 1
for file in "${files[@]}"; do
  cp "${site_dir}/${file}" "${target_dir}/${file}"
done
printf 'diskpipe.com\n' > "${target_dir}/CNAME"
touch "${target_dir}/.nojekyll"
git -C "${target_dir}" add -- "${files[@]}" CNAME .nojekyll
if ! git -C "${target_dir}" diff --cached --quiet; then
  git -C "${target_dir}" -c user.name='github-actions[bot]' \
    -c user.email='41898282+github-actions[bot]@users.noreply.github.com' \
    commit -m 'Publish validated Diskpipe documentation'
  git -C "${target_dir}" push origin HEAD:refs/heads/gh-pages
fi
printf 'DISKPIPE_PUBLISHED https://diskpipe.com/\n'
