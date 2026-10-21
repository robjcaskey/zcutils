#!/usr/bin/env python3
"""Generate Apple iig output on a Mac, or verify the pinned cache on any host."""
import argparse
import hashlib
import json
import os
import pathlib
import platform
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
CLASSES = ('ZcController', 'ZcUserClient')
DEFAULT_CACHE = ROOT / 'macos/driverkit/generated'


def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()


def sdk_identity(sdk):
    sdk = sdk.resolve(strict=True)
    if not (sdk / 'SDKSettings.json').is_file():
        raise ValueError('expected a complete DriverKit SDK with SDKSettings.json')
    entries = []
    for p in sorted(sdk.rglob('*')):
        rel = p.relative_to(sdk).as_posix()
        if p.is_symlink():
            entries.append([rel, 'symlink', os.readlink(p)])
        elif p.is_file():
            entries.append([rel, 'file', sha(p)])
    digest = hashlib.sha256(json.dumps(entries, separators=(',', ':')).encode()).hexdigest()
    return {'tree_sha256': digest, 'entry_count': len(entries),
            'settings_sha256': sha(sdk / 'SDKSettings.json')}


def sources():
    return {'macos/driverkit/' + c + '.iig': sha(ROOT / 'macos/driverkit' / (c + '.iig'))
            for c in CLASSES}


def recipe(arch, deployment):
    return {'schema': 1, 'arch': arch, 'deployment_target': deployment,
            'target_triple': arch + '-apple-driverkit' + deployment,
            'language': 'c++', 'standard': 'c++17',
            'defines': ['__IIG=1', 'PLATFORM_DriverKit=1'], 'classes': list(CLASSES)}


def command(arch, deployment, sdk, cls, destination, iig):
    return [iig, '--def', 'macos/driverkit/' + cls + '.iig',
            '--header', str(destination / (cls + '.h')),
            '--impl', str(destination / (cls + '.cpp')),
            '--deployment-target', deployment, '--log', str(destination / (cls + '.log')),
            '--', '-target', recipe(arch, deployment)['target_triple'],
            '-isysroot', str(sdk), '-iframework', str(sdk / 'System/DriverKit/System/Library/Frameworks'),
            '-isystem', str(sdk / 'System/DriverKit/usr/include'), '-x', 'c++', '-std=c++17',
            '-D__IIG=1', '-DPLATFORM_DriverKit=1', '-I', 'macos/driverkit']


def verify(cache, sdk, arch, deployment):
    directory = cache / arch
    manifest = json.loads((directory / 'manifest.json').read_text())
    if manifest.get('recipe') != recipe(arch, deployment):
        raise ValueError('stale iig cache: generation recipe/architecture/deployment changed')
    if manifest.get('inputs') != sources():
        raise ValueError('stale iig cache: .iig interfaces changed; regenerate on a Mac')
    if manifest.get('sdk') != sdk_identity(sdk):
        raise ValueError('stale iig cache: SDK contents differ; use the pinned SDK or regenerate')
    expected = {c + suffix for c in CLASSES for suffix in ('.h', '.cpp')}
    if set(manifest.get('outputs', {})) != expected:
        raise ValueError('invalid iig cache: generated-file coverage differs')
    for name, digest in manifest['outputs'].items():
        if sha(directory / name) != digest:
            raise ValueError('corrupt iig cache: output changed: ' + name)
    generator = manifest.get('generator', {})
    if not generator.get('sha256') or not generator.get('xcode_version'):
        raise ValueError('invalid iig cache: Apple generator provenance is missing')
    return manifest


def generate(cache, sdk, arch, deployment):
    if platform.system() != 'Darwin':
        raise ValueError('generation requires Apple iig on macOS; verification works on Linux')
    iig = subprocess.check_output(['xcrun', '--find', 'iig'], text=True).strip()
    cache.mkdir(parents=True, exist_ok=True)
    destination = cache / arch
    if destination.exists():
        raise ValueError('output cache already exists; generate into a new directory')
    with tempfile.TemporaryDirectory(prefix='iig-', dir=cache) as temp:
        temp = pathlib.Path(temp)
        for cls in CLASSES:
            subprocess.run(command(arch, deployment, sdk, cls, temp, iig), cwd=ROOT, check=True)
        version = subprocess.run([iig, '--version'], capture_output=True, text=True)
        manifest = {'recipe': recipe(arch, deployment), 'inputs': sources(), 'sdk': sdk_identity(sdk),
                    'generator': {'sha256': sha(pathlib.Path(iig)),
                                  'version_output': (version.stdout + version.stderr).strip(),
                                  'version_returncode': version.returncode,
                                  'xcode_version': subprocess.check_output(['xcodebuild', '-version'], text=True).strip()},
                    'outputs': {c + suffix: sha(temp / (c + suffix)) for c in CLASSES for suffix in ('.h', '.cpp')}}
        # Logs and temporary absolute paths are build evidence, not cached source.
        for p in temp.glob('*.log'):
            p.unlink()
        (temp / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
        temp.rename(destination)
    verify(cache, sdk, arch, deployment)
    return manifest


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('action', choices=['generate', 'verify'])
    p.add_argument('--sdk', required=True, type=pathlib.Path)
    p.add_argument('--cache', type=pathlib.Path, default=DEFAULT_CACHE)
    p.add_argument('--arch', choices=['arm64', 'x86_64'], required=True)
    p.add_argument('--deployment-target', default='23.0')
    a = p.parse_args()
    if not all(c.isdigit() or c == '.' for c in a.deployment_target):
        p.error('invalid deployment target')
    method = generate if a.action == 'generate' else verify
    data = method(a.cache.resolve(), a.sdk.resolve(), a.arch, a.deployment_target)
    print(json.dumps({'verified': True, 'arch': a.arch, 'sdk_sha256': data['sdk']['tree_sha256'],
                      'generator_sha256': data['generator']['sha256']}))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print('driverkit-interfaces: ' + str(error), file=sys.stderr)
        sys.exit(1)
