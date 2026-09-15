#!/usr/bin/env python3
"""Verify two downloaded FIPS builds, then emit a scoped badge and admission policy.

Launch records must come from authenticated GitHub run downloads. Their EC2
identities are operational records, not hardware attestations. The supplied
public key must be trusted independently of either artifact directory.
"""
import argparse
import importlib.util
import json
from pathlib import Path


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


compare = module('compare_builds', 'fips-compare-builds.py')
attest = module('image_attest', 'zc-image-attest.py')


def admission_policy(repository, public_key, bundle_hash):
    if not attest.REGISTRY_REF_RE.fullmatch(repository) or '@' in repository or '*' in repository:
        raise ValueError('supply one concrete image repository without a tag or digest')
    if attest.repository_without_tag(repository) != repository:
        raise ValueError('image repository must not include a tag')
    if not __import__('re').fullmatch('[0-9a-f]{64}', bundle_hash):
        raise ValueError('invalid unsigned executable bundle hash')
    # Exact cardinality also rejects conflicting/duplicate SBOM properties.
    policy = '''package sigstore
    default isCompliant = false
    isCompliant {
      matches := [a | a := input.predicate.annotations[_]; startswith(a.comment, "zcutils:unsigned-executable-bundle:sha256=")]
      count(matches) == 1
      matches[0].comment == "zcutils:unsigned-executable-bundle:sha256=%s"
    }
''' % bundle_hash
    return {
        'apiVersion': 'policy.sigstore.dev/v1beta1', 'kind': 'ClusterImagePolicy',
        'metadata': {'name': 'zcblock-csi-reproduced-executables'},
        'spec': {'images': [{'glob': repository + '@sha256:*'}],
                 'authorities': [{'key': {'data': public_key},
                                  'attestations': [{'name': 'reproduced-executable-bundle',
                                                    'predicateType': attest.SPDX_PREDICATE,
                                                    'policy': {'type': 'rego', 'data': policy}}]}]}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('first', 'second', 'first-launch', 'second-launch', 'trusted-public-key', 'output-dir'):
        parser.add_argument('--' + name, required=True, type=Path)
    parser.add_argument('--image-repository', required=True)
    parser.add_argument('--cosign', default='cosign')
    args = parser.parse_args()
    # An exclusive output directory prevents a failed attempt leaving stale badges.
    args.output_dir.mkdir(parents=True, exist_ok=False)
    key = args.trusted_public_key.resolve()
    for root in (args.first, args.second):
        attest.verify_directory(root / 'image-attestations', 'zcblock-csi-fips-aspiring',
                                cosign=args.cosign, cosign_verification_key=str(key),
                                require_signature=True)
    report = args.output_dir / 'independent-build-comparison.json'
    compare.compare_builds(args.first, args.second, args.first_launch, args.second_launch, report)
    value = json.loads(report.read_text())
    digest = value['unsigned_executable_bundle_sha256']
    policy = admission_policy(args.image_repository, key.read_text(), digest)
    (args.output_dir / 'cluster-image-policy.json').write_text(json.dumps(policy, indent=2) + '\n')
    # Shields endpoint format; badge is scoped to the immutable comparison report.
    badge = {'schemaVersion': 1, 'label': 'executable bundle',
             'message': 'reproduced on 2 workers', 'color': 'brightgreen'}
    (args.output_dir / 'reproducibility-badge.json').write_text(json.dumps(badge, indent=2) + '\n')
    print(json.dumps({'unsigned_executable_bundle_sha256': digest,
                      'report': str(report), 'scope': 'unsigned executable bundle; excludes OCI filesystem and signatures'}))


if __name__ == '__main__':
    main()
