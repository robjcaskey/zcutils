#!/usr/bin/env bash
# Publish exactly the generated manifest; preserve repository configuration.
set -euo pipefail
[[ $# == 2 ]] || { printf 'usage: %s SITE_DIR DESTINATION_CHECKOUT\n' "$0" >&2; exit 2; }
site_dir="$(cd "$1" && pwd)"
target_dir="$(cd "$2" && pwd)"
[[ "$(git -C "${target_dir}" rev-parse --show-toplevel)" == "${target_dir}" ]] || exit 1
python3 - "${site_dir}" "${target_dir}" <<'PY'
from pathlib import Path, PurePosixPath
import shutil, subprocess, sys
source, target = map(Path, sys.argv[1:])
def safe(name):
    path = PurePosixPath(name)
    if not name or str(path) != name or path.is_absolute() or any(p in ('.', '..') or p.startswith('.') for p in path.parts):
        raise SystemExit(f'unsafe site manifest path: {name!r}')
    if any(c in name for c in '\\\r\n\t') or name in ('CNAME', 'README.md', 'site-manifest.txt'):
        raise SystemExit(f'protected site manifest path: {name!r}')
    if not (path.suffix == '.html' or name == 'site.css' or name.startswith('assets/repository/')):
        raise SystemExit(f'non-site manifest path: {name!r}')
    return name
def regular(root, name, required=True):
    path = root
    for part in PurePosixPath(name).parts:
        path /= part
        if path.is_symlink(): raise SystemExit(f'symlink site path: {path}')
    if path.exists() and not path.is_file(): raise SystemExit(f'non-file site path: {path}')
    if required and not path.is_file(): raise SystemExit(f'missing site file: {path}')
    return path
def manifest(root, required):
    path = regular(root, 'site-manifest.txt', required)
    if not path.exists(): return set()
    entries = path.read_text().splitlines()
    names = {safe(name) for name in entries}
    if len(names) != len(entries): raise SystemExit('duplicate manifest path')
    return names
new, old = manifest(source, True), manifest(target, False)
required = {'index.html', 'kubernetes.html', 'standalone.html', 'day-2.html', 'universe.html', 'site.css'}
if not required <= new: raise SystemExit('site manifest is missing required pages')
# Validate everything before touching the destination.
for name in new: regular(source, name)
for name in new | old: regular(target, name, False)
for name in ['CNAME', '.nojekyll']: regular(target, name, False)
for name in old - new:
    path = target / name
    if path.exists(): path.unlink()
for name in new:
    path = target / name
    path.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source / name, path)
shutil.copyfile(source / 'site-manifest.txt', target / 'site-manifest.txt')
(target / 'CNAME').write_text('diskpipe.com\n')
(target / '.nojekyll').touch()
subprocess.run(['git', '-C', str(target), 'add', '-A', '--', *sorted(new | old), 'site-manifest.txt', 'CNAME', '.nojekyll'], check=True)
print(f'DISKPIPE_FILES published={len(new)} removed={len(old-new)}')
PY
if ! git -C "${target_dir}" diff --cached --quiet; then
  git -C "${target_dir}" -c user.name='github-actions[bot]' \
    -c user.email='41898282+github-actions[bot]@users.noreply.github.com' \
    commit -m 'Publish validated Diskpipe documentation'
  git -C "${target_dir}" push origin HEAD:refs/heads/gh-pages
fi
printf 'DISKPIPE_PUBLISHED https://diskpipe.com/\n'
