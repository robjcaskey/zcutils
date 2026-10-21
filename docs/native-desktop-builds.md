# Local desktop builds and infrastructure exercise

Users should build locally. The macOS, DriverKit, and Windows GitHub workflows
are retained to document a one-time infrastructure exercise. After successful
runs, automatic triggers are removed and the workflows are disabled in GitHub.
There are no nightly/weekly schedules, self-hosted runners, AWS credentials,
EC2 launch commands, or deployment steps in these workflows. Re-enabling one
requires an explicit maintainer action. Standard GitHub-hosted runners were used;
the workflows do not allocate EC2 Mac dedicated hosts.

## macOS userspace

Install Rust, Xcode command line tools, and protobuf (`brew install protobuf`).
Run `bash scripts/build-macos-userspace.sh`, then:

```sh
PROTOC=protoc cargo test --locked --lib --bin zcglobal-policy-node --bin zcglobal-kubernetes-adapter
python3 scripts/test-portable-storage.py
```

## macOS DriverKit development bundle

This is an experimental SCSIControllerDriverKit disk edge plus a normal userspace
`zcnblk-macos-client` broker. Placement, mirroring, lanes, and storage remain in
userspace after the edge. The driver never treats another block device as a RAID
primitive. The SCSI path uses `UserGetDataBuffer`, never dereferences a DMA address.
BlockStorageDeviceDriverKit's DMA-only read/write interface cannot provide the
CPU data access this software-backed device requires.

Install full Xcode, select it with `xcode-select`, and install Rust/protobuf:

```sh
bash scripts/build-macos-driverkit.sh
PROTOC=protoc cargo build --locked --bin zcnblk-macos-client
PROTOC=protoc cargo test --locked --bin zcnblk-macos-client
```

The result is `target/macos-driverkit/ZcStorage.app`. Run the embedded `ZcStorage`
executable to request activation, or pass `--deactivate` to request removal.
The app uses Apple's SystemExtensions approval flow. The build script only
builds/signs a development bundle; it does not activate it or modify SIP/boot policy.
The standalone broker must also be signed with `Broker.entitlements`.

Ad-hoc signatures are **not** sufficient for installation with normal macOS
security. Production needs an Apple Developer team, approved DriverKit and
SCSI-controller entitlements, app system-extension-install entitlement, broker
userclient-access entitlement, matching provisioning profiles, signing, and
notarization. Bundle IDs in the plists, Swift app, and broker entitlement must
agree with the team's approved IDs. No private signing keys are stored in CI.

After valid provisioning/activation, the intended invocation is:

```sh
./target/debug/zcnblk-macos-client 127.0.0.1:19603 67108864
```

Capacity is explicit and must match the endpoint; the current wire protocol has
no capacity discovery. Use an exclusive, disposable target. Do not attach the
same writable filesystem concurrently on another machine. One controller, one
LUN, one broker, one outstanding request, 4096-byte sectors and maximum I/O.
The broker polls at 1 ms, fails the session on transport errors, maps FUA writes
and SYNCHRONIZE CACHE to remote SYNC, and advertises no UNMAP support. Ordinary
write acknowledgements retain the endpoint's normal write-ACK semantics.
A failed/reset session requires closing and reopening the broker. This is a
functional bring-up design, not a high-IOPS implementation or benchmark.

Compilation and parser tests do not prove disk mounting. Runtime activation,
Disk Utility formatting, filesystem readback, and disconnect/reconnect must be
validated on a provisioned test Mac before calling the native edge usable.
Never format a disk until its identity has been positively verified as the new
zcutils virtual device. The developer Mac used for this work has SIP enabled
and no valid signing identity; the build does not change that state. The
ad-hoc artifact passes signature-integrity verification, but its restricted
broker entitlement causes launch rejection on that unprovisioned host. This is
not an installation or mounting success.

## Cached DriverKit interfaces and Linux builds

Routine builds use checked-in Apple-generated headers/C++ under
`macos/driverkit/generated/{arm64,x86_64}`. They do not invoke `iig`.
Each architecture's manifest records both `.iig` input hashes, all four generated
output hashes, the complete SDK tree hash (including inherited interfaces and
link libraries), deployment target, generation options, Apple `iig` executable
hash and Xcode version. Changing a `.cpp` implementation does not invalidate the
cache. Changing an interface, generated output, SDK, architecture or deployment
target fails preflight before compilation; no automatic regeneration/fallback
occurs. Fingerprints detect stale inputs; they are not a signing certificate.

Keep the matching DriverKit SDK separately in a private local cache; Apple SDK
headers/tools are not vendored in this repository or automatically downloaded.
LLVM Clang and the Mach-O linker `ld64.lld` are required on Linux. For example:

```sh
python3 scripts/test-driverkit-cache.py
python3 scripts/driverkit-interfaces.py verify \
  --sdk /path/to/DriverKit.sdk --arch arm64
python3 scripts/build-driverkit-cached.py \
  --sdk /path/to/DriverKit.sdk --arch arm64 \
  --clang clang++ --linker /path/to/ld64.lld \
  --out target/linux-driverkit-arm64
```

Repeat with `--arch x86_64` and a distinct output directory for Intel. Outputs
include the development `.dext`, an archive, and a manifest containing source,
SDK/interface and binary hashes plus the compiler command/version. The archive
is an extension only, not a signed containing app or an activated device.
A Linux-built Mach-O/ad-hoc signature does not grant DriverKit entitlements.
The activation helper remains a separate build using the Apple SystemExtensions
framework; the existing Mac script builds that helper and a development app.
Runtime approval, signing and mounting requirements above remain unchanged.

To refresh after an intentional interface/SDK change, use Apple `iig` on a Mac
with full Xcode. Generate into a new directory, review the source/manifest diff,
and replace the corresponding checked-in cache:

```sh
sdk=$(xcrun --sdk driverkit --show-sdk-path)
for arch in arm64 x86_64; do
  python3 scripts/driverkit-interfaces.py generate \
    --sdk "$sdk" --cache /tmp/zc-iig-refresh --arch "$arch"
done
```

The disabled, manual-only DriverKit workflow also has a `refresh_iig` input for
this maintenance step. It generates both architectures, compiles each with
Apple's compiler, and exports the cache/SDK/native-build evidence in an
authenticated artifact with one-day retention. Enable it explicitly for that
single invocation and disable it after the run starts. There are no scheduled
triggers and it never launches or renews an EC2 Mac.

## Windows userspace and native driver

Install Rust (MSVC), Visual Studio C++ build tools, protobuf, NASM, and NuGet.
Ensure `protoc`, `nasm`, and `nuget` are on PATH. In PowerShell:

```powershell
./scripts/windows/build-userspace.ps1
cargo test --locked --lib
cargo test --locked --bin zcnblk-client --bin zcnblk-portable-target --bin zcnblk-windows-client
python scripts/test-windows-storage.py
./scripts/windows/build-driver.ps1
```

The native build downloads pinned Microsoft WDK/SDK NuGet packages and emits
`target/windows-driver/zcnblk.sys` and `zcnblk.inf`. It does not install the
driver, trust certificates, or change test-signing/boot policy. CI artifacts are
unsigned development outputs, not a production install package. Use
`scripts/windows/package-zcnblk-driver.ps1` with your own signing certificate
and official SignTool/Inf2Cat for separate packaging. Driver loading and I/O
require a dedicated test machine and appropriate signing.

## Recorded infrastructure validation

All three workflows passed on October 20, 2026, and were then disabled in GitHub:

- [macOS ARM64/Intel and Linux regression](https://github.com/robjcaskey/zcutils/actions/runs/39514725515): 145 tests passed and 2 ignored on each Mac; WAL recovery, backup/restore, mirror restart/catch-up and regional quorum smoke checks passed.
- [Windows x64](https://github.com/robjcaskey/zcutils/actions/runs/39514180059): 55 library tests, TCP WAL recovery/backup/restore, six userspace executables, and the native MSVC/WDK Storport build passed (attempt 2).
- [DriverKit](https://github.com/robjcaskey/zcutils/actions/runs/39514514040): ARM64 extension, activation app, broker, three SCSI behavior tests, and archive passed.

Exact source revisions and job results are in [the evidence record](native-desktop-ci-evidence.json).
The final source differs from these build revisions only in documentation and
workflow trigger removal for the corresponding platform paths.

API references: [SCSIControllerDriverKit](https://developer.apple.com/documentation/scsicontrollerdriverkit),
[UserGetDataBuffer](https://developer.apple.com/documentation/scsicontrollerdriverkit/iouserscsiparallelinterfacecontroller/usergetdatabuffer),
and [Apple DTS's software-backed storage discussion](https://developer.apple.com/forums/thread/837879).
