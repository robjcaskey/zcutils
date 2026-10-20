#!/usr/bin/env bash
# Exercise publication against an isolated local remote, never GitHub.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/zc-docs-pages-test.XXXXXXXX")"
trap 'rm -rf -- "${test_root}"' EXIT
git init --bare --quiet "${test_root}/remote.git"
git init --quiet --initial-branch=gh-pages "${test_root}/seed"
printf 'existing chart index\n' > "${test_root}/seed/index.yaml"
printf 'existing archive\n' > "${test_root}/seed/chart.tgz"
printf 'existing signature\n' > "${test_root}/seed/chart.tgz.sig"
printf 'existing other asset\n' > "${test_root}/seed/keep.txt"
git -C "${test_root}/seed" add .
git -C "${test_root}/seed" -c user.name=fixture -c user.email=fixture@example.invalid commit --quiet -m seed
git -C "${test_root}/seed" remote add origin "${test_root}/remote.git"
git -C "${test_root}/seed" push --quiet origin gh-pages
git clone --quiet --branch gh-pages "${test_root}/remote.git" "${test_root}/source"
mkdir -p "${test_root}/source/scripts" "${test_root}/source/docs-site/dist" "${test_root}/bin"
cp "${repo_root}/scripts/publish-docs-pages.sh" "${test_root}/source/scripts/"
for file in index.html universe.html saml.html day-2.html federation.html zccusan/index.html zccusan/kubernetes.html zccusan/standalone.html zccusan/day-2.html site.css; do
  mkdir -p "$(dirname "${test_root}/source/docs-site/dist/${file}")"
  printf 'generated %s\n' "${file}" > "${test_root}/source/docs-site/dist/${file}"
done
cat > "${test_root}/bin/gh" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${GH_TEST_CALL_LOG}"
SH
chmod +x "${test_root}/bin/gh"
export PATH="${test_root}/bin:${PATH}"
export GH_TEST_CALL_LOG="${test_root}/gh-calls"
export GITHUB_REPOSITORY=fixture/docs
bash "${test_root}/source/scripts/publish-docs-pages.sh" > "${test_root}/publish.log" 2>&1
first="$(git --git-dir="${test_root}/remote.git" rev-parse gh-pages)"
for file in index.yaml chart.tgz chart.tgz.sig keep.txt; do
  cmp "${test_root}/seed/${file}" <(git --git-dir="${test_root}/remote.git" show "gh-pages:${file}")
done
for file in index.html universe.html saml.html day-2.html federation.html zccusan/index.html zccusan/kubernetes.html zccusan/standalone.html zccusan/day-2.html site.css; do
  cmp "${test_root}/source/docs-site/dist/${file}" <(git --git-dir="${test_root}/remote.git" show "gh-pages:${file}")
done
git --git-dir="${test_root}/remote.git" cat-file -e gh-pages:.nojekyll
bash "${test_root}/source/scripts/publish-docs-pages.sh" >> "${test_root}/publish.log" 2>&1
[[ "${first}" == "$(git --git-dir="${test_root}/remote.git" rev-parse gh-pages)" ]]
[[ "$(wc -l < "${GH_TEST_CALL_LOG}")" == 2 ]]
grep -q 'repos/fixture/docs/pages/builds' "${GH_TEST_CALL_LOG}"
rm "${test_root}/source/docs-site/dist/site.css"
ln -s "${test_root}/seed/keep.txt" "${test_root}/source/docs-site/dist/site.css"
if bash "${test_root}/source/scripts/publish-docs-pages.sh" >> "${test_root}/publish.log" 2>&1; then
  printf 'publisher accepted a symbolic-link asset\n' >&2
  exit 1
fi
[[ "${first}" == "$(git --git-dir="${test_root}/remote.git" rev-parse gh-pages)" ]]
printf 'PAGES_PUBLISHER_TEST_PASS assets_preserved=true idempotent=true symlinks_rejected=true\n'
