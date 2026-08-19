#!/usr/bin/env bash
set -Eeuo pipefail
PATH="/usr/sbin:/sbin:$PATH"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK_DIR="${OKD_SNO_WORK_DIR:-$ROOT/.local/okd-sno-qemu}"
# Newer multi-architecture OKD/SCOS agent payloads currently register the
# literal architecture "multi", which assisted-service rejects. Keep the
# local SCC/CSI lab on the last FCOS-based x86_64 release until that upstream
# regression is fixed.
OKD_VERSION="${OKD_VERSION:-4.15.0-0.okd-2024-03-10-010116}"
VM_CPUS="${OKD_SNO_CPUS:-8}"
VM_MEMORY_MIB="${OKD_SNO_MEMORY_MIB:-24576}"
VM_DISK_GIB="${OKD_SNO_DISK_GIB:-130}"
VM_MAC="${OKD_SNO_MAC:-52:54:00:53:4e:4f}"
VM_IP="${OKD_SNO_IP:-192.168.126.10}"
BRIDGE_IP="${OKD_SNO_BRIDGE_IP:-192.168.126.1}"
PREFIX="${OKD_SNO_PREFIX:-24}"
BRIDGE="${OKD_SNO_BRIDGE:-zcsno0}"
TAP="${OKD_SNO_TAP:-zcsnotap0}"
NFT_TABLE="${OKD_SNO_NFT_TABLE:-zc_okd_sno}"
CLUSTER_NAME="${OKD_SNO_CLUSTER_NAME:-sno}"
BASE_DOMAIN="${OKD_SNO_BASE_DOMAIN:-okd.test}"
NODE_NAME="${OKD_SNO_NODE_NAME:-master-0}"
MIN_FREE_GIB="${OKD_SNO_MIN_FREE_GIB:-45}"
UPLINK="${OKD_SNO_UPLINK:-$(ip route show default | awk 'NR == 1 {print $5}')}"
CLUSTER_DOMAIN="$CLUSTER_NAME.$BASE_DOMAIN"
API_NAME="api.$CLUSTER_DOMAIN"

INSTALLER_URL="https://github.com/okd-project/okd/releases/download/$OKD_VERSION/openshift-install-linux-$OKD_VERSION.tar.gz"
CLIENT_URL="https://github.com/okd-project/okd/releases/download/$OKD_VERSION/openshift-client-linux-$OKD_VERSION.tar.gz"
TOOLS_DIR="$WORK_DIR/tools"
ASSET_DIR="$WORK_DIR/install"
STATE_DIR="$WORK_DIR/state"
LOG_DIR="$WORK_DIR/logs"
DISK="$STATE_DIR/okd-sno.qcow2"
ISO="$ASSET_DIR/agent.x86_64.iso"
QEMU_PID="$STATE_DIR/qemu.pid"
DNSMASQ_PID="$STATE_DIR/dnsmasq.pid"
QMP_SOCKET="$STATE_DIR/qmp.sock"

die() { printf 'okd-sno-qemu: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing command '$1'"; }

verified_pid() {
	local pidfile="$1" marker="$2" pid cmdline
	[ -s "$pidfile" ] || return 1
	pid="$(cat "$pidfile")"
	case "$pid" in ''|*[!0-9]*) return 1 ;; esac
	[ -r "/proc/$pid/cmdline" ] || return 1
	cmdline="$(tr '\0' ' ' < "/proc/$pid/cmdline")"
	case "$cmdline" in *"$marker"*) printf '%s\n' "$pid" ;; *) return 1 ;; esac
}

download_tools() {
	local installer_archive="$TOOLS_DIR/openshift-install.tar.gz"
	local client_archive="$TOOLS_DIR/openshift-client.tar.gz"
	mkdir -p "$TOOLS_DIR"
	if [ ! -x "$TOOLS_DIR/openshift-install" ]; then
		curl -fL --retry 4 --continue-at - -o "$installer_archive" "$INSTALLER_URL"
		tar -xzf "$installer_archive" -C "$TOOLS_DIR" openshift-install
	fi
	if [ ! -x "$TOOLS_DIR/oc" ]; then
		curl -fL --retry 4 --continue-at - -o "$client_archive" "$CLIENT_URL"
		tar -xzf "$client_archive" -C "$TOOLS_DIR" oc kubectl
	fi
}

check_capacity() {
	local free_kib min_kib
	free_kib="$(df -Pk "$WORK_DIR" | awk 'NR == 2 {print $4}')"
	min_kib=$((MIN_FREE_GIB * 1024 * 1024))
	[ "$free_kib" -ge "$min_kib" ] || die "only $((free_kib / 1024 / 1024)) GiB free; require at least $MIN_FREE_GIB GiB"
	[ "$VM_CPUS" -ge 8 ] || die "SNO requires at least 8 vCPUs"
	[ "$VM_MEMORY_MIB" -ge 16384 ] || die "SNO requires at least 16384 MiB RAM"
	[ "$VM_DISK_GIB" -ge 120 ] || die "SNO requires at least a 120 GiB installation disk"
}

generate_assets() {
	local ssh_key
	mkdir -p "$ASSET_DIR" "$STATE_DIR" "$LOG_DIR"
	if [ ! -s "$STATE_DIR/id_ed25519" ]; then
		ssh-keygen -q -t ed25519 -N '' -C okd-sno-qemu -f "$STATE_DIR/id_ed25519"
	fi
	ssh_key="$(cat "$STATE_DIR/id_ed25519.pub")"
	if [ ! -s "$ASSET_DIR/install-config.yaml" ]; then
		cat > "$ASSET_DIR/install-config.yaml" <<EOF
apiVersion: v1
baseDomain: $BASE_DOMAIN
metadata:
  name: $CLUSTER_NAME
compute:
- name: worker
  replicas: 0
controlPlane:
  name: master
  replicas: 1
  architecture: amd64
  hyperthreading: Enabled
networking:
  networkType: OVNKubernetes
  clusterNetwork:
  - cidr: 10.128.0.0/14
    hostPrefix: 23
  serviceNetwork:
  - 172.30.0.0/16
  machineNetwork:
  - cidr: 192.168.126.0/24
platform:
  none: {}
# Deliberately non-secret placeholder: base64("dummy:dummy").
pullSecret: '{"auths":{"fake.invalid":{"auth":"ZHVtbXk6ZHVtbXk="}}}'
sshKey: '$ssh_key'
EOF
	fi
	if [ ! -s "$ASSET_DIR/agent-config.yaml" ]; then
		cat > "$ASSET_DIR/agent-config.yaml" <<EOF
apiVersion: v1beta1
kind: AgentConfig
metadata:
  name: $CLUSTER_NAME
rendezvousIP: $VM_IP
hosts:
- hostname: $NODE_NAME
  role: master
  interfaces:
  - name: enp1s0
    macAddress: $VM_MAC
  rootDeviceHints:
    deviceName: /dev/vda
EOF
	fi
	if [ ! -s "$ISO" ]; then
		PATH="$TOOLS_DIR:$PATH" "$TOOLS_DIR/openshift-install" --dir "$ASSET_DIR" agent create image --log-level=info 2>&1 | tee "$LOG_DIR/create-image.log"
	fi
	[ -s "$ISO" ] || die "installer did not create $ISO"
	if [ ! -s "$DISK" ]; then
		qemu-img create -f qcow2 -o "preallocation=metadata,lazy_refcounts=on" "$DISK" "${VM_DISK_GIB}G"
	fi
}

network_up() {
	local dns_pid
	if ! ip link show "$BRIDGE" >/dev/null 2>&1; then
		sudo -n ip link add "$BRIDGE" type bridge
		sudo -n ip addr add "$BRIDGE_IP/$PREFIX" dev "$BRIDGE"
		sudo -n ip link set "$BRIDGE" up
	fi
	if ! ip link show "$TAP" >/dev/null 2>&1; then
		sudo -n ip tuntap add dev "$TAP" mode tap user "$(id -un)"
		sudo -n ip link set "$TAP" master "$BRIDGE"
		sudo -n ip link set "$TAP" up
	fi
	if ! sudo -n nft list table ip "$NFT_TABLE" >/dev/null 2>&1; then
		sudo -n nft add table ip "$NFT_TABLE"
		sudo -n nft "add chain ip $NFT_TABLE postrouting { type nat hook postrouting priority srcnat; policy accept; }"
		sudo -n nft add rule ip "$NFT_TABLE" postrouting ip saddr "192.168.126.0/24" oifname "$UPLINK" masquerade
	fi
	if ! dns_pid="$(verified_pid "$DNSMASQ_PID" "dnsmasq --pid-file=$DNSMASQ_PID")"; then
		rm -f "$DNSMASQ_PID"
		sudo -n dnsmasq \
			--pid-file="$DNSMASQ_PID" \
			--interface="$BRIDGE" --bind-interfaces \
			--dhcp-range="$VM_IP,$VM_IP,255.255.255.0,12h" \
			--dhcp-host="$VM_MAC,$VM_IP,$NODE_NAME,infinite" \
			--dhcp-option="option:router,$BRIDGE_IP" \
			--dhcp-option="option:dns-server,$BRIDGE_IP" \
			--address="/$API_NAME/$VM_IP" \
			--address="/api-int.$CLUSTER_DOMAIN/$VM_IP" \
			--address="/.apps.$CLUSTER_DOMAIN/$VM_IP" \
			--log-facility="$LOG_DIR/dnsmasq.log"
		dns_pid="$(verified_pid "$DNSMASQ_PID" "dnsmasq --pid-file=$DNSMASQ_PID")" || die "dnsmasq failed to stay running"
	fi
}

network_down() {
	local pid
	if pid="$(verified_pid "$DNSMASQ_PID" "dnsmasq --pid-file=$DNSMASQ_PID")"; then
		sudo -n kill -TERM "$pid"
		for _ in $(seq 1 50); do [ ! -e "/proc/$pid" ] && break; sleep .1; done
	fi
	rm -f "$DNSMASQ_PID"
	sudo -n nft delete table ip "$NFT_TABLE" >/dev/null 2>&1 || true
	sudo -n ip link delete "$TAP" >/dev/null 2>&1 || true
	sudo -n ip link delete "$BRIDGE" >/dev/null 2>&1 || true
}

start_vm() {
	local pid
	if pid="$(verified_pid "$QEMU_PID" "okd-sno-qemu")"; then
		network_up
		printf 'OKD SNO VM already running (pid=%s)\n' "$pid"
		return
	fi
	rm -f "$QEMU_PID" "$QMP_SOCKET"
	network_up
	qemu-system-x86_64 \
		-name guest=okd-sno-qemu,debug-threads=on \
		-machine q35,accel=kvm -cpu host \
		-smp "$VM_CPUS" -m "$VM_MEMORY_MIB" \
		-nodefaults -display none -monitor none \
		-serial "file:$LOG_DIR/console.log" \
		-qmp "unix:$QMP_SOCKET,server=on,wait=off" \
		-pidfile "$QEMU_PID" -daemonize \
		-device virtio-rng-pci \
		-device virtio-net-pci,netdev=cluster,mac="$VM_MAC" \
		-netdev tap,id=cluster,ifname="$TAP",script=no,downscript=no,vhost=off \
		-drive file="$DISK",if=none,id=osdisk,format=qcow2,cache=none,aio=io_uring,discard=unmap \
		-device virtio-blk-pci,drive=osdisk,bootindex=1 \
		-drive file="$ISO",if=none,id=agentiso,media=cdrom,format=raw,readonly=on \
		-device ide-cd,drive=agentiso,bootindex=2 \
		-boot once=d,menu=off
	pid="$(verified_pid "$QEMU_PID" "okd-sno-qemu")" || die "QEMU failed to start; inspect $LOG_DIR/console.log"
	printf 'OKD SNO VM started (pid=%s, ip=%s)\n' "$pid" "$VM_IP"
}

stop_vm() {
	local pid
	if pid="$(verified_pid "$QEMU_PID" "okd-sno-qemu")"; then
		if [ -S "$QMP_SOCKET" ] && command -v socat >/dev/null 2>&1; then
			printf '%s\n%s\n' '{"execute":"qmp_capabilities"}' '{"execute":"system_powerdown"}' | socat - "UNIX-CONNECT:$QMP_SOCKET" >/dev/null || true
			for _ in $(seq 1 300); do [ ! -e "/proc/$pid" ] && break; sleep .1; done
		fi
		if [ -e "/proc/$pid" ]; then
			kill -TERM "$pid"
			for _ in $(seq 1 100); do [ ! -e "/proc/$pid" ] && break; sleep .1; done
		fi
		[ ! -e "/proc/$pid" ] || die "QEMU pid $pid did not stop"
	fi
	rm -f "$QEMU_PID" "$QMP_SOCKET"
	network_down
}

make_local_kubeconfig() {
	local source="$ASSET_DIR/auth/kubeconfig" destination="$WORK_DIR/kubeconfig-local" cluster
	[ -s "$source" ] || return 0
	cp "$source" "$destination"
	cluster="$(KUBECONFIG="$destination" "$TOOLS_DIR/oc" config view -o jsonpath='{.contexts[?(@.name=="admin")].context.cluster}')"
	[ -n "$cluster" ] || cluster="$(KUBECONFIG="$destination" "$TOOLS_DIR/oc" config view -o jsonpath='{.clusters[0].name}')"
	KUBECONFIG="$destination" "$TOOLS_DIR/oc" config set-cluster "$cluster" \
		--server="https://$VM_IP:6443" --tls-server-name="$API_NAME" >/dev/null
}

show_status() {
	local pid
	printf 'work_dir=%s\n' "$WORK_DIR"
	printf 'cluster_api=https://%s:6443 vm_ip=%s\n' "$API_NAME" "$VM_IP"
	if pid="$(verified_pid "$QEMU_PID" "okd-sno-qemu")"; then printf 'vm=running pid=%s\n' "$pid"; else printf 'vm=stopped\n'; fi
	if [ -s "$ASSET_DIR/auth/kubeconfig" ]; then
		make_local_kubeconfig
		KUBECONFIG="$WORK_DIR/kubeconfig-local" "$TOOLS_DIR/oc" get nodes -o wide || true
		KUBECONFIG="$WORK_DIR/kubeconfig-local" "$TOOLS_DIR/oc" get clusteroperators || true
	else
		printf 'install=bootstrapping kubeconfig=not-yet-created\n'
	fi
	printf 'console_log=%s\n' "$LOG_DIR/console.log"
}

wait_ready() {
	local deadline=$((SECONDS + ${OKD_SNO_WAIT_SECONDS:-5400}))
	printf 'Waiting up to %s seconds for the API and Ready node...\n' "${OKD_SNO_WAIT_SECONDS:-5400}"
	while [ "$SECONDS" -lt "$deadline" ]; do
		if [ -s "$ASSET_DIR/auth/kubeconfig" ]; then
			make_local_kubeconfig
			if KUBECONFIG="$WORK_DIR/kubeconfig-local" "$TOOLS_DIR/oc" wait node/"$NODE_NAME" --for=condition=Ready --timeout=15s >/dev/null 2>&1; then
				KUBECONFIG="$WORK_DIR/kubeconfig-local" "$TOOLS_DIR/oc" get nodes -o wide
				printf 'OKD_SNO_QEMU_READY kubeconfig=%s\n' "$WORK_DIR/kubeconfig-local"
				return 0
			fi
		fi
		sleep 15
	done
	die "cluster did not become Ready before timeout; inspect $LOG_DIR/console.log and run '$0 status'"
}

prepare() {
	need curl; need tar; need qemu-img; need qemu-system-x86_64; need ssh-keygen; need dnsmasq; need nft; need ip
	[ -c /dev/kvm ] || die "/dev/kvm is unavailable"
	sudo -n true || die "passwordless sudo is required for the isolated tap/bridge"
	mkdir -p "$WORK_DIR"
	check_capacity
	download_tools
	generate_assets
	printf 'OKD SNO assets ready: %s\n' "$WORK_DIR"
}

usage() {
	printf 'Usage: %s {prepare|start|install|wait|status|stop|destroy}\n' "$0" >&2
	printf '  install  prepare assets, start the QEMU VM, and wait for the node to become Ready\n' >&2
}

case "${1:-}" in
prepare) prepare ;;
start) prepare; start_vm ;;
install) prepare; start_vm; wait_ready ;;
wait) wait_ready ;;
status) show_status ;;
stop) stop_vm ;;
destroy)
	stop_vm
	case "$WORK_DIR" in "$ROOT/.local/okd-sno-qemu"|/var/tmp/zcutils-okd-sno) rm -rf -- "$WORK_DIR" ;; *) die "refusing to remove non-default WORK_DIR: $WORK_DIR" ;; esac
	;;
*) usage; exit 2 ;;
esac
