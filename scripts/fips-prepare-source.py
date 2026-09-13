#!/usr/bin/env python3
"""Restore the complete, hash-pinned upstream source inside an isolated builder.

The published sys crate trims tests and rewrites go.mod. This preparation
restores that subtree byte-for-byte from the Security Policy archive, without
changing the Rust wrapper. It does not establish that the wrapper's CMake flags
satisfy the approved build procedure: that remains a separate acceptance gate.
Never run this against the developer's shared Cargo registry.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import urllib.request
import zipfile

URL = 'https://github.com/aws/aws-lc/archive/refs/tags/AWS-LC-FIPS-3.1.0.zip'
SHA256 = 'fe408fa438850786396faf79eba9ea4116c3802e60f3a95865f0dd2adb64c9f1'


def prepare(cargo_home, archive):
    home = Path(cargo_home).resolve()
    if home != Path('/usr/local/cargo') or os.environ.get('CARGO_HOME') != str(home):
        raise ValueError('source preparation requires the isolated image builder CARGO_HOME=/usr/local/cargo')
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--offline', '--locked', '--features', 'fips', '--format-version', '1']))
    packages = [p for p in metadata['packages'] if p['name'] == 'aws-lc-fips-sys']
    if len(packages) != 1 or packages[0]['version'] != '0.13.11':
        raise ValueError('expected the pinned aws-lc-fips-sys 0.13.11 wrapper')
    target = Path(packages[0]['manifest_path']).resolve().parent / 'aws-lc'
    target.relative_to(home / 'registry/src')
    if hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
        raise ValueError('validated source archive SHA-256 mismatch')
    with tempfile.TemporaryDirectory(dir=target.parent) as temp:
        staging = Path(temp)
        with zipfile.ZipFile(archive) as source:
            roots = {i.filename.split('/')[0] for i in source.infolist()}
            if len(roots) != 1:
                raise ValueError('invalid archive root')
            for item in source.infolist():
                if item.is_dir():
                    continue
                relative = Path(item.filename.split('/', 1)[1])
                if relative.is_absolute() or '..' in relative.parts or (item.external_attr >> 16) & 0o170000 == 0o120000:
                    raise ValueError('unsafe archive entry')
                dest = staging / relative
                dest.parent.mkdir(parents=True, exist_ok=True)
                with dest.open('xb') as output:
                    output.write(source.read(item))
                if (item.external_attr >> 16) & 0o111:
                    dest.chmod(0o755)
        shutil.rmtree(target)
        shutil.copytree(staging, target)
    print(json.dumps({'module': 'AWS-LC FIPS 3.1.0', 'archive_sha256': SHA256, 'source_files': sum(p.is_file() for p in target.rglob('*'))}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cargo-home', required=True)
    parser.add_argument('--archive', type=Path)
    args = parser.parse_args()
    if args.archive:
        prepare(args.cargo_home, args.archive)
    else:
        with tempfile.TemporaryDirectory() as temp:
            archive = Path(temp) / 'source.zip'
            with urllib.request.urlopen(URL, timeout=120) as response, archive.open('wb') as output:
                shutil.copyfileobj(response, output)
            prepare(args.cargo_home, archive)


if __name__ == '__main__':
    main()
