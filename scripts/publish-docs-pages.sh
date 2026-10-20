#!/usr/bin/env bash
# Publish only the allowlisted static files; retain the Helm repository assets.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
site_dir="${1:-${repo_root}/docs-site/dist}"
files=(index.html universe.html saml.html day-2.html federation.html zccusan/index.html zccusan/kubernetes.html zccusan/standalone.html zccusan/day-2.html site.css)
for file in "${files[@]}"; do
  [[ -f "${site_dir}/${file}" && ! -L "${site_dir}/${file}" ]] || {
    printf 'missing regular site file: %s\n' "${file}" >&2
    exit 1
  }
done
publish_root="$(mktemp -d "${TMPDIR:-/tmp}/zc-docs-pages.XXXXXXXX")"
cleanup() {
  if [[ -d "${publish_root}/tree" ]]; then
    git -C "${repo_root}" worktree remove --force "${publish_root}/tree"
  fi
  rm -rf -- "${publish_root}"
}
trap cleanup EXIT
git -C "${repo_root}" fetch origin gh-pages
git -C "${repo_root}" worktree add --detach "${publish_root}/tree" refs/remotes/origin/gh-pages
published=false
for attempt in 1 2 3; do
  # Refresh the isolated checkout if a chart promotion won the push race.
  git -C "${repo_root}" fetch origin gh-pages
  git -C "${publish_root}/tree" reset --hard refs/remotes/origin/gh-pages
  for file in "${files[@]}"; do
    mkdir -p "$(dirname "${publish_root}/tree/${file}")"
    cp "${site_dir}/${file}" "${publish_root}/tree/${file}"
  done
  touch "${publish_root}/tree/.nojekyll"
  git -C "${publish_root}/tree" add -- "${files[@]}" .nojekyll
  if git -C "${publish_root}/tree" diff --cached --quiet; then
    published=true
    break
  fi
  git -C "${publish_root}/tree" -c user.name='github-actions[bot]' \
    -c user.email='41898282+github-actions[bot]@users.noreply.github.com' \
    commit -m 'docs: publish generated identity guides'
  if git -C "${publish_root}/tree" push origin HEAD:refs/heads/gh-pages; then
    published=true
    break
  fi
  printf 'Pages branch changed or push failed; retry %s/3\n' "${attempt}" >&2
done
[[ "${published}" == true ]] || { printf 'Pages push failed\n' >&2; exit 1; }

# GITHUB_TOKEN pushes do not trigger the legacy Pages build automatically.
# Request it explicitly with the workflow's pages:write permission.
gh api --method POST "repos/${GITHUB_REPOSITORY:-robjcaskey/zcutils}/pages/builds" \
  --jq '{status,url}'
printf 'PAGES_PUBLISH_REQUESTED https://robjcaskey.github.io/zcutils/\n'
