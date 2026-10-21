#!/usr/bin/env python3
"""Compile a development .dext on Linux/macOS using verified Apple iig output.

Supply a separately obtained, byte-identical DriverKit SDK. No generator,
activation, signing certificate, SDK download, or security-policy change occurs.
The Linux archive is a development extension, not an installable signed app.
"""
import argparse
import hashlib
import importlib.util
import json
import os
import pathlib
import platform
import plistlib
import shutil
import subprocess
import struct
import sys
import tarfile
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('driverkit_interfaces', ROOT / 'scripts/driverkit-interfaces.py')
iig = importlib.util.module_from_spec(spec)
spec.loader.exec_module(iig)



def image_properties(path, arch, deployment, sdk_version=None):
    """Reject ordinary executables and wrong-platform output before packaging."""
    data = path.read_bytes()
    if len(data) < 32:
        raise ValueError('truncated Mach-O')
    magic, cpu, subtype, kind, count, size, flags, reserved = struct.unpack_from('<8I', data)
    expected_cpu = 0x100000c if arch == 'arm64' else 0x1000007
    if magic != 0xfeedfacf or cpu != expected_cpu or kind != 2 or 32 + size > len(data):
        raise ValueError('not the requested 64-bit Mach-O executable')
    offset = 32
    platforms = []
    sections = []
    libraries = []
    for _ in range(count):
        cmd, length = struct.unpack_from('<II', data, offset)
        if length < 8 or offset + length > 32 + size:
            raise ValueError('invalid Mach-O load command')
        if cmd in (0x80000028, 5):
            raise ValueError('DriverKit executable must not have LC_MAIN/LC_UNIXTHREAD; stock LLD is incompatible')
        if cmd == 0x32:
            if length < 24:
                raise ValueError('truncated build version')
            platform_id, minimum, encoded_sdk = struct.unpack_from('<III', data, offset + 8)
            platforms.append([platform_id, minimum, encoded_sdk])
        if cmd == 0x19:
            if length < 72:
                raise ValueError('truncated segment command')
            nsections = struct.unpack_from('<I', data, offset + 64)[0]
            if 72 + nsections * 80 > length:
                raise ValueError('truncated section table')
            for index in range(nsections):
                start = offset + 72 + index * 80
                sections.append(data[start:start + 16].split(b'\0')[0].decode())
        if cmd == 0xc:
            if length < 24:
                raise ValueError('truncated dylib command')
            start = offset + struct.unpack_from('<I', data, offset + 8)[0]
            libraries.append(data[start:offset + length].split(b'\0')[0].decode())
        offset += length
    versions = list(map(int, deployment.split('.')))
    encoded = sum(v << shift for v, shift in zip(versions, [16, 8, 0]))
    if len(platforms) != 1 or platforms[0][0] != 10 or platforms[0][1] != encoded:
        raise ValueError('wrong DriverKit platform/deployment load command')
    if sdk_version is not None:
        expected = sum(v << shift for v, shift in zip(map(int, sdk_version.split('.')), [16, 8, 0]))
        if platforms[0][2] != expected:
            raise ValueError('wrong DriverKit SDK build version')
    if '__osclassinfo' not in sections or not libraries or any(not lib.startswith('/System/DriverKit/') for lib in libraries):
        raise ValueError('missing DriverKit class metadata or non-DriverKit library dependency')
    return {'cpu_type': cpu, 'file_type': kind, 'flags': flags, 'build_version': platforms[0],
            'class_metadata_section': True, 'normal_entry_point': False, 'libraries': libraries}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--sdk', type=pathlib.Path, required=True)
    p.add_argument('--cache', type=pathlib.Path, default=iig.DEFAULT_CACHE)
    p.add_argument('--arch', choices=['arm64', 'x86_64'], required=True)
    p.add_argument('--deployment-target', default='23.0')
    p.add_argument('--clang', default='clang++')
    p.add_argument('--linker', help='Linux requires the path to LLVM ld64.lld')
    p.add_argument('--out', type=pathlib.Path, required=True)
    a = p.parse_args()
    sdk = a.sdk.resolve(strict=True)
    cache = a.cache.resolve(strict=True)
    manifest = iig.verify(cache, sdk, a.arch, a.deployment_target)
    compiler = shutil.which(a.clang)
    if not compiler:
        p.error('clang++ was not found')
    if platform.system() == 'Linux' and (not a.linker or not pathlib.Path(a.linker).is_file()):
        p.error('Linux requires --linker /path/to/ld64.lld')
    if a.out.exists():
        p.error('--out must be a new directory')
    # Preflight above must finish before any compiler invocation or output creation.
    a.out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='driverkit-', dir=a.out.parent) as temp:
        temp = pathlib.Path(temp)
        bundle = temp / 'org.zcutils.storage.dext'
        binary = bundle / 'Contents/MacOS/org.zcutils.storage'
        binary.parent.mkdir(parents=True)
        argv = [compiler, '-std=gnu++17', '-target', manifest['recipe']['target_triple'],
                '-isysroot', str(sdk), '-iframework', str(sdk / 'System/DriverKit/System/Library/Frameworks'),
                '-isystem', str(sdk / 'System/DriverKit/usr/include'),
                '-I', str(cache / a.arch), '-I', str(ROOT / 'macos/driverkit'),
                '-fno-exceptions', '-fno-rtti', str(ROOT / 'macos/driverkit/ZcController.cpp'),
                str(cache / a.arch / 'ZcController.cpp'), str(cache / a.arch / 'ZcUserClient.cpp'),
                '-framework', 'DriverKit', '-framework', 'SCSIControllerDriverKit']
        if a.linker:
            sdk_version = json.loads((sdk / 'SDKSettings.json').read_text())['Version']
            argv += ['-fuse-ld=' + str(pathlib.Path(a.linker).absolute()), '-Wl,-adhoc_codesign',
                     '-Wl,-platform_version,driverkit,' + a.deployment_target + ',' + sdk_version]
        argv += ['-o', str(binary)]
        subprocess.run(argv, check=True)
        properties = image_properties(binary, a.arch, a.deployment_target, json.loads((sdk / 'SDKSettings.json').read_text())['Version'])
        shutil.copyfile(ROOT / 'macos/driverkit/Info.plist', bundle / 'Contents/Info.plist')
        # A signing recipe, not a claim that restricted entitlements are provisioned.
        shutil.copyfile(ROOT / 'macos/driverkit/Driver.entitlements', temp / 'Driver.entitlements')
        source_hashes = {str(f.relative_to(ROOT)): iig.sha(f) for f in [
            ROOT / 'macos/driverkit/ZcController.cpp', ROOT / 'macos/driverkit/Info.plist',
            ROOT / 'macos/driverkit/Driver.entitlements']}
        evidence = {'architecture': a.arch, 'interface_manifest': manifest,
                    'source_hashes': source_hashes, 'binary_sha256': iig.sha(binary),
                    'compiler': subprocess.check_output([compiler, '--version'], text=True).strip(),
                    'command': argv, 'image_properties': properties, 'production_signed': False, 'runtime_activation_verified': False,
                    'artifact': 'Development DriverKit extension only; requires signed containing app and approved entitlements for installation'}
        (temp / 'build-manifest.json').write_text(json.dumps(evidence, indent=2) + '\n')
        archive = temp / ('zcutils-driverkit-' + a.arch + '-development.tar.gz')
        with tarfile.open(archive, 'w:gz') as tar:
            for f in [bundle, temp / 'Driver.entitlements', temp / 'build-manifest.json']:
                tar.add(f, arcname=f.name)
        temp.rename(a.out)
    print(json.dumps({'output': str(a.out), 'arch': a.arch, 'binary_sha256': evidence['binary_sha256'],
                      'production_signed': False, 'runtime_activation_verified': False}))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print('build-driverkit-cached: ' + str(error), file=sys.stderr)
        sys.exit(1)
