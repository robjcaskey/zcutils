#!/usr/bin/env python3
"""Compare two native module builds and install only the offline provider.

An unprivileged builder uses a user namespace to create its network namespace.
Both builds use the same absolute working path sequentially, with no native
compiler-output cache. Network isolation does not change the prescribed CMake
or make commands.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

SPEC = importlib.util.spec_from_file_location('repro', Path(__file__).with_name('fips-build-reproducibility.py'))
repro = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(repro)


def compare_native(online_report, offline_report, online_root, offline_root, output):
    Path(output).unlink(missing_ok=True)
    online = json.loads(Path(online_report).read_text())
    offline = json.loads(Path(offline_report).read_text())
    interfaces = offline.get('network_interfaces')
    if not isinstance(interfaces, list) or any(x != 'lo' for x in interfaces):
        raise ValueError('native offline network isolation was not observed')
    if not any(x != 'lo' for x in online.get('network_interfaces', [])):
        raise ValueError('native reference did not have network access')
    for key in ('source', 'tools', 'commands', 'certificate_profile_environment'):
        if online[key] != offline[key]:
            raise ValueError('native build input mismatch: ' + key)
    hashes = {}
    mismatches = []
    for name in ('bcm.o', 'libcrypto.a', 'bssl', 'identity_probe'):
        pair = {}
        for mode, report, root in (('online', online, online_root), ('offline', offline, offline_root)):
            entry = report['artifacts'][name]
            path = Path(root) / entry['path']
            if not path.resolve().is_relative_to(Path(root).resolve()):
                raise ValueError('native artifact outside build root')
            value = repro.sha256(path)
            if value != entry['sha256']:
                raise ValueError('native artifact changed after build: ' + name)
            pair[mode] = value
        if pair['online'] != pair['offline']:
            mismatches.append(name)
        hashes[name] = pair
    if mismatches:
        failure = {'status': 'different', 'artifacts': hashes, 'mismatches': mismatches}
        repro.write_json(Path(output).with_suffix('.failure.json'), failure)
        print(json.dumps(failure, indent=2), file=sys.stderr, flush=True)
        raise ValueError('native online/offline mismatch: ' + ', '.join(mismatches))
    repro.write_json(output, {'schema': 1, 'status': 'identical', 'selected_artifacts': 'offline',
                              'scope': 'native module, archive, tool and identity probe', 'artifacts': hashes,
                              'online_report_sha256': repro.sha256(online_report),
                              'offline_report_sha256': repro.sha256(offline_report)})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--archive', required=True)
    parser.add_argument('--work-dir', required=True)
    parser.add_argument('--provider-dir', required=True)
    parser.add_argument('--report', required=True)
    parser.add_argument('--compare-online', action='store_true', help='explicit experiment; disabled in normal offline builds')
    parser.add_argument('--allow-untested-environment', action='store_true')
    args = parser.parse_args()
    root = Path(args.work_dir).resolve()
    provider = Path(args.provider_dir).resolve()
    if root.exists() or provider.exists():
        raise ValueError('work and provider directories must not already exist')
    root.mkdir(parents=True)
    current = root / 'current'
    script = Path(__file__).with_name('fips-recompile-aws-lc.py').resolve()
    for mode in (('online', 'offline') if args.compare_online else ('offline',)):
        command = [sys.executable, str(script), '--archive', str(Path(args.archive).resolve()),
                   '--work-dir', str(current), '--report', str(root / (mode + '.json'))]
        if args.allow_untested_environment:
            command += ['--allow-untested-environment']
        if mode == 'offline':
            command += ['--require-offline', '--provider-dir', str(root / 'offline-provider')]
            isolation = ['unshare', '--net'] if os.geteuid() == 0 else ['unshare', '--user', '--map-root-user', '--net']
            command = isolation + ['--'] + command
        # Source downloads may be shared, but each trial gets an empty Go compiler cache.
        go_cache = root / 'current-go-cache'
        env = dict(os.environ, GOCACHE=str(go_cache), CCACHE_DISABLE='1')
        if mode == 'offline':
            env['GOPROXY'] = 'off'
        subprocess.run(command, check=True, env=env)
        if go_cache.exists():
            shutil.rmtree(go_cache)
        current.rename(root / mode)
    if args.compare_online:
        compare_native(root / 'online.json', root / 'offline.json', root / 'online', root / 'offline',
                       root / 'provider-reproducibility.json')
    # Promotion is strictly after successful comparison.
    provider.parent.mkdir(parents=True, exist_ok=True)
    shutil.move(str(root / 'offline-provider'), str(provider))
    if args.compare_online:
        shutil.copy2(root / 'provider-reproducibility.json', provider / 'share/zcutils/fips/provider-reproducibility.json')
    Path(args.report).parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(root / 'offline.json', args.report)


if __name__ == '__main__':
    main()
