# DriverKit linker compatibility

Stock LLVM LLD 19.1.7 creates a `_main` reference and `LC_MAIN` for every
`MH_EXECUTE`. Apple ld instead omits these for a DriverKit executable. This
small, platform-specific patch makes that distinction. It does not ignore
undefined symbols, fabricate a main function, edit linked Mach-O headers, or
turn the extension into a dylib. Non-DriverKit entry-point behavior is unchanged.

The build script downloads checksum-pinned upstream LLD, CMake support and
libunwind source archives and links a Mach-O-only driver against locally
installed LLVM 19.1.7 development libraries. Apple SDKs are not downloaded here.
Use Python 3.12+, CMake, Ninja, a host C++ compiler, `patch`, and LLVM 19.1.7
headers/libraries/tools (`llvm-19-dev` on the tested Debian host).

```sh
python3 tools/driverkit-lld/build.py --out /tmp/zc-driverkit-lld --jobs 4
python3 tools/driverkit-lld/test.py --linker /tmp/zc-driverkit-lld/bin/ld64.lld
```

`build-manifest.json` records source checksums, patch/launcher hashes and the
resulting binary hash. This is a local compatibility tool, not an upstream LLVM
release or a claim of native-driver runtime validation. The tests exercise both
platforms, check `LC_MAIN` presence/absence, and ensure genuine unresolved symbols
still fail. The extension packager also checks DriverKit platform/deployment,
CPU type, class metadata, library paths and absence of a normal entry point.

LLVM 19's Clang emits the older `-driverkit_version_min` linker argument. LLD
warns that it does not implement that argument; the packager additionally passes
explicit `-platform_version driverkit MIN SDK` and checks the emitted build
version, so the deployment target is not inferred from the ignored argument.
