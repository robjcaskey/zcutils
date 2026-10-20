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
mkdir "${test_root}/site"
for file in index.html kubernetes.html standalone.html day-2.html universe.html site.css; do
  printf 'generated %s\n' "${file}" > "${test_root}/site/${file}"
done
bash "${repo_root}/scripts/publish-diskpipe-docs.sh" "${test_root}/site" "${test_root}/target" > "${test_root}/publish.log" 2>&1
first="$(git --git-dir="${test_root}/remote.git" rev-parse gh-pages)"
for file in index.html kubernetes.html standalone.html day-2.html universe.html site.css; do
  cmp "${test_root}/site/${file}" <(git --git-dir="${test_root}/remote.git" show "gh-pages:${file}")
done
[[ "$(git --git-dir="${test_root}/remote.git" show gh-pages:CNAME)" == diskpipe.com ]]
[[ "$(git --git-dir="${test_root}/remote.git" show gh-pages:README.md)" == 'deployment repository' ]]
bash "${repo_root}/scripts/publish-diskpipe-docs.sh" "${test_root}/site" "${test_root}/target" >> "${test_root}/publish.log" 2>&1
[[ "${first}" == "$(git --git-dir="${test_root}/remote.git" rev-parse gh-pages)" ]]
rm "${test_root}/site/site.css"
ln -s "${test_root}/target/README.md" "${test_root}/site/site.css"
if bash "${repo_root}/scripts/publish-diskpipe-docs.sh" "${test_root}/site" "${test_root}/target" >> "${test_root}/publish.log" 2>&1; then
  printf 'accepted symlink source\n' >&2; exit 1
fi
[[ "${first}" == "$(git --git-dir="${test_root}/remote.git" rev-parse gh-pages)" ]]
printf 'DISKPIPE_PUBLISH_TEST_PASS custom_domain=true idempotent=true symlinks_rejected=true\n'
