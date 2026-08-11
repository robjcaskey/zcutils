#!/usr/bin/env bash
set -euo pipefail

ACTION="${1:-}"
NS="${ZCOFI_RXE_NETNS:-zcrxe-zcutils}"
CLIENT_LINK="${ZCOFI_RXE_CLIENT_LINK:-zcrxec0}"
TARGET_LINK="${ZCOFI_RXE_TARGET_LINK:-zcrxet0}"
CLIENT_DEVICE="${ZCOFI_RXE_CLIENT_DEVICE:-rxe_zc_c}"
TARGET_DEVICE="${ZCOFI_RXE_TARGET_DEVICE:-rxe_zc_t}"
CLIENT_ADDR="${ZCOFI_RXE_CLIENT_ADDR:-198.18.0.1}"
TARGET_ADDR="${ZCOFI_RXE_TARGET_ADDR:-198.18.0.2}"
PREFIX="${ZCOFI_RXE_PREFIX:-30}"
created_ns=0
created_link=0
created_client_rxe=0
created_target_rxe=0

die() {
	printf 'zcofi-softroce-netns: %s\n' "$*" >&2
	exit 1
}

rdma_exists() {
	local device="$1"
	rdma link show 2>/dev/null | grep -Eq "(^|[[:space:]])${device}/"
}

target_rdma_exists() {
	local device="$1"
	sudo -n ip netns exec "$NS" rdma link show 2>/dev/null | \
		grep -Eq "(^|[[:space:]])${device}/"
}

cleanup_partial() {
	set +e
	if [ "$created_target_rxe" = 1 ]; then
		sudo -n ip netns exec "$NS" rdma link delete "$TARGET_DEVICE"
	fi
	if [ "$created_client_rxe" = 1 ]; then
		sudo -n rdma link delete "$CLIENT_DEVICE"
	fi
	if [ "$created_link" = 1 ]; then
		sudo -n ip link delete "$CLIENT_LINK"
	fi
	if [ "$created_ns" = 1 ]; then
		sudo -n ip netns delete "$NS"
	fi
}

setup() {
	command -v rdma >/dev/null 2>&1 || die "rdma is required"
	command -v ip >/dev/null 2>&1 || die "ip is required"
	sudo -n true || die "non-interactive sudo is required"
	ip netns list | awk '{print $1}' | grep -Fxq "$NS" && die "namespace $NS already exists"
	ip link show dev "$CLIENT_LINK" >/dev/null 2>&1 && die "link $CLIENT_LINK already exists"
	ip link show dev "$TARGET_LINK" >/dev/null 2>&1 && die "link $TARGET_LINK already exists"
	rdma_exists "$CLIENT_DEVICE" && die "RDMA device $CLIENT_DEVICE already exists"

	sudo -n /usr/sbin/modprobe rdma_rxe
	trap cleanup_partial ERR INT TERM
	sudo -n ip netns add "$NS"
	created_ns=1
	sudo -n ip link add "$CLIENT_LINK" type veth peer name "$TARGET_LINK"
	created_link=1
	sudo -n ip link set "$TARGET_LINK" netns "$NS"
	sudo -n ip addr add "$CLIENT_ADDR/$PREFIX" dev "$CLIENT_LINK"
	sudo -n ip link set "$CLIENT_LINK" mtu 1500 up
	sudo -n ip netns exec "$NS" ip link set lo up
	sudo -n ip netns exec "$NS" ip addr add "$TARGET_ADDR/$PREFIX" dev "$TARGET_LINK"
	sudo -n ip netns exec "$NS" ip link set "$TARGET_LINK" mtu 1500 up
	sudo -n rdma link add "$CLIENT_DEVICE" type rxe netdev "$CLIENT_LINK"
	created_client_rxe=1
	sudo -n ip netns exec "$NS" rdma link add "$TARGET_DEVICE" type rxe netdev "$TARGET_LINK"
	created_target_rxe=1
	timeout 10 ping -c 1 -W 1 "$TARGET_ADDR" >/dev/null

	printf 'softroce_netns=ready namespace=%s client_link=%s client_ip=%s client_rdma=%s target_link=%s target_ip=%s target_rdma=%s mtu=1500\n' \
		"$NS" "$CLIENT_LINK" "$CLIENT_ADDR" "$CLIENT_DEVICE" \
		"$TARGET_LINK" "$TARGET_ADDR" "$TARGET_DEVICE"
	rdma link show | sed 's/^/client_/'
	sudo -n ip netns exec "$NS" rdma link show | sed 's/^/target_/'
	trap - ERR INT TERM
}

cleanup() {
	if ip netns list | awk '{print $1}' | grep -Fxq "$NS"; then
		mapfile -t pids < <(sudo -n ip netns pids "$NS")
		if [ "${#pids[@]}" -ne 0 ]; then
			for pid in "${pids[@]}"; do
				[ -r "/proc/$pid/comm" ] || continue
				printf 'softroce_cleanup_refused_pid=%s comm=%s\n' "$pid" "$(<"/proc/$pid/comm")" >&2
			done
			die "namespace $NS still contains processes"
		fi
		if target_rdma_exists "$TARGET_DEVICE"; then
			sudo -n ip netns exec "$NS" rdma link delete "$TARGET_DEVICE"
		fi
	fi
	if rdma_exists "$CLIENT_DEVICE"; then
		sudo -n rdma link delete "$CLIENT_DEVICE"
	fi
	if ip link show dev "$CLIENT_LINK" >/dev/null 2>&1; then
		sudo -n ip link delete "$CLIENT_LINK"
	fi
	if ip netns list | awk '{print $1}' | grep -Fxq "$NS"; then
		sudo -n ip netns delete "$NS"
	fi
	printf 'softroce_netns=clean namespace=%s client_link=%s client_rdma=%s target_rdma=%s\n' \
		"$NS" "$CLIENT_LINK" "$CLIENT_DEVICE" "$TARGET_DEVICE"
}

info() {
	printf 'softroce_namespace=%s client_link=%s client_ip=%s client_rdma=%s target_link=%s target_ip=%s target_rdma=%s\n' \
		"$NS" "$CLIENT_LINK" "$CLIENT_ADDR" "$CLIENT_DEVICE" \
		"$TARGET_LINK" "$TARGET_ADDR" "$TARGET_DEVICE"
	rdma link show
	sudo -n ip netns exec "$NS" rdma link show
}

case "$ACTION" in
	setup) setup ;;
	cleanup) cleanup ;;
	info) info ;;
	*) die "usage: $0 setup|cleanup|info" ;;
esac
