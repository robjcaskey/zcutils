#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_RELEASE=1
INSTALL_APT=1
INSTALL_EFA=0
HUGEPAGES="${ZCUTILS_HUGEPAGES:-}"
TCP_TUNE=1
MANIFEST_PATH="${ZCUTILS_BOOTSTRAP_MANIFEST:-$HOME/.local/state/zcutils/adhoc-bootstrap.env}"

usage() {
	cat <<'EOF'
usage: scripts/welcome-to-the-team.sh [options]

Bootstrap an adhoc zcutils benchmark node after the source tree has been
synced to it.

options:
  --no-apt              skip apt package installation
  --no-build            skip the project build (Rust is still installed)
  --hugepages N|auto    required unless --no-hugepages; auto reserves 1/16 of
                        RAM, clamped to 128..8192 2-MiB pages
  --no-hugepages        do not reserve explicit HugeTLB pages
  --install-efa         install the AWS EFA userspace stack, best effort
  --no-tcp-tune         skip TCP/memlock/sysctl tuning
  --manifest PATH       write machine/bootstrap provenance here
  -h, --help            show this help

environment:
  ZCUTILS_BOOTSTRAP_BINS   cargo bins to build
                           default: zcutils zcblockbench zcnblk-shm-target
                                    zcnblk-fan zcnblk-wal-leaf
                                    zcfanout-logshm-bench zcfanout-logtcp-bench
  ZCUTILS_CLOUD_DAILY_BUDGET_USD
                           recorded daily adhoc ceiling, default: 20
EOF
}

log() {
	printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"
}

warn() {
	printf '[%s] WARNING: %s\n' "$(date -u +%H:%M:%S)" "$*" >&2
}

run_sudo() {
	if [ "$(id -u)" -eq 0 ]; then
		"$@"
	else
		sudo "$@"
	fi
}

apt_update_retry() {
	for attempt in 1 2 3; do
		if run_sudo apt-get update; then
			return 0
		fi
		sleep $((attempt * 5))
	done
	return 1
}

fix_ubuntu_ports_mirror() {
	if ! command -v apt-get >/dev/null 2>&1; then
		return 0
	fi
	if run_sudo grep -Rqs 'ec2\.ports\.ubuntu\.com' \
		/etc/apt/sources.list /etc/apt/sources.list.d 2>/dev/null; then
		log "rewriting stale ec2 ports mirror references"
		run_sudo sed -i \
			's|http://[^ ]*ec2\.ports\.ubuntu\.com/ubuntu-ports|http://ports.ubuntu.com/ubuntu-ports|g' \
			/etc/apt/sources.list /etc/apt/sources.list.d/*.sources 2>/dev/null || true
		run_sudo rm -rf /var/lib/apt/lists/*
	fi
}

install_apt_packages() {
	if ! command -v apt-get >/dev/null 2>&1; then
		warn "apt-get not found; skipping OS package installation"
		return 0
	fi

	fix_ubuntu_ports_mirror
	log "installing build/runtime packages"
	apt_update_retry

	local base_packages=(
		bc
		build-essential
		ca-certificates
		clang
		cmake
		curl
		ethtool
		fio
		git
		hwloc
		iproute2
		jq
		libssl-dev
		liburing-dev
		make
		numactl
		perf-tools-unstable
		pkg-config
		python3
		python3-pip
		rdma-core
		rsync
		sysstat
		tmux
	)
	DEBIAN_FRONTEND=noninteractive run_sudo apt-get install -y "${base_packages[@]}"

	local best_effort_packages=(
		libfabric-bin
		libfabric-dev
		linux-headers-"$(uname -r)"
		linux-tools-common
		linux-tools-"$(uname -r)"
		perftest
	)
	for pkg in "${best_effort_packages[@]}"; do
		DEBIAN_FRONTEND=noninteractive run_sudo apt-get install -y "$pkg" || \
			warn "optional package install failed: $pkg"
	done
}

ensure_rust() {
	export PATH="$HOME/.cargo/bin:$PATH"
	if command -v rustc >/dev/null 2>&1; then
		local version major minor
		version="$(rustc -V | awk '{print $2}')"
		major="${version%%.*}"
		minor="${version#*.}"
		minor="${minor%%.*}"
		if [ "$major" -gt 1 ] || { [ "$major" -eq 1 ] && [ "$minor" -ge 85 ]; }; then
			log "rust already installed: $(rustc -V)"
			return 0
		fi
	fi

	log "installing Rust stable with rustup"
	curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
		sh -s -- -y --profile minimal --default-toolchain stable
	export PATH="$HOME/.cargo/bin:$PATH"
	log "$(rustc -V)"
	log "$(cargo -V)"
}

tune_system() {
	local hugepages_target="$HUGEPAGES"
	if [ "$hugepages_target" = auto ]; then
		local mem_kib huge_kib
		mem_kib="$(awk '/^MemTotal:/ { print $2; exit }' /proc/meminfo)"
		huge_kib="$(awk '/^Hugepagesize:/ { print $2; exit }' /proc/meminfo)"
		[ -n "$mem_kib" ] && [ -n "$huge_kib" ] || {
			warn "cannot size the default HugeTLB pool from /proc/meminfo"
			hugepages_target=0
		}
		if [ "$hugepages_target" != 0 ]; then
			hugepages_target=$((mem_kib / 16 / huge_kib))
			[ "$hugepages_target" -ge 128 ] || hugepages_target=128
			[ "$hugepages_target" -le 8192 ] || hugepages_target=8192
		fi
	fi
	case "$hugepages_target" in
		''|*[!0-9]*)
			printf 'invalid hugepage count: %s\n' "$hugepages_target" >&2
			exit 2
			;;
	esac

	log "applying benchmark host sysctls"
	run_sudo sysctl -w \
		net.core.rmem_max=134217728 \
		net.core.wmem_max=134217728 \
		net.ipv4.tcp_rmem='4096 87380 134217728' \
		net.ipv4.tcp_wmem='4096 65536 134217728' \
		net.core.netdev_max_backlog=250000 \
		net.ipv4.tcp_no_metrics_save=1 >/dev/null || \
		warn "some TCP sysctls failed"

	if sysctl net.ipv4.tcp_available_congestion_control 2>/dev/null | grep -qw bbr; then
		run_sudo sysctl -w net.ipv4.tcp_congestion_control=bbr >/dev/null || true
	fi

	if [ "$hugepages_target" -gt 0 ]; then
		log "setting vm.nr_hugepages=$hugepages_target (requested=$HUGEPAGES)"
		printf 'vm.nr_hugepages = %s\n' "$hugepages_target" |
			run_sudo tee /etc/sysctl.d/99-zcutils-hugepages.conf >/dev/null
		run_sudo sysctl -w "vm.nr_hugepages=$hugepages_target" >/dev/null || \
			warn "hugepage allocation failed"
	fi

	log "raising shell memlock limit where permitted"
	ulimit -l unlimited 2>/dev/null || warn "current shell memlock is still limited"
	if [ "$(id -u)" -eq 0 ] || command -v sudo >/dev/null 2>&1; then
		printf '%s\n' '* soft memlock unlimited' '* hard memlock unlimited' |
			run_sudo tee /etc/security/limits.d/99-zcutils-memlock.conf >/dev/null || true
	fi
}

install_efa_stack() {
	log "installing AWS EFA userspace stack"
	local tmp
	tmp="$(mktemp -d)"
	(
		cd "$tmp"
		curl -fsSLO https://efa-installer.amazonaws.com/aws-efa-installer-1.48.0.tar.gz
		tar -xf aws-efa-installer-1.48.0.tar.gz
		cd aws-efa-installer
		run_sudo ./efa_installer.sh -y --skip-kmod --skip-mpi --skip-plugin --no-verify
	)
	rm -rf "$tmp"
	if command -v fi_info >/dev/null 2>&1; then
		fi_info -p efa -t FI_EP_RDM | sed -n '1,80p' || true
	fi
}

build_release_bins() {
	ensure_rust
	local bins="${ZCUTILS_BOOTSTRAP_BINS:-zcutils zcblockbench zcnblk-shm-target zcnblk-fan zcnblk-wal-leaf zcfanout-logshm-bench zcfanout-logtcp-bench}"
	local cargo_args=(build --release)
	local bin
	for bin in $bins; do
		cargo_args+=(--bin "$bin")
	done
	log "building release binaries: $bins"
	(
		cd "$REPO_DIR"
		cargo "${cargo_args[@]}"
	)
}

imds_value() {
	local path="$1"
	local token
	token="$(curl -fsS --connect-timeout 1 -X PUT \
		-H 'X-aws-ec2-metadata-token-ttl-seconds: 60' \
		http://169.254.169.254/latest/api/token 2>/dev/null || true)"
	[ -n "$token" ] || return 0
	curl -fsS --connect-timeout 1 \
		-H "X-aws-ec2-metadata-token: $token" \
		"http://169.254.169.254/latest/meta-data/$path" 2>/dev/null || true
}

gce_metadata_value() {
	local path="$1"
	curl -fsS --connect-timeout 1 \
		-H 'Metadata-Flavor: Google' \
		"http://metadata.google.internal/computeMetadata/v1/instance/$path" \
		2>/dev/null || true
}

metadata_basename() {
	local value="$1"
	printf '%s\n' "${value##*/}"
}

write_bootstrap_manifest() {
	local tmp git_head cloud_provider instance_id instance_type az project_id
	mkdir -p "$(dirname "$MANIFEST_PATH")"
	tmp="${MANIFEST_PATH}.tmp.$$"
	git_head="$(git -C "$REPO_DIR" rev-parse HEAD 2>/dev/null || printf dirty-or-unversioned)"
	instance_id="$(imds_value instance-id)"
	instance_type="$(imds_value instance-type)"
	az="$(imds_value placement/availability-zone)"
	project_id=
	if [ -n "$instance_id" ]; then
		cloud_provider=ec2
	else
		instance_id="$(gce_metadata_value id)"
		instance_type="$(metadata_basename "$(gce_metadata_value machine-type)")"
		az="$(metadata_basename "$(gce_metadata_value zone)")"
		project_id="$(curl -fsS --connect-timeout 1 \
			-H 'Metadata-Flavor: Google' \
			http://metadata.google.internal/computeMetadata/v1/project/project-id \
			2>/dev/null || true)"
		if [ -n "$instance_id" ]; then
			cloud_provider=gce
		else
			cloud_provider=unknown
		fi
	fi
	{
		printf 'manifest_version=2\n'
		printf 'bootstrapped_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
		printf 'host=%s\n' "$(hostname)"
		printf 'kernel=%s\n' "$(uname -r)"
		printf 'architecture=%s\n' "$(uname -m)"
		printf 'rustc=%s\n' "$(rustc -V 2>/dev/null | tr ' ' '_' || printf unavailable)"
		printf 'cargo=%s\n' "$(cargo -V 2>/dev/null | tr ' ' '_' || printf unavailable)"
		printf 'git_head=%s\n' "$git_head"
		printf 'cloud_provider=%s\n' "$cloud_provider"
		printf 'cloud_project=%s\n' "${project_id:-unknown}"
		printf 'instance_id=%s\n' "${instance_id:-unknown}"
		printf 'instance_type=%s\n' "${instance_type:-unknown}"
		printf 'availability_zone=%s\n' "${az:-unknown}"
		printf 'online_cpus=%s\n' "$(nproc)"
		printf 'numa_nodes=%s\n' "$(find /sys/devices/system/node -maxdepth 1 -type d -name 'node[0-9]*' | wc -l)"
		printf 'hugepages=%s\n' "$(cat /proc/sys/vm/nr_hugepages 2>/dev/null || printf 0)"
		printf 'memlock_kib=%s\n' "$(ulimit -l || true)"
		printf 'coordination_scope=dedicated-adhoc-instance\n'
		printf 'coordination_honored=true\n'
		printf 'cloud_daily_budget_usd=%s\n' "${ZCUTILS_CLOUD_DAILY_BUDGET_USD:-20}"
		printf 'bulk_traffic_policy=private-ip-same-zone-only\n'
		printf 'public_ip_policy=control-only-one-per-node\n'
		for iface in /sys/class/net/*; do
			iface="${iface##*/}"
			[ "$iface" = lo ] && continue
			printf 'nic_%s_numa=%s\n' "$iface" \
				"$(cat "/sys/class/net/$iface/device/numa_node" 2>/dev/null || printf unknown)"
		done
	} >"$tmp"
	mv "$tmp" "$MANIFEST_PATH"
	log "bootstrap manifest: $MANIFEST_PATH"
}

print_topology() {
	log "host topology snapshot"
	uname -a || true
	lscpu | sed -n '1,35p' || true
	printf '\n'
	ip -br addr || true
	printf '\n'
	for iface in /sys/class/net/*; do
		iface="${iface##*/}"
		[ "$iface" = lo ] && continue
		printf 'interface=%s numa=%s queues=%s\n' \
			"$iface" \
			"$(cat "/sys/class/net/$iface/device/numa_node" 2>/dev/null || printf '?')" \
			"$(find "/sys/class/net/$iface/queues" -maxdepth 1 -type d 2>/dev/null | wc -l)"
		ethtool -i "$iface" 2>/dev/null | sed 's/^/  /' || true
	done
	printf '\n'
	if command -v fi_info >/dev/null 2>&1; then
		fi_info -p efa -t FI_EP_RDM | sed -n '1,80p' || true
	fi
}

benchmark_warnings() {
	local memlock
	memlock="$(ulimit -l || true)"
	if [ "$memlock" != "unlimited" ]; then
		warn "memlock is $memlock; high-IOPS zero-copy and RDMA runs are not representative"
	fi
	if [ "$(cat /proc/sys/vm/nr_hugepages 2>/dev/null || printf 0)" = "0" ]; then
		warn "vm.nr_hugepages is 0; pass --hugepages N for hugepage-backed runs"
	fi
	warn "record lane-to-worker and lane-to-CPU mappings for every benchmark"
	warn "pin hot workers/kthreads and verify route/NIC affinity before trusting IOPS numbers"
}

while [ "$#" -gt 0 ]; do
	case "$1" in
		--no-apt)
			INSTALL_APT=0
			;;
		--no-build)
			BUILD_RELEASE=0
			;;
		--hugepages)
			shift
			HUGEPAGES="${1:?missing hugepage count}"
			;;
		--no-hugepages)
			HUGEPAGES=0
			;;
		--install-efa)
			INSTALL_EFA=1
			;;
		--no-tcp-tune)
			TCP_TUNE=0
			;;
		--manifest)
			shift
			MANIFEST_PATH="${1:?missing manifest path}"
			;;
		-h|--help)
			usage
			exit 0
			;;
		*)
			printf 'unknown option: %s\n' "$1" >&2
			usage >&2
			exit 2
			;;
	esac
	shift
done

if [ -z "$HUGEPAGES" ]; then
	printf 'choose HugeTLB policy explicitly: --hugepages N, --hugepages auto, or --no-hugepages\n' >&2
	usage >&2
	exit 2
fi

if [ "$INSTALL_APT" -eq 1 ]; then
	install_apt_packages
fi
if [ "$TCP_TUNE" -eq 1 ]; then
	tune_system
fi
if [ "$INSTALL_EFA" -eq 1 ]; then
	install_efa_stack
fi
if [ "$BUILD_RELEASE" -eq 1 ]; then
	build_release_bins
else
	ensure_rust
fi
print_topology
benchmark_warnings
write_bootstrap_manifest
log "zcutils adhoc bootstrap complete"
