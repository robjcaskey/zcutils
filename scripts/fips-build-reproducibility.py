#!/usr/bin/env python3
"""Build independent application outputs and reject any online/offline difference.

Inputs are prepared separately. Build records are observations, not a FIPS
validation. Only the offline payload may be packaged after comparison succeeds.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import time


BINARIES = (
    'zc-fips-check', 'zcutils', 'zcblock-csi', 'zcblock-node-setup',
    'zcblock-control', 'zcblock-freeze', 'zccusan-telemetry-server',
    'zccusan-operator', 'zcrepl', 'zcpit', 'zctier', 'zcnblk-fan',
    'zcnblk-shm-target', 'zcnblk-wal-failover', 'zcnblk-wal-leaf', 'zcnblk-order-smoke',
)


def sha256(path):
    with Path(path).open('rb') as stream:
        value = hashlib.sha256()
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def manifest(root):
    root = Path(root)
    if root.is_symlink() or not root.is_dir():
        raise ValueError('artifact directory is missing or a symlink')
    entries = {}
    for path in sorted(root.rglob('*')):
        if path.is_symlink():
            raise ValueError('symlink in artifact tree: ' + str(path))
        if path.is_file():
            entries[path.relative_to(root).as_posix()] = sha256(path)
        elif not path.is_dir():
            raise ValueError('special file in artifact tree: ' + str(path))
    if not entries:
        raise ValueError('artifact tree is empty')
    return entries


def write_json(path, value):
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def network_interfaces():
    return sorted(name for _, name in socket.if_nameindex())


def require_offline():
    interfaces = network_interfaces()
    if any(name != 'lo' for name in interfaces):
        raise ValueError('offline build has a non-loopback network interface: ' + repr(interfaces))
    return interfaces


def compilation_inputs():
    files = {name: sha256(name) for name in ('Cargo.toml', 'Cargo.lock', 'build.rs')}
    for directory in ('src', 'vendor/aws-lc-fips-sys-provider'):
        files.update({directory + '/' + name: value for name, value in manifest(directory).items()})
    return {
        'source_sha256': hashlib.sha256(json.dumps(files, sort_keys=True).encode()).hexdigest(),
        'compiler_recipe_sha256': sha256(__file__),
        'rustc': subprocess.check_output(['rustc', '-vV'], text=True),
        'cc': subprocess.check_output(['cc', '--version'], text=True),
    }


def build(mode, out, jobs):
    interfaces = require_offline() if mode == 'offline' else network_interfaces()
    if mode == 'online' and not any(name != 'lo' for name in interfaces):
        raise ValueError('online reference lacks a network interface')
    if (Path('target').exists() and any(Path('target').iterdir())) or Path(out).exists():
        raise ValueError('build requires empty target and output directories')
    env = dict(os.environ, RUST_MIN_STACK='33554432', CARGO_INCREMENTAL='0')
    epoch = env.get('SOURCE_DATE_EPOCH') or str(int(time.time()))
    if not epoch.isdigit():
        raise ValueError('SOURCE_DATE_EPOCH must be Unix seconds')
    env['SOURCE_DATE_EPOCH'] = epoch
    env.pop('CARGO_TARGET_DIR', None)
    env.pop('RUSTC_WRAPPER', None)
    env.pop('RUSTC_WORKSPACE_WRAPPER', None)
    if env.get('FIPS_DISTRO') == 'ubi9':
        env['ZCUTILS_DISABLE_LIBFABRIC'] = '1'
    inputs = compilation_inputs()
    command = ['cargo', 'build', '--frozen' if mode == 'offline' else '--locked',
               '--release', '--features', 'fips', '--jobs', str(jobs)]
    for name in BINARIES:
        command += ['--bin', name]
    subprocess.run(command, env=env, check=True)
    output = Path(out)
    (output / 'bin').mkdir(parents=True)
    for name in BINARIES:
        shutil.copy2(Path('target/release') / name, output / 'bin' / name)
    write_json(output / 'build.json', {
        'schema': 1, 'mode': mode, 'network_interfaces': interfaces,
        'clean_target': True, 'command': command, 'inputs': inputs,
        'effective_build_timestamp': int(epoch),
        'cargo_lock_sha256': sha256('Cargo.lock'),
        'provider_libcrypto_sha256': sha256(Path(env['AWS_LC_FIPS_SYS_SYSTEM_DIR']) / 'lib/libcrypto.a'),
        'artifacts': manifest(output / 'bin'),
    })


def compare(offline, online, report):
    Path(report).unlink(missing_ok=True)
    roots = {'offline': Path(offline), 'online': Path(online)}
    records = {}
    for mode, root in roots.items():
        record = json.loads((root / 'build.json').read_text())
        if record.get('mode') != mode or record.get('clean_target') is not True:
            raise ValueError('missing clean independent build record: ' + mode)
        interfaces = record.get('network_interfaces')
        if not isinstance(interfaces, list) or not all(isinstance(x, str) for x in interfaces):
            raise ValueError('missing network observation')
        if mode == 'offline' and any(x != 'lo' for x in interfaces):
            raise ValueError('offline build had networking')
        if mode == 'online' and not any(x != 'lo' for x in interfaces):
            raise ValueError('reference build was not online')
        actual = manifest(root / 'bin')
        if set(actual) != set(BINARIES):
            raise ValueError('missing or unexpected shipped executable')
        if actual != record.get('artifacts'):
            raise ValueError('build record does not match artifacts')
        records[mode] = record
    for key in ('artifacts', 'inputs', 'cargo_lock_sha256', 'provider_libcrypto_sha256'):
        if records['offline'][key] != records['online'][key]:
            raise ValueError('online/offline mismatch: ' + key)
    write_json(report, {'schema': 1, 'status': 'identical', 'selected_artifacts': 'offline',
                        'scope': 'all shipped application executables; same prepared inputs',
                        'builds': records})


def verify(binaries, report):
    record = json.loads(Path(report).read_text())
    if record.get('mode') != 'offline' or record.get('clean_target') is not True:
        raise ValueError('image lacks a clean offline build record')
    interfaces = record.get('network_interfaces')
    if not isinstance(interfaces, list) or any(x != 'lo' for x in interfaces):
        raise ValueError('image lacks offline network observation')
    actual = manifest(binaries)
    if set(actual) != set(BINARIES):
        raise ValueError('image executable set differs from the recorded set')
    if actual != record.get('artifacts'):
        raise ValueError('packaged executable differs from offline build')
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='action', required=True)
    b = commands.add_parser('build')
    b.add_argument('--mode', choices=('offline', 'online'), required=True)
    b.add_argument('--out', default='/out')
    b.add_argument('--jobs', type=int, default=4)
    c = commands.add_parser('compare')
    c.add_argument('--offline', required=True)
    c.add_argument('--online', required=True)
    c.add_argument('--report', required=True)
    v = commands.add_parser('verify')
    v.add_argument('--binaries', required=True)
    v.add_argument('--report', required=True)
    args = parser.parse_args()
    if args.action == 'build':
        build(args.mode, args.out, args.jobs)
    elif args.action == 'verify':
        verify(args.binaries, args.report)
    else:
        compare(args.offline, args.online, args.report)


if __name__ == '__main__':
    main()
