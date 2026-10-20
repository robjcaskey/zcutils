#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/diskpipe-publish-test.XXXXXXXX")"
trap 'rm -rf -- "${test_root}"' EXIT
git init --bare --quiet "${test_root}/remote.git"
git init --quiet --initial-branch=gh-pages "${test_root}/target"
printf 'deployment repository\n' > "${test_root}/target/README.md"
git -C "${test_root}/target" add README.md
git -C "${test_root}/target" -c user.name=fixture -c user.email=fixture@example.invalid commit --quiet -m seed
git -C "${test_root}/target" remote add origin "${test_root}/remote.git"
git -C "${test_root}/target" push --quiet origin gh-pages
mkdir -p "${test_root}/site/docs" "${test_root}/site/assets/repository/.github/workflows"
printf 'public workflow example\n' > "${test_root}/site/assets/repository/.github/workflows/example.yml"
printf 'nested guide\n' > "${test_root}/site/docs/exercise.html"
for file in index.html kubernetes.html standalone.html day-2.html universe.html site.css; do
  printf 'generated %s\n' "${file}" > "${test_root}/site/${file}"
done
printf '%s\n' index.html kubernetes.html standalone.html day-2.html universe.html site.css docs/exercise.html assets/repository/.github/workflows/example.yml > "${test_root}/site/site-manifest.txt"
bash "${repo_root}/scripts/publish-diskpipe-docs.sh" "${test_root}/site" "${test_root}/target" > "${test_root}/publish.log" 2>&1
first="$(git --git-dir="${test_root}/remote.git" rev-parse gh-pages)"
[[ "$(git --git-dir="${test_root}/remote.git" show gh-pages:assets/repository/.github/workflows/example.yml)" == 'public workflow example' ]]
for file in index.html kubernetes.html standalone.html day-2.html universe.html site.css; do
  cmp "${test_root}/site/${file}" <(git --git-dir="${test_root}/remote.git" show "gh-pages:${file}")
done
[[ "$(git --git-dir="${test_root}/remote.git" show gh-pages:CNAME)" == diskpipe.com ]]
[[ "$(git --git-dir="${test_root}/remote.git" show gh-pages:README.md)" == 'deployment repository' ]]
bash "${repo_root}/scripts/publish-diskpipe-docs.sh" "${test_root}/site" "${test_root}/target" >> "${test_root}/publish.log" 2>&1
[[ "${first}" == "$(git --git-dir="${test_root}/remote.git" rev-parse gh-pages)" ]]
# Removing generated pages preserves repository configuration.
printf 'workflow configuration\n' > "${test_root}/target/keep.yml"
git -C "${test_root}/target" add keep.yml
git -C "${test_root}/target" -c user.name=fixture -c user.email=fixture@example.invalid commit --quiet -m config
rm "${test_root}/site/docs/exercise.html"
sed -i '/docs\/exercise.html/d' "${test_root}/site/site-manifest.txt"
bash "${repo_root}/scripts/publish-diskpipe-docs.sh" "${test_root}/site" "${test_root}/target" >> "${test_root}/publish.log" 2>&1
! git --git-dir="${test_root}/remote.git" cat-file -e gh-pages:docs/exercise.html 2>/dev/null
[[ "$(git --git-dir="${test_root}/remote.git" show gh-pages:keep.yml)" == 'workflow configuration' ]]
first="$(git --git-dir="${test_root}/remote.git" rev-parse gh-pages)"
printf '../escape.html\n' >> "${test_root}/site/site-manifest.txt"
if bash "${repo_root}/scripts/publish-diskpipe-docs.sh" "${test_root}/site" "${test_root}/target" >> "${test_root}/publish.log" 2>&1; then
  printf 'accepted manifest traversal\n' >&2; exit 1
fi
sed -i '\|../escape.html|d' "${test_root}/site/site-manifest.txt"
rm "${test_root}/site/site.css"
ln -s "${test_root}/target/README.md" "${test_root}/site/site.css"
if bash "${repo_root}/scripts/publish-diskpipe-docs.sh" "${test_root}/site" "${test_root}/target" >> "${test_root}/publish.log" 2>&1; then
  printf 'accepted symlink source\n' >&2; exit 1
fi
[[ "${first}" == "$(git --git-dir="${test_root}/remote.git" rev-parse gh-pages)" ]]
printf 'DISKPIPE_PUBLISH_TEST_PASS custom_domain=true idempotent=true symlinks_rejected=true nested_pages=true stale_pages_removed=true traversal_rejected=true\n'
