#!/usr/bin/env python3
"""Create and compare deterministic executable payloads; keep signatures external."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import time


def effective_timestamp(value=None):
    value = value if value is not None else os.environ.get('SOURCE_DATE_EPOCH')
    if value is None:
        return int(time.time())
    text = str(value)
    if not text.isdigit() or not 0 <= int(text) <= 253402300799:
        raise ValueError('effective-build-timestamp must be Unix seconds in UTC')
    return int(text)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def create(binaries, output, timestamp, inputs=None):
    root = Path(binaries)
    files = {}
    if root.is_symlink() or not root.is_dir():
        raise ValueError('binaries must be a directory')
    for path in sorted(root.iterdir()):
        if path.is_symlink() or not path.is_file():
            raise ValueError('payload accepts only regular executable files')
        files['bin/' + path.name] = path.read_bytes()
    if not files:
        raise ValueError('empty payload')
    manifest = {'schema': 1, 'scope': 'shipped executables; excludes OCI image and signature envelopes',
                'effective_build_timestamp': effective_timestamp(timestamp),
                'inputs': inputs or {}, 'files': {name: digest(data) for name, data in files.items()}}
    files['manifest.json'] = (json.dumps(manifest, sort_keys=True, indent=2) + '\n').encode()
    target = Path(output)
    target.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(target, 'w', format=tarfile.PAX_FORMAT) as archive:
        for name, data in sorted(files.items()):
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = 0o755 if name.startswith('bin/') else 0o644
            info.mtime = manifest['effective_build_timestamp']
            info.uid = info.gid = 0
            info.uname = info.gname = ''
            archive.addfile(info, io.BytesIO(data))
    validate(target)
    return manifest


def validate(payload):
    files = {}
    with tarfile.open(payload, 'r:') as archive:
        for member in archive:
            if (not member.isfile() or member.name in files or
                (member.name != 'manifest.json' and
                 (not member.name.startswith('bin/') or len(Path(member.name).parts) != 2
                  or '..' in Path(member.name).parts))):
                raise ValueError('invalid or duplicate payload member')
            files[member.name] = archive.extractfile(member).read()
    manifest = json.loads(files.pop('manifest.json'))
    if manifest.get('schema') != 1 or not files or manifest['files'] != {n: digest(b) for n, b in files.items()}:
        raise ValueError('payload contents differ from manifest')
    effective_timestamp(manifest['effective_build_timestamp'])
    return manifest


def compare(first, second):
    validate(first)
    validate(second)
    a, b = digest(Path(first).read_bytes()), digest(Path(second).read_bytes())
    if a != b:
        raise ValueError('payload bytes differ')
    return {'status': 'identical', 'payload_sha256': a, 'signature_comparison': 'excluded'}


def verify_signature(payload, bundle, key, cosign='cosign'):
    # Trust comes from the caller's key, never a key supplied inside the archive.
    subprocess.run([cosign, 'verify-blob', '--key', key, '--bundle', str(bundle), str(payload)], check=True)
    return validate(payload)


def sign(payload, bundle, key, cosign='cosign'):
    validate(payload)
    subprocess.run([cosign, 'sign-blob', '--yes', '--key', key, '--bundle', str(bundle), str(payload)], check=True)
    verify_signature(payload, bundle, key, cosign)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    build = sub.add_parser('create')
    build.add_argument('--binaries', required=True)
    build.add_argument('--output', required=True)
    build.add_argument('--effective-build-timestamp')
    build.add_argument('--inputs', type=Path, help='stable compiler input identity JSON')
    comparison = sub.add_parser('compare')
    comparison.add_argument('--first', required=True)
    comparison.add_argument('--second', required=True)
    for name in ('sign', 'verify'):
        cmd = sub.add_parser(name)
        cmd.add_argument('--payload', required=True)
        cmd.add_argument('--bundle', required=True)
        cmd.add_argument('--key', required=True, help='independently trusted key URI or public key file')
    args = parser.parse_args()
    if args.command == 'create':
        result = create(args.binaries, args.output, effective_timestamp(args.effective_build_timestamp),
                        json.loads(args.inputs.read_text()) if args.inputs else None)
    elif args.command == 'compare':
        result = compare(args.first, args.second)
    elif args.command == 'sign':
        sign(args.payload, args.bundle, args.key)
        result = {'signature_verified': True, 'payload_sha256': digest(Path(args.payload).read_bytes())}
    else:
        result = verify_signature(args.payload, args.bundle, args.key)
    print(json.dumps(result, sort_keys=True, indent=2))


if __name__ == '__main__':
    main()
