#!/usr/bin/env python3
"""Build a pinned Mach-O-only LLD with DriverKit's no-entry-point semantics.

Linux, Python3.12+, LLVM19.1.7 development files, CMake, Ninja and a host C++
compiler are required. Downloads only checksum-pinned upstream LLVM sources.
"""
import argparse
import hashlib
import json
import pathlib
import shutil
import subprocess
import tarfile
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent
VERSION = '19.1.7'
ARCHIVES = {'lld': '630bf0f31b210e89fd7869bdfd6c376d010fc3b72ee42f08058435035c432e42',
            'cmake': '11c5a28f90053b0c43d0dec3d0ad579347fc277199c005206b963c19aae514e3',
            'libunwind': '10e650f436bc416674f01b5e01177405886f4d0f0b2568c6808044eacad52ea0'}
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--out', type=pathlib.Path, required=True)
p.add_argument('--llvm-config', default='llvm-config-19')
p.add_argument('--jobs', type=int, default=4)
a = p.parse_args()
if subprocess.check_output([a.llvm_config, '--version'], text=True).strip() != VERSION:
    p.error('requires LLVM development files version ' + VERSION)
if not 1 <= a.jobs <= 32:
    p.error('--jobs must be 1..32')
if a.out.exists():
    p.error('--out must be a new directory')
a.out.mkdir(parents=True)
work = a.out.resolve()
for name, digest in ARCHIVES.items():
    archive = work / (name + '-' + VERSION + '.src.tar.xz')
    urllib.request.urlretrieve('https://github.com/llvm/llvm-project/releases/download/llvmorg-' + VERSION + '/' + archive.name, archive)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != digest:
        raise ValueError('LLVM archive checksum mismatch: ' + name)
    with tarfile.open(archive) as tar:
        tar.extractall(work, filter='data')
(work / 'llvm').mkdir()
(work / 'libunwind').symlink_to('libunwind-' + VERSION + '.src', target_is_directory=True)
source = work / ('lld-' + VERSION + '.src')
subprocess.run(['patch', '-p1', '-i', str(ROOT / 'driverkit-entry.patch')], cwd=source, check=True)
with (source / 'CMakeLists.txt').open('a') as f:
    f.write('\nadd_executable(zc-driverkit-ld64 "' + (ROOT / 'main.cpp').as_posix() + '")\n')
    f.write('target_include_directories(zc-driverkit-ld64 PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/include" "${CMAKE_CURRENT_BINARY_DIR}/include")\n')
    f.write('target_link_libraries(zc-driverkit-ld64 PRIVATE lldMachO lldCommon LLVM)\n')
    f.write('set_target_properties(zc-driverkit-ld64 PROPERTIES OUTPUT_NAME ld64.lld RUNTIME_OUTPUT_DIRECTORY "' + (work / 'bin').as_posix() + '")\n')
build = work / 'build'
cmake_dir = subprocess.check_output([a.llvm_config, '--cmakedir'], text=True).strip()
subprocess.run(['cmake', '-G', 'Ninja', '-S', str(source), '-B', str(build),
                '-DLLVM_DIR=' + cmake_dir, '-DLLVM_COMMON_CMAKE_UTILS=' + str(work / ('cmake-' + VERSION + '.src')),
                '-DCMAKE_BUILD_TYPE=Release', '-DLLVM_INCLUDE_TESTS=OFF', '-DLLVM_LINK_LLVM_DYLIB=ON'], check=True)
subprocess.run(['cmake', '--build', str(build), '--target', 'zc-driverkit-ld64', '-j', str(a.jobs)], check=True)
binary = work / 'bin/ld64.lld'
manifest = {'llvm_version': VERSION, 'archives': ARCHIVES,
            'patch_sha256': hashlib.sha256((ROOT / 'driverkit-entry.patch').read_bytes()).hexdigest(),
            'launcher_sha256': hashlib.sha256((ROOT / 'main.cpp').read_bytes()).hexdigest(),
            'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
            'change': 'DriverKit MH_EXECUTE has no _main reference and no LC_MAIN; other platforms retain upstream behavior',
            'runtime_activation_verified': False}
(work / 'build-manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
print(json.dumps({'linker': str(binary), 'binary_sha256': manifest['binary_sha256']}))
