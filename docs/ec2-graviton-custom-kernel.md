# EC2 Graviton Custom Kernel Path

This is the practical path for testing the Linux 7.0.8 io-slot/ZCRX/send-zc
kernel work on one sacrificial `c8gn` Graviton node before baking an AMI or
touching a benchmark cluster.

## Current Tree

Use the local kernel tree:

```bash
/home/rob/dev-workspace/src/linux
```

Expected branch:

```bash
rob/io-slots-v7.0.8-backport-attempt
```

At inspection time it was `36` commits ahead of `v7.0.8`, with head:

```text
9737ce8e9ad1 io_uring: return byte count for slot overflow bios
```

Relevant patch surface:

- `include/uapi/linux/io_uring.h`
  - `IORING_OP_SEND_ZC`
  - `IORING_OP_RECV_ZC`
  - `IORING_OP_SLOT_RW`
  - `IORING_REGISTER_ZCRX_IFQ = 32`
  - `IORING_REGISTER_ZCRX_CTRL = 36`
  - `IORING_REGISTER_IO_SLOT = 38`
  - `IORING_UNREGISTER_IO_SLOT = 39`
- `include/uapi/linux/io_uring/query.h`
  - `IO_URING_QUERY_ZCRX = 1`
- `io_uring/Kconfig`
  - `CONFIG_IO_URING`
  - `CONFIG_IO_URING_ZCRX`, depends on `CONFIG_NET_RX_BUSY_POLL`
  - `CONFIG_IO_URING_SLOT_RW`
- `drivers/nvme/host/pci.c`
  - persistent DMA setup for registered slots
- `drivers/net/netdevsim/`
  - simulator coverage only, useful for local/QEMU validation but not an EC2 ENA
    proof.

The stock `c8gn.8xlarge` smoke failed both the send-zc opcode query and ZCRX IFQ
registration with `EINVAL`, so the first goal is to prove the custom kernel
exposes the UAPI and keeps normal ENA/NVMe boot/network behavior intact.

## Build Packages

From this repository:

```bash
scripts/ec2-graviton-kernel-build.sh
```

Default output:

```bash
qemu-zcrx/ec2-graviton-kernel-out/
```

To validate the config and source selection without compiling the full kernel:

```bash
CONFIG_ONLY=1 scripts/ec2-graviton-kernel-build.sh
```

The script builds arm64 Debian packages with:

- `ARCH=arm64`
- `CROSS_COMPILE=aarch64-linux-gnu-`
- `KBUILD_DEBARCH=arm64`
- `CONFIG_LOCALVERSION="-io-slots-graviton"`
- `CONFIG_LOCALVERSION_AUTO=n`
- `LOCALVERSION=` on the `make` command line so the package release does not
  grow a stray `+` suffix from SCM state.

It starts from `arm64 defconfig` and forces the EC2-relevant boot and test
surface:

- UEFI/ACPI/PCI: `CONFIG_EFI`, `CONFIG_EFI_STUB`, `CONFIG_ACPI`, `CONFIG_PCI`
- EBS/root disk: `CONFIG_BLK_DEV_INITRD`, `CONFIG_BLK_DEV_NVME`,
  `CONFIG_NVME_MULTIPATH`
- Network: `CONFIG_ENA_ETHERNET`, `CONFIG_NET_RX_BUSY_POLL`
- io_uring: `CONFIG_IO_URING`, `CONFIG_IO_URING_ZCRX`,
  `CONFIG_IO_URING_SLOT_RW`
- Test helpers: `CONFIG_NETDEVSIM=m`, `CONFIG_BLK_DEV_NULL_BLK=m`,
  `CONFIG_CONFIGFS_FS`

Install build prerequisites if needed:

```bash
sudo apt-get install -y build-essential bc bison flex libssl-dev libelf-dev \
  dwarves debhelper rsync gcc-aarch64-linux-gnu
```

## Local Build And QEMU Smoke

Yes: build locally first. The useful local path is an arm64 cross-build on this
host, followed by an arm64 QEMU boot smoke. Do not compile the kernel inside an
emulated arm64 guest unless there is no alternative; it is much slower and does
not add EC2-specific confidence.

Fast config audit:

```bash
CONFIG_ONLY=1 scripts/ec2-graviton-kernel-build.sh
```

Full local package build:

```bash
scripts/ec2-graviton-kernel-build.sh
```

Boot the built arm64 `Image` under QEMU:

```bash
scripts/qemu-arm64-kernel-smoke.sh
```

The QEMU smoke builds a tiny static aarch64 initramfs, boots
`$BUILD_DIR/arch/arm64/boot/Image` with `qemu-system-aarch64`, and checks:

- the kernel reaches userspace on an arm64 `virt` machine;
- `uname -m` is `aarch64`;
- `uname -r` contains `io-slots-graviton`;
- `/proc/config.gz` exists, proving `CONFIG_IKCONFIG_PROC=y`.

Install the QEMU tools if the smoke reports them missing:

```bash
sudo apt-get install -y qemu-system-arm qemu-efi-aarch64
```

Local QEMU is not an ENA, NVMe, GRUB, or EC2 Nitro proof. It catches bad arm64
config/boot regressions before spending EC2 time; the sacrificial Graviton node
is still the proof for ENA, EBS/NVMe, package install, reboot, and `zcprobe`.

## Ad-Hoc Graviton Build Host

For native arm64 package builds or AMI baking, start one disposable medium
Graviton node. A good default is `c7g.4xlarge`: 16 arm64 vCPUs, 32 GiB RAM, no
local NVMe, and cheap enough for a short build lease. Do not enable EFA for this
build host; save EFA/ENA Express experiments for network validation nodes.

Check current Spot prices first:

```bash
/home/rob/spot-helper/ec2_perf_spot.py spot-prices \
  --profile tf \
  --regions us-east-1 \
  --instance-types c7g.2xlarge,c7g.4xlarge,c8g.4xlarge,m7g.2xlarge,m7g.4xlarge \
  --nodes 1 \
  --limit 30
```

Dry-run the build host request:

```bash
/home/rob/spot-helper/ec2_perf_spot.py launch \
  --profile tf \
  --region us-east-1 \
  --availability-zone us-east-1a \
  --subnet-id subnet-9cf16dc7 \
  --security-group-ids sg-06a6264f49bd2329d,sg-e0dfdb9d \
  --key-name adhocMasterKeypair \
  --instance-type c7g.4xlarge \
  --nodes 1 \
  --max-spot-price 0.35 \
  --max-total-cost 5 \
  --root-gb 128 \
  --no-enable-efa \
  --drop-dead-utc YYYY-MM-DDTHH:MM:SSZ \
  --run-id graviton-kernel-build-YYYYMMDDTHHMMZ \
  --inventory qemu-zcrx/ec2-graviton-build-inventory.json
```

Add `--yes` only after the printed AMI, AZ, subnet, tags, and cost ceiling are
right. Print SSH commands after launch:

```bash
/home/rob/spot-helper/ec2_perf_spot.py ssh-commands \
  --inventory qemu-zcrx/ec2-graviton-build-inventory.json
```

On the build host, clone or sync the kernel tree and this repo, install the same
build prerequisites, then run `scripts/ec2-graviton-kernel-build.sh`. Native
arm64 builds are slower than a large x86 cross-build in some configurations, but
they remove cross-toolchain doubt before AMI baking.

Terminate the build host as soon as packages or the AMI are captured:

```bash
/home/rob/spot-helper/ec2_perf_spot.py terminate \
  --profile tf \
  --region us-east-1 \
  --run-id graviton-kernel-build-YYYYMMDDTHHMMZ \
  --yes
```

## Deploy To One Sacrificial Node

Do not deploy to the active benchmark cluster. Copy the build output to one
throwaway arm64 Ubuntu/Debian Graviton instance:

```bash
host=ubuntu@HOST
ssh "$host" 'mkdir -p /var/tmp/zcutils-graviton-kernel'
rsync -av qemu-zcrx/ec2-graviton-kernel-out/*.deb \
  scripts/ec2-graviton-kernel-deploy.sh \
  scripts/ec2-graviton-kernel-probe.sh \
  "$host:/var/tmp/zcutils-graviton-kernel/"
ssh "$host" 'bash /var/tmp/zcutils-graviton-kernel/ec2-graviton-kernel-deploy.sh /var/tmp/zcutils-graviton-kernel'
```

The deploy script installs the image/header packages, regenerates initramfs and
GRUB, and stages a one-shot boot with `grub-reboot` when available. It does not
change the permanent GRUB default unless `STAGE_PERMANENT=1` is set.
It leaves `linux-libc-dev` uninstalled by default; set `INSTALL_LIBC_DEV=1` only
if the sacrificial node needs the packaged userspace headers.
Use `/var/tmp`, the home directory, or another persistent path for the copied
scripts; `/tmp` may be cleared during reboot on Ubuntu cloud images.

Ubuntu cloud images often start with `GRUB_DEFAULT=0`. After installing a newer
kernel, that can make the custom kernel the default entry even if
`grub-reboot` is used. For true one-shot rollback, set saved-default mode and
preserve the vendor kernel before rebooting:

```bash
ssh "$host" 'set -e
sudo cp -a /etc/default/grub /etc/default/grub.zcutils-before-saved
sudo sed -i -E "s/^GRUB_DEFAULT=.*/GRUB_DEFAULT=saved/" /etc/default/grub
sudo grub-set-default "Advanced options for Ubuntu>Ubuntu, with Linux $(uname -r)"
sudo update-grub
sudo grub-reboot "Advanced options for Ubuntu>Ubuntu, with Linux 7.0.8-io-slots-graviton"
sudo grub-editenv list'
```

Reboot manually after confirming the staged entry:

```bash
ssh "$host" 'sudo reboot'
```

## First-Boot Validation

After reboot:

```bash
ssh "$host" 'bash /var/tmp/zcutils-graviton-kernel/ec2-graviton-kernel-probe.sh'
```

Minimum pass criteria before any traffic:

- `uname -m` is `aarch64`.
- `uname -r` includes `io-slots-graviton`.
- `/boot/config-$(uname -r)` or `/proc/config.gz` reports:
  - `CONFIG_IO_URING=y`
  - `CONFIG_IO_URING_ZCRX=y`
  - `CONFIG_IO_URING_SLOT_RW=y`
  - `CONFIG_ENA_ETHERNET=y` or `m`
  - `CONFIG_BLK_DEV_NVME=y` or `m`
- ENA is loaded or built in and `ethtool -i IFACE` reports the active driver as
  `ena`.
- Root disk and network survived a cold reboot.
- `zcutils zcprobe` reports send-zc and io-slot availability from the custom
  kernel.

If `zcutils` is not built on the arm64 node yet, a minimal direct syscall check
is still useful. `IORING_REGISTER_QUERY` should report at least:

- `nr_request_opcodes > 65` for `IORING_OP_SLOT_RW`;
- `nr_register_opcodes > 39` for io-slot register/unregister;
- `nr_register_opcodes > 36` for ZCRX IFQ/control registration;
- `IO_URING_QUERY_ZCRX` returns `0`.

ZCRX may still fail on real ENA if the driver/hardware does not expose the
required header/data split and queue steering behavior to the kernel ZCRX path.
That is an expected result to record, not a deployment failure.

## ZCRX Caveats On ENA

Linux ZCRX requires NIC support outside the io_uring syscall surface:

- header/data split
- steering selected flows to the ZCRX queues
- keeping other traffic away from those queues via RSS

The kernel API does not configure that NIC state for the application. On ENA,
verify what is actually available with:

```bash
iface=eth0
ethtool -i "$iface"
ethtool -k "$iface"
ethtool -l "$iface"
ethtool -x "$iface" || true
ethtool -n "$iface" || true
```

Do not assume ENA Express/SRD implies ZCRX support. ENA Express is an EC2
attachment/network feature that can improve single-flow bandwidth and tail
latency, while ZCRX is a Linux NIC receive-buffer placement path.

Source-level ENA status from the local tree:

- The in-tree ENA driver under `drivers/net/ethernet/amazon/ena` is version
  `2.1.0` and does not use page-pool allocation or `ndo_queue_mem_*`.
- AWS ENA driver tag `ena_linux_2.16.1`, inspected locally from
  `https://github.com/amzn/amzn-drivers`, has page-pool support but still does
  not advertise the ZCRX queue-memory hooks by name.
- Therefore this custom kernel should be expected to fix `zcprobe` query
  support, send-zc probing, and io-slot/NVMe probing first. Real ENA-backed
  ZCRX is a separate driver-backport task if IFQ registration still returns
  `EINVAL` or `EOPNOTSUPP`.

## Rollback

The normal path is one-shot boot only:

1. Keep the vendor kernel as the permanent default.
2. Install the custom packages.
3. Stage one boot with `grub-reboot`.
4. Reboot.
5. If the custom kernel fails to boot, stop/start or reboot again and GRUB
   should return to the permanent vendor default.

If the instance does not return:

1. Stop the instance.
2. Detach the root EBS volume.
3. Attach it to a healthy arm64 rescue instance.
4. Mount the root filesystem and restore the GRUB default to the vendor kernel.
5. Optionally remove `vmlinuz-*io-slots-graviton*`,
   `initrd.img-*io-slots-graviton*`, and `/lib/modules/*io-slots-graviton*`.

For production AMI baking, only create the image after this one-node cycle has
survived boot, SSH, ENA, NVMe, and `zcprobe`.

## Primary References

- Linux ZCRX documentation: https://docs.kernel.org/networking/iou-zcrx.html
- Linux kbuild variables: https://docs.kernel.org/kbuild/kbuild.html
- EC2 C8gn instance specs: https://docs.aws.amazon.com/ec2/latest/instancetypes/co.html
- ENA Express/SRD behavior: https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ena-express.html
