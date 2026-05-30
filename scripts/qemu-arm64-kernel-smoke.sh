#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LINUX_SRC="${LINUX_SRC:-/home/rob/dev-workspace/src/linux}"
BUILD_DIR="${BUILD_DIR:-/home/rob/dev-workspace/build/linux-arm64-ec2-graviton}"
KERNEL_IMAGE="${KERNEL_IMAGE:-$BUILD_DIR/arch/arm64/boot/Image}"
WORK_DIR="${WORK_DIR:-$REPO_ROOT/qemu-zcrx/arm64-kernel-smoke}"
LOG="${LOG:-$WORK_DIR/qemu-arm64-kernel-smoke.log}"
TIMEOUT="${TIMEOUT:-180s}"
QEMU_MEM="${QEMU_MEM:-1024M}"
QEMU_SMP="${QEMU_SMP:-2}"
EXPECTED_RELEASE_SUBSTRING="${EXPECTED_RELEASE_SUBSTRING:-io-slots-graviton}"

need() {
	if ! command -v "$1" >/dev/null 2>&1; then
		echo "missing required command: $1" >&2
		case "$1" in
		qemu-system-aarch64)
			echo "install on Debian/Ubuntu with: sudo apt-get install -y qemu-system-arm qemu-efi-aarch64" >&2
			;;
		aarch64-linux-gnu-gcc)
			echo "install on Debian/Ubuntu with: sudo apt-get install -y gcc-aarch64-linux-gnu" >&2
			;;
		cpio)
			echo "install on Debian/Ubuntu with: sudo apt-get install -y cpio" >&2
			;;
		esac
		exit 127
	fi
}

need qemu-system-aarch64
need aarch64-linux-gnu-gcc
need cpio
need gzip
need timeout

if [ ! -s "$KERNEL_IMAGE" ]; then
	echo "missing arm64 kernel image: $KERNEL_IMAGE" >&2
	echo "build it first with: scripts/ec2-graviton-kernel-build.sh" >&2
	exit 1
fi

mkdir -p "$WORK_DIR"
root="$WORK_DIR/initramfs-root"
rm -rf "$root"
mkdir -p "$root"/{dev,proc,sys,tmp}

cat > "$WORK_DIR/init.c" <<'EOF_C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/reboot.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <unistd.h>

#ifndef EXPECTED_RELEASE_SUBSTRING
#define EXPECTED_RELEASE_SUBSTRING "io-slots-graviton"
#endif

static void say(const char *fmt, ...)
{
	va_list ap;

	va_start(ap, fmt);
	vprintf(fmt, ap);
	va_end(ap);
	fflush(stdout);
}

static void mount_one(const char *src, const char *target, const char *fstype)
{
	if (mount(src, target, fstype, 0, "") != 0 && errno != EBUSY) {
		say("mount %s on %s failed: errno=%d\n", fstype, target, errno);
	}
}

static void poweroff(void)
{
	sync();
	syscall(SYS_reboot, LINUX_REBOOT_MAGIC1, LINUX_REBOOT_MAGIC2,
		LINUX_REBOOT_CMD_POWER_OFF, NULL);
	for (;;) {
		pause();
	}
}

int main(void)
{
	struct utsname u;
	int ok = 1;
	int fd;

	setvbuf(stdout, NULL, _IONBF, 0);
	mkdir("/proc", 0555);
	mkdir("/sys", 0555);
	mkdir("/dev", 0755);
	mount_one("proc", "/proc", "proc");
	mount_one("sysfs", "/sys", "sysfs");
	mount_one("devtmpfs", "/dev", "devtmpfs");

	if (uname(&u) != 0) {
		say("uname failed: errno=%d\n", errno);
		poweroff();
	}

	say("zcutils-arm64-qemu-smoke: sysname=%s machine=%s release=%s\n",
		u.sysname, u.machine, u.release);

	if (strcmp(u.machine, "aarch64") != 0) {
		say("zcutils-arm64-qemu-smoke: unexpected machine\n");
		ok = 0;
	}
	if (strstr(u.release, EXPECTED_RELEASE_SUBSTRING) == NULL) {
		say("zcutils-arm64-qemu-smoke: release missing expected substring %s\n",
			EXPECTED_RELEASE_SUBSTRING);
		ok = 0;
	}

	fd = open("/proc/config.gz", O_RDONLY);
	if (fd < 0) {
		say("zcutils-arm64-qemu-smoke: /proc/config.gz missing: errno=%d\n", errno);
		ok = 0;
	} else {
		close(fd);
	}

	say("zcutils-arm64-qemu-smoke: %s\n", ok ? "ok" : "failed");
	poweroff();
	return ok ? 0 : 1;
}
EOF_C

aarch64-linux-gnu-gcc \
	-static -Os -s \
	-DEXPECTED_RELEASE_SUBSTRING="\"$EXPECTED_RELEASE_SUBSTRING\"" \
	"$WORK_DIR/init.c" \
	-o "$root/init"

initrd="$WORK_DIR/initramfs.cpio.gz"
(cd "$root" && find . -print0 | cpio --null -o -H newc) | gzip -9 > "$initrd"

append="console=ttyAMA0 panic=-1 oops=panic rdinit=/init zcutils_arm64_smoke=1"

timeout "$TIMEOUT" qemu-system-aarch64 \
	-machine virt \
	-cpu max \
	-m "$QEMU_MEM" \
	-smp "$QEMU_SMP" \
	-nographic \
	-no-reboot \
	-nodefaults \
	-serial mon:stdio \
	-kernel "$KERNEL_IMAGE" \
	-initrd "$initrd" \
	-append "$append" | tee "$LOG"

if ! grep -q "zcutils-arm64-qemu-smoke: ok" "$LOG"; then
	echo "arm64 QEMU kernel smoke failed; log: $LOG" >&2
	exit 1
fi

echo "arm64 QEMU kernel smoke passed; log: $LOG"
