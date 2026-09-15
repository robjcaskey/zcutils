#!/usr/bin/env python3
"""Compare downloaded offline artifacts from two distinct EC2 build workers.

Run signature/artifact verification before supplying these directories. Launch
records identify the workers; this comparison does not authenticate those
records or extend a CMVP certificate. Per-run metadata is not compared as code.
"""
import argparse
import importlib.util
import json
from pathlib import Path

SPEC = importlib.util.spec_from_file_location('repro', Path(__file__).with_name('fips-build-reproducibility.py'))
repro = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(repro)
PAYLOAD_SPEC = importlib.util.spec_from_file_location('payload', Path(__file__).with_name('zc-release-payload.py'))
payload = importlib.util.module_from_spec(PAYLOAD_SPEC)
PAYLOAD_SPEC.loader.exec_module(payload)


def compare_builds(first, second, first_launch, second_launch, report):
    Path(report).unlink(missing_ok=True)
    launches = [json.loads(Path(p).read_text()) for p in (first_launch, second_launch)]
    ids = [value.get('instance_id') for value in launches]
    if not all(isinstance(x, str) and x.startswith('i-') for x in ids) or ids[0] == ids[1]:
        raise ValueError('two distinct EC2 worker identities are required')
    builds = []
    providers = []
    bundles = []
    for root in (Path(first), Path(second)):
        record = repro.verify(root / 'bin', root / 'evidence/offline-build.json')
        receipt = json.loads((root / 'provider/share/zcutils/fips/provider-receipt.json').read_text())
        interfaces = receipt.get('network_interfaces')
        if not isinstance(interfaces, list) or any(x != 'lo' for x in interfaces):
            raise ValueError('provider was not built offline')
        hashes = {name: repro.sha256(root / 'provider/lib' / name) for name in ('libcrypto.a', 'bcm.o')}
        for name, value in hashes.items():
            if receipt['artifacts'][name]['sha256'] != value:
                raise ValueError('provider differs from its build receipt')
        if record['provider_libcrypto_sha256'] != hashes['libcrypto.a']:
            raise ValueError('application used a different provider')
        builds.append(record)
        bundle = root / 'image-attestations/zcblock-csi-fips-aspiring.unsigned-executable-bundle.tar'
        bundled = payload.validate(bundle)
        if bundled['files'] != {'bin/' + name: value for name, value in record['artifacts'].items()}:
            raise ValueError('unsigned executable bundle differs from the offline artifacts')
        bundles.append(bundle)
        providers.append({'artifacts': hashes, 'source': receipt['source'],
                          'tools': receipt['tools'], 'commands': receipt['commands']})
    for key in ('inputs', 'cargo_lock_sha256', 'artifacts', 'provider_libcrypto_sha256'):
        if builds[0][key] != builds[1][key]:
            raise ValueError('independent application builds differ: ' + key)
    if providers[0] != providers[1]:
        raise ValueError('independent native providers or their inputs differ')
    bundle_comparison = payload.compare(*bundles)
    repro.write_json(report, {'schema': 1, 'status': 'identical', 'worker_instance_ids': ids,
                              'scope': 'all shipped application executables and native provider objects; not OCI metadata',
                              'application': builds[0]['artifacts'], 'provider': providers[0]['artifacts'],
                              'unsigned_executable_bundle_sha256': bundle_comparison['payload_sha256'],
                              'input_identity': builds[0]['inputs'],
                              'launch_record_sha256': [repro.sha256(p) for p in (first_launch, second_launch)]})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('first', 'second', 'first-launch', 'second-launch', 'report'):
        parser.add_argument('--' + name, required=True)
    args = parser.parse_args()
    compare_builds(args.first, args.second, args.first_launch, args.second_launch, args.report)


if __name__ == '__main__':
    main()
