#!/usr/bin/env python3
"""Real linker regressions: DriverKit has no main; macOS still requires it."""
import argparse
import pathlib
import struct
import subprocess
import tempfile

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--linker', required=True)
p.add_argument('--clang', default='clang-19')
a = p.parse_args()


def image(path):
    data = path.read_bytes()
    header = struct.unpack_from('<8I', data)
    offset = 32
    commands = []
    for _ in range(header[4]):
        cmd, length = struct.unpack_from('<II', data, offset)
        commands.append(cmd)
        offset += length
    return header, commands


with tempfile.TemporaryDirectory(prefix='driverkit-linker-test-') as name:
    root = pathlib.Path(name)
    for platform, version in [('macos', '14.0'), ('driverkit', '23.0')]:
        for case, code in [('no-main', 'int other(void) { return 0; }'),
                           ('main', 'int main(void) { return 0; }'),
                           ('undefined', 'extern void missing(void); void other(void) { missing(); }')]:
            source = root / (platform + '-' + case + '.c')
            source.write_text(code)
            obj = source.with_suffix('.o')
            binary = source.with_suffix('.macho')
            subprocess.run([a.clang, '-target', 'arm64-apple-' + platform + version, '-ffreestanding',
                            '-fno-stack-protector', '-c', str(source), '-o', str(obj)], check=True)
            args = [a.linker, '-arch', 'arm64', '-platform_version', platform, version,
                    '24.5' if platform == 'driverkit' else '15.0', str(obj), '-o', str(binary)]
            result = subprocess.run(args, capture_output=True, text=True)
            if case == 'undefined':
                assert result.returncode != 0 and '_missing' in result.stderr, result.stderr
            elif platform == 'macos' and case == 'no-main':
                assert result.returncode != 0 and '_main' in result.stderr, result.stderr
            else:
                assert result.returncode == 0, result.stderr
                header, commands = image(binary)
                assert header[3] == 2  # MH_EXECUTE, including the DriverKit image
                assert (0x80000028 in commands) == (platform == 'macos')
            print(platform, case, 'PASS')
