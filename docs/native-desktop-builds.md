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
