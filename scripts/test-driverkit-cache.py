#!/usr/bin/env python3
"""Exercise stale-cache failures and compiler preflight without Apple tooling."""
import importlib.util
import json
import pathlib
import sys
import struct
import tempfile
import unittest
from unittest.mock import patch

HERE = pathlib.Path(__file__).resolve().parent


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


class CacheTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        self.iig = module('cache_test', HERE / 'driverkit-interfaces.py')
        self.iig.ROOT = self.root
        self.sdk = self.root / 'DriverKit.sdk'
        self.sdk.mkdir()
        (self.sdk / 'SDKSettings.json').write_text('{"Version":"test-fixture"}')
        (self.sdk / 'base.iig').write_text('base interface fixture')
        self.cache = self.root / 'generated'
        (self.cache / 'arm64').mkdir(parents=True)
        (self.root / 'macos/driverkit').mkdir(parents=True)
        for c in self.iig.CLASSES:
            (self.root / 'macos/driverkit' / (c + '.iig')).write_text(c + ' interface fixture')
            for suffix in ['.h', '.cpp']:
                (self.cache / 'arm64' / (c + suffix)).write_text(c + suffix + ' generated fixture')
        self.data = {'recipe': self.iig.recipe('arm64', '23.0'), 'inputs': self.iig.sources(),
                     'sdk': self.iig.sdk_identity(self.sdk),
                     'generator': {'sha256': 'fixture-not-a-real-generator', 'xcode_version': 'test-fixture'},
                     'outputs': {c + suffix: self.iig.sha(self.cache / 'arm64' / (c + suffix))
                                 for c in self.iig.CLASSES for suffix in ['.h', '.cpp']}}
        self.write_manifest()

    def write_manifest(self):
        (self.cache / 'arm64/manifest.json').write_text(json.dumps(self.data))

    def verify(self):
        return self.iig.verify(self.cache, self.sdk, 'arm64', '23.0')

    def test_verification_needs_no_apple_tools_and_ignores_implementation_edits(self):
        with patch.object(self.iig.subprocess, 'run', side_effect=AssertionError('no subprocess')):
            self.verify()
            (self.root / 'macos/driverkit/ZcController.cpp').write_text('new implementation')
            self.verify()

    def test_changed_interface_is_rejected(self):
        (self.root / 'macos/driverkit/ZcController.iig').write_text('ABI changed')
        with self.assertRaisesRegex(ValueError, 'interfaces changed'):
            self.verify()

    def test_changed_sdk_inherited_interface_is_rejected(self):
        (self.sdk / 'base.iig').write_text('base ABI changed')
        with self.assertRaisesRegex(ValueError, 'SDK contents differ'):
            self.verify()

    def test_changed_generated_source_is_rejected(self):
        (self.cache / 'arm64/ZcController.cpp').write_text('generated ABI changed')
        with self.assertRaisesRegex(ValueError, 'output changed'):
            self.verify()

    def test_deployment_change_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'recipe'):
            self.iig.verify(self.cache, self.sdk, 'arm64', '24.0')

    def test_missing_or_extra_output_is_rejected(self):
        self.data['outputs']['../../outside.cpp'] = 'bad'
        self.write_manifest()
        with self.assertRaisesRegex(ValueError, 'coverage'):
            self.verify()

    def test_missing_generator_provenance_is_rejected(self):
        self.data['generator'] = {}
        self.write_manifest()
        with self.assertRaisesRegex(ValueError, 'provenance'):
            self.verify()

    def fake_image(self, normal_entry=False):
        build = struct.pack('<6I', 0x32, 24, 10, 23 << 16, (24 << 16) | (5 << 8), 0)
        segment = bytearray(152)
        struct.pack_into('<II', segment, 0, 0x19, 152)
        struct.pack_into('<I', segment, 64, 1)
        segment[72:85] = b'__osclassinfo'
        name = b'/System/DriverKit/usr/lib/libc++.dylib\0'
        length = (24 + len(name) + 7) & ~7
        library = struct.pack('<6I', 0xc, length, 24, 0, 0, 0) + name
        library += bytes(length - len(library))
        commands = [build, bytes(segment), library]
        if normal_entry:
            commands.append(struct.pack('<IIQQ', 0x80000028, 24, 0, 0))
        header = struct.pack('<8I', 0xfeedfacf, 0x100000c, 0, 2, len(commands), sum(map(len, commands)), 5, 0)
        image = self.root / 'fixture.macho'
        image.write_bytes(header + b''.join(commands))
        return image

    def test_image_sdk_version_is_checked_without_shadowing_the_argument(self):
        builder = module('image_test', HERE / 'build-driverkit-cached.py')
        path = self.fake_image()
        builder.image_properties(path, 'arm64', '23.0', '24.5')
        with self.assertRaisesRegex(ValueError, 'SDK build version'):
            builder.image_properties(path, 'arm64', '23.0', '24.4')

    def test_image_with_normal_entry_point_is_rejected(self):
        builder = module('entry_test', HERE / 'build-driverkit-cached.py')
        with self.assertRaisesRegex(ValueError, 'LC_MAIN'):
            builder.image_properties(self.fake_image(normal_entry=True), 'arm64', '23.0', '24.5')

    def test_stale_build_never_invokes_compiler_or_creates_output(self):
        builder = module('builder_test', HERE / 'build-driverkit-cached.py')
        builder.ROOT = self.root
        builder.iig = self.iig
        (self.sdk / 'base.iig').write_text('incompatible SDK')
        out = self.root / 'must-not-exist'
        argv = ['build', '--sdk', str(self.sdk), '--cache', str(self.cache),
                '--arch', 'arm64', '--out', str(out)]
        with patch.object(sys, 'argv', argv), patch.object(builder.subprocess, 'run', side_effect=AssertionError('compiler called')):
            with self.assertRaisesRegex(ValueError, 'SDK contents differ'):
                builder.main()
        self.assertFalse(out.exists())


if __name__ == '__main__':
    unittest.main()
