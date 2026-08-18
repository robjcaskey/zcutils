#!/bin/sh

set -eu

export PATH=/usr/bin:/bin:/usr/sbin:/sbin
export IBV_DRIVERS_PATH=/usr/lib/x86_64-linux-gnu/libibverbs
export FI_LOG_LEVEL=warn

IP=/usr/bin/ip
URING_PLAY=/uring-play
SIZE_MIB=64
LEAF_PORT=29000
ZCRX_PORT=28000
ZCRX_READY_PORT=28001
ZCRX_BYTES=65536
ZCNET_NS=zcnode

phase_net_exec()
{
	if [ "${phase:-}" = zcnet ]; then
		"$IP" netns exec "$ZCNET_NS" "$@"
	else
		"$@"
	fi
}

cmdline_value()
{
	key="$1"
	for arg in $(cat /proc/cmdline); do
		case "$arg" in
			"$key"=*)
				printf '%s\n' "${arg#*=}"
				return 0
				;;
		esac
	done
	return 1
}

finish()
{
	status=$?
	trap - EXIT HUP INT TERM
	printf 'ZCCUSAN_GUEST_FINAL phase=%s role=%s status=%s\n' \
		"${phase:-unknown}" "${role:-unknown}" "$status"
	/bin/busybox sync || true
	/bin/busybox poweroff -f || true
	sleep 5
	exit "$status"
}

load_module()
{
	name="$1"
	printf 'guest-module-load: name=%s\n' "$name"
	insmod "/modules/${name}.ko"
}

wait_for_eth0()
{
	for _ in $(seq 1 30); do
		[ -e /sys/class/net/eth0 ] && return 0
		sleep 1
	done
	return 1
}

wait_for_peer()
{
	peer="$1"
	for _ in $(seq 1 30); do
		if phase_net_exec ping -c 1 -W 1 "$peer" >/dev/null 2>&1; then
			printf 'guest-peer-ready: peer=%s\n' "$peer"
			return 0
		fi
		sleep 1
	done
	printf 'guest-peer-timeout: peer=%s\n' "$peer" >&2
	return 1
}

load_virtio_net()
{
	load_module failover
	load_module net_failover
	load_module virtio_net
	wait_for_eth0
	$IP link set lo up
	$IP link set eth0 mtu 1500
	$IP link set eth0 up
}

setup_softroce()
{
	case "$role" in
		target)
			local_ip=10.82.0.2
			peer_ip=10.82.0.1
			;;
		client)
			local_ip=10.82.0.1
			peer_ip=10.82.0.2
			;;
		*) return 1 ;;
	esac

	$IP addr add "${local_ip}/24" dev eth0
	wait_for_peer "$peer_ip"

	load_module configfs
	mkdir -p /sys/kernel/config
	mount -t configfs configfs /sys/kernel/config || true
	load_module ib_core
	load_module ib_uverbs_support
	load_module ib_uverbs
	load_module ib_cm
	load_module iw_cm
	load_module rdma_cm
	load_module rdma_ucm
	load_module udp_tunnel
	load_module ip6_udp_tunnel
	load_module rdma_rxe

	/usr/bin/rdma link add rxe0 type rxe netdev eth0
	for _ in $(seq 1 30); do
		[ -e /sys/class/infiniband/rxe0 ] && break
		sleep 1
	done
	[ -e /sys/class/infiniband/rxe0 ]
	/usr/bin/rdma link show
	/usr/bin/ibv_devinfo -v -d rxe0 > /ibv-devinfo.log
	cat /ibv-devinfo.log
	/usr/bin/fi_info -p verbs -t FI_EP_RDM > /fi-info.log
	grep -q 'provider: verbs' /fi-info.log
	head -80 /fi-info.log
	message_domain=$(sed -n 's/^[[:space:]]*domain: //p' /fi-info.log | head -1)
	[ -n "$message_domain" ]
	/usr/bin/fi_info -p ofi_rxd -d "$message_domain" -t FI_EP_RDM > /fi-rxd-info.log
	grep -q 'provider: verbs;ofi_rxd' /fi-rxd-info.log
	head -80 /fi-rxd-info.log
	printf 'SOFTROCE_PROVIDER_READY role=%s netdev=eth0 rdma_device=rxe0 discovered_domain=%s provider=verbs;ofi_rxd endpoint=FI_EP_RDM caps=FI_MSG\n' \
		"$role" "$message_domain"
}

setup_sockets_rma()
{
	case "$role" in
		target)
			local_ip=10.84.0.2
			peer_ip=10.84.0.1
			;;
		client)
			local_ip=10.84.0.1
			peer_ip=10.84.0.2
			;;
		*) return 1 ;;
	esac
	$IP addr add "${local_ip}/24" dev eth0
	wait_for_peer "$peer_ip"
}

run_softroce_rc_probe()
{
	gid_index=$(
		sed -n 's/.*GID\[ *\([0-9][0-9]*\)\].*::ffff:.*/\1/p' /ibv-devinfo.log |
			head -1
	)
	[ -n "$gid_index" ] || gid_index=1
	printf 'softroce-rc-probe: role=%s gid_index=%s messages=16 bytes=4096\n' "$role" "$gid_index"
	if [ "$role" = target ]; then
		/bin/timeout 60 /usr/bin/ibv_rc_pingpong -d rxe0 -g "$gid_index" \
			-p 18515 -n 16 -s 4096 > /ibv-rc.log 2>&1
	else
		probe_ok=0
		for attempt in $(seq 1 15); do
			if /bin/timeout 15 /usr/bin/ibv_rc_pingpong -d rxe0 -g "$gid_index" \
				-p 18515 -n 16 -s 4096 10.82.0.2 > /ibv-rc.log 2>&1; then
				probe_ok=1
				break
			fi
			printf 'softroce-rc-retry: attempt=%s\n' "$attempt"
			sleep 1
		done
		[ "$probe_ok" -eq 1 ]
	fi
	cat /ibv-rc.log
	printf 'SOFTROCE_VERBS_RC_PASS role=%s device=rxe0\n' "$role"
}

new_nsim_device()
{
	id="$1"
	if ! echo "$id 1 1" > /sys/bus/netdevsim/new_device 2>/dev/null; then
		echo "$id 1" > /sys/bus/netdevsim/new_device
	fi
	for _ in $(seq 1 30); do
		for path in "/sys/bus/netdevsim/devices/netdevsim${id}/net/"*; do
			if [ -e "$path" ]; then
				printf '%s\n' "${path##*/}"
				return 0
			fi
		done
		sleep 1
	done
	return 1
}

configure_nsim_fastpath()
{
	id="$1"
	dir="/sys/kernel/debug/netdevsim/netdevsim${id}/ports/0/fastpath"
	[ -d "$dir" ] || return 1
	echo 1024 > "$dir/rx_ring_size"
	echo 0 > "$dir/napi_delay_us"
	echo 1 > "$dir/rx_5tuple_hash"
	[ ! -e "$dir/rx_dport_hash" ] || echo 0 > "$dir/rx_dport_hash"
	[ ! -e "$dir/tx_5tuple_hash" ] || echo 0 > "$dir/tx_5tuple_hash"
}

setup_zcnet()
{
	case "$role" in
		target)
			local_ip=10.83.0.2
			peer_ip=10.83.0.1
			;;
		client)
			local_ip=10.83.0.1
			peer_ip=10.83.0.2
			;;
		*) return 1 ;;
	esac

	load_module psample
	load_module llc
	load_module stp
	load_module bridge
	load_module netdevsim

	node_if=$(new_nsim_device 1101)
	switch_if=$(new_nsim_device 1102)
	configure_nsim_fastpath 1101
	configure_nsim_fastpath 1102
	mkdir -p /run/netns
	$IP netns add "$ZCNET_NS"
	$IP link set "$node_if" netns "$ZCNET_NS"
	$IP netns exec "$ZCNET_NS" $IP link set "$node_if" name zcnet0
	$IP link set "$switch_if" name zcsw0
	$IP link add name zcbr0 type bridge
	$IP link set zcbr0 type bridge stp_state 0
	$IP link set eth0 master zcbr0
	$IP link set zcsw0 master zcbr0
	$IP link set zcbr0 up
	$IP link set eth0 up
	$IP link set zcsw0 up
	$IP netns exec "$ZCNET_NS" $IP link set lo up
	$IP netns exec "$ZCNET_NS" $IP link set zcnet0 up

	$IP netns exec "$ZCNET_NS" /usr/bin/ethtool -G zcnet0 tcp-data-split on
	$IP netns exec "$ZCNET_NS" /usr/bin/ethtool -K zcnet0 tcp-segmentation-offload off \
		generic-segmentation-offload off generic-receive-offload off || true
	/usr/bin/ethtool -K zcsw0 tcp-segmentation-offload off \
		generic-segmentation-offload off generic-receive-offload off || true
	exec 8<"/run/netns/$ZCNET_NS"
	exec 9</proc/self/ns/net
	node_idx=$($IP netns exec "$ZCNET_NS" cat /sys/class/net/zcnet0/ifindex)
	switch_idx=$(cat /sys/class/net/zcsw0/ifindex)
	echo "8:${node_idx} 9:${switch_idx}" > /sys/bus/netdevsim/link_device

	ZCRX_DFS=/sys/kernel/debug/netdevsim/netdevsim1101/ports/0/zcrx
	[ -d "$ZCRX_DFS" ]
	echo 0 > "$ZCRX_DFS/rx_payload_nocopy"
	$IP netns exec "$ZCNET_NS" $IP addr add "${local_ip}/24" dev zcnet0
	wait_for_peer "$peer_ip"
	phase_net_exec "$IP" route get "$peer_ip" | tee /zcnet-route.log
	grep -q 'dev zcnet0' /zcnet-route.log
	printf 'ZCNET_TOPOLOGY_READY role=%s namespace=%s route=%s:zcnet0-linked-zcsw0-bridged-eth0 peer=%s rx_payload_nocopy=0\n' \
		"$role" "$ZCNET_NS" "$local_ip" "$peer_ip"
}

run_zcrx_target_probe()
{
	before_bytes=$(cat "$ZCRX_DFS/rx_vdma_bytes")
	before_drops=$(cat "$ZCRX_DFS/rx_vdma_drops")
	phase_net_exec /bin/timeout 60 "$URING_PLAY" recv-zc-server zcnet0 0 "$ZCRX_PORT" \
		"$ZCRX_BYTES" 0x5a > /zcrx-server.log 2>&1 &
	server_pid=$!
	for _ in $(seq 1 30); do
		grep -q 'registered ZCRX' /zcrx-server.log 2>/dev/null && break
		[ -e "/proc/$server_pid" ] || {
			cat /zcrx-server.log
			return 1
		}
		sleep 1
	done
	grep -q 'registered ZCRX' /zcrx-server.log
	echo 1 > "$ZCRX_DFS/rx_netmem"
	printf 'ZCRX_READY\n' |
		phase_net_exec /bin/timeout 30 /bin/nc -l -p "$ZCRX_READY_PORT" > /zcrx-ready.log 2>&1
	printf 'ZCRX_STANDALONE_READY port=%s rx_netmem=1\n' "$ZCRX_READY_PORT"
	wait "$server_pid"
	cat /zcrx-server.log
	after_packets=$(cat "$ZCRX_DFS/rx_vdma_packets")
	after_bytes=$(cat "$ZCRX_DFS/rx_vdma_bytes")
	after_drops=$(cat "$ZCRX_DFS/rx_vdma_drops")
	alloc_fails=$(cat "$ZCRX_DFS/rx_vdma_alloc_fails")
	no_iov=$(cat "$ZCRX_DFS/rx_vdma_no_iov")
	copy_fails=$(cat "$ZCRX_DFS/rx_vdma_copy_fails")
	delta_bytes=$((after_bytes - before_bytes))
	delta_drops=$((after_drops - before_drops))
	printf 'ZCRX_STANDALONE_COUNTERS packets=%s vdma_bytes=%s vdma_bytes_delta=%s drops=%s drops_delta=%s alloc_fails=%s no_iov=%s copy_fails=%s\n' \
		"$after_packets" "$after_bytes" "$delta_bytes" "$after_drops" "$delta_drops" \
		"$alloc_fails" "$no_iov" "$copy_fails"
	[ "$after_packets" -gt 0 ]
	[ "$delta_bytes" -ge "$ZCRX_BYTES" ]
	[ "$delta_drops" -eq 0 ]
	[ "$alloc_fails" -eq 0 ]
	[ "$no_iov" -eq 0 ]
	[ "$copy_fails" -eq 0 ]
	printf 'ZCRX_STANDALONE_PASS packets=%s vdma_bytes_delta=%s drops_delta=%s alloc_fails=%s no_iov=%s copy_fails=%s payload_mode=virtual-dma-copy\n' \
		"$after_packets" "$delta_bytes" "$delta_drops" "$alloc_fails" "$no_iov" "$copy_fails"
	echo 0 > "$ZCRX_DFS/rx_netmem"
	printf 'ZCRX_STANDALONE_DISABLED rx_netmem=0 before_storage_tcp=true\n'
}

run_zcrx_client_probe()
{
	ready_ok=0
	for attempt in $(seq 1 30); do
		if ready=$(phase_net_exec /bin/timeout 2 /bin/nc 10.83.0.2 "$ZCRX_READY_PORT" 2>/dev/null) &&
			[ "$ready" = ZCRX_READY ]; then
			ready_ok=1
			break
		fi
		printf 'zcrx-ready-retry: attempt=%s\n' "$attempt"
		sleep 1
	done
	[ "$ready_ok" -eq 1 ]
	printf 'ZCRX_STANDALONE_READY_CONFIRMED port=%s\n' "$ZCRX_READY_PORT"

	send_ok=0
	for attempt in $(seq 1 20); do
		if phase_net_exec /bin/timeout 15 "$URING_PLAY" tcp-send 10.83.0.2 "$ZCRX_PORT" \
			"$ZCRX_BYTES" 0x5a > /zcrx-client.log 2>&1; then
			send_ok=1
			break
		fi
		printf 'zcrx-client-retry: attempt=%s\n' "$attempt"
		sleep 1
	done
	cat /zcrx-client.log
	[ "$send_ok" -eq 1 ]
	printf 'ZCRX_STANDALONE_SEND_PASS bytes=%s fill=0x5a\n' "$ZCRX_BYTES"
}

run_leaf()
{
	leaf_ip="$1"
	if [ "$phase" = softroce ]; then
		set +e
		URING_PLAY_OFI_DOMAIN=rxe0-dgram \
		URING_PLAY_OFI_CQ_SLEEP_NS=0 \
		URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT=ofi \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER=ofi_rxd \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT=rdm \
		URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
		URING_PLAY_PIN_CPU_LIST=1 \
		URING_PLAY_TOPOLOGY_STRICT=0 \
		/bin/timeout 150 "$URING_PLAY" zcnblk-wal-leaf "zcmem:${SIZE_MIB}M" \
			"$leaf_ip" "$LEAF_PORT" 1 1 4096 1 true blocking > /leaf.log 2>&1
		leaf_status=$?
		set -e
	elif [ "$phase" = socketsrma ]; then
		set +e
		URING_PLAY_OFI_DOMAIN=eth0 \
		URING_PLAY_OFI_CQ_SLEEP_NS=0 \
		URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT=ofi \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER=sockets \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT=rdm \
		URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS=1 \
		URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
		URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
		URING_PLAY_PIN_CPU_LIST=1 \
		URING_PLAY_TOPOLOGY_STRICT=0 \
		/bin/timeout 150 "$URING_PLAY" zcnblk-wal-leaf "zcmem:${SIZE_MIB}M" \
			"$leaf_ip" "$LEAF_PORT" 1 1 4096 1 true blocking > /leaf.log 2>&1
		leaf_status=$?
		set -e
	else
		set +e
		phase_net_exec /bin/env \
			URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT=tcp \
			URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
			URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC=1 \
			/bin/timeout 150 "$URING_PLAY" zcnblk-wal-leaf "zcmem:${SIZE_MIB}M" \
			"$leaf_ip" "$LEAF_PORT" 1 1 4096 1 true blocking > /leaf.log 2>&1
		leaf_status=$?
		set -e
	fi
	cat /leaf.log
	[ "$leaf_status" -eq 0 ]
	grep -q '^zcnblk-wal-leaf-summary:' /leaf.log
	case "$phase" in
		softroce) leaf_transport=ofi-verbs-rxd-message ;;
		socketsrma) leaf_transport=ofi-sockets-rma ;;
		*) leaf_transport=tcp-zcnet ;;
	esac
	printf 'ZCCUSAN_LEAF_PASS phase=%s transport=%s\n' "$phase" "$leaf_transport"
}

run_block_client()
{
	leaf_ip="$1"
	source_ip="$2"
	load_module aead
	printf 'guest-module-load: name=zcnblk_client_mod transport=shm lanes=1 size_mib=%s\n' "$SIZE_MIB"
	insmod /modules/zcnblk_client_mod.ko \
		transport=shm lanes=1 connections_per_lane=1 shard_count=1 \
		size_mib="$SIZE_MIB" logical_block_size=4096 max_frame_bytes=4096 \
		queues=1 queue_depth=128 pipeline_depth=128 shm_ring_entries=256 \
		shm_payload_entries=4096 shm_poll_us=1000 batch_depth=1 write_acks=1 \
		hctx_affinity=0 pin_threads=0
	for _ in $(seq 1 30); do
		[ -b /dev/zcnblk0 ] && [ -c /dev/zcnblk-shmctl ] && break
		sleep 1
	done
	[ -b /dev/zcnblk0 ] && [ -c /dev/zcnblk-shmctl ]

	pid_file=/run/zccusan-onramp.pid
	if [ "$phase" = softroce ]; then
		URING_PLAY_OFI_DOMAIN=rxe0-dgram \
		URING_PLAY_OFI_CQ_SLEEP_NS=0 \
		URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=ofi \
		URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER=ofi_rxd \
		URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT=rdm \
		URING_PLAY_ZCNBLK_SHM_LEAF_ADDR="${leaf_ip}:${LEAF_PORT}" \
		URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE="$pid_file" \
		URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH=64 \
		URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS=1 \
		URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE=blocking \
		URING_PLAY_TOPOLOGY_STRICT=0 \
		/zcnblk-shm-target /dev/zcnblk-shmctl wal-tcp 64 1 1000 1000 10000 \
			> /onramp.log 2>&1 &
	elif [ "$phase" = socketsrma ]; then
		URING_PLAY_OFI_DOMAIN=eth0 \
		URING_PLAY_OFI_CQ_SLEEP_NS=0 \
		URING_PLAY_OFI_SELECTIVE_COMPLETION=1 \
		URING_PLAY_OFI_RMA_READ_COMPLETION_STRIDE=65536 \
		URING_PLAY_OFI_RMA_DEFER_TAIL_COMPLETION=1 \
		URING_PLAY_OFI_RMA_READ_MORE=1 \
		URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=ofi \
		URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER=sockets \
		URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT=rdm \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS=1 \
		URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD=8 \
		URING_PLAY_ZCNBLK_SHM_LEAF_ADDR="${leaf_ip}:${LEAF_PORT}" \
		URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE="$pid_file" \
		URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH=64 \
		URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS=1 \
		URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE=blocking \
		URING_PLAY_TOPOLOGY_STRICT=0 \
		/zcnblk-shm-target /dev/zcnblk-shmctl wal-tcp 64 1 1000 1000 10000 \
			2>&1 | tee /onramp.log &
	else
		phase_net_exec /bin/env \
			URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT=tcp \
			URING_PLAY_ZCNBLK_SHM_LEAF_ADDR="${leaf_ip}:${LEAF_PORT}" \
			URING_PLAY_ZCNBLK_SHM_LEAF_SOURCE_ADDR="$source_ip" \
			URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE="$pid_file" \
			URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH=64 \
			URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS=1 \
			URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE=blocking \
			/zcnblk-shm-target /dev/zcnblk-shmctl wal-tcp 64 1 1000 1000 10000 \
			> /onramp.log 2>&1 &
	fi
	onramp_job_pid=$!
	for _ in $(seq 1 40); do
		[ -s "$pid_file" ] && grep -q '^zcnblk-shm-target:' /onramp.log 2>/dev/null && break
		[ -e "/proc/$onramp_job_pid" ] || {
			cat /onramp.log
			return 1
		}
		sleep 1
	done
	[ -s "$pid_file" ]
	grep -q '^zcnblk-shm-target:' /onramp.log

	if [ "$phase" = socketsrma ]; then
		stress_ops=$(cmdline_value zccusan_stress_ops || printf '10000\n')
		stress_timeout=$(cmdline_value zccusan_stress_timeout || printf '90\n')
		case "$stress_ops:$stress_timeout" in
			*[!0-9:]*|:*|*:|0:*|*:0) return 1 ;;
		esac
		printf 'sockets-rma-block-read-probe: blocks=8 bytes_per_block=4096\n'
		/bin/timeout 30 dd if=/dev/zcnblk0 of=/dev/null bs=4096 count=8
		printf 'sockets-rma-block-read-probe: PASS\n'
		printf 'sockets-rma-block-read-stress: ops=%s qd=8 timeout_seconds=%s\n' \
			"$stress_ops" "$stress_timeout"
		blockbench_pass=0
		URING_PLAY_TOPOLOGY_STRICT=0 \
		URING_PLAY_BLOCKBENCH_RING_STATS=1 \
		URING_PLAY_BLOCKBENCH_WAIT_MIN_COMPLETIONS=8 \
		/bin/timeout "$stress_timeout" /zcblockbench /dev/zcnblk0 \
			--engine uring-fixed --mode read --workers 1 \
			--ops-per-worker "$stress_ops" --bs 4096 --iodepth 8 \
			--region-bytes-per-worker 67108864 --ring-entries 32 \
			--buffer-mode small-pages --pin false 2>&1 | tee /blockbench.log
		cat /blockbench.log
		if grep -q "zcblockbench-result:.*total_ops=$stress_ops .*pipeline_per_worker=8" /blockbench.log; then
			blockbench_pass=1
		else
			printf 'sockets-rma-block-read-stress: FAIL ops=%s; preserving shutdown telemetry\n' \
				"$stress_ops" >&2
			onramp_pid=$(cat "$pid_file")
			for task in /proc/"$onramp_pid"/task/*; do
				printf 'sockets-rma-onramp-task: tid=%s comm=%s wchan=%s syscall=' \
					"${task##*/}" "$(cat "$task/comm")" "$(cat "$task/wchan")"
				cat "$task/syscall" 2>/dev/null || printf 'unavailable\n'
				cat "$task/stack" 2>/dev/null || true
			done
		fi
		printf 'sockets-rma-block-read-stress-kernel-state:\n'
		cat /sys/kernel/debug/zcnblk/state | tee /kernel-state.log
		kernel_state=$(grep '^conn=0 ' /kernel-state.log)
		req_prod=$(printf '%s\n' "$kernel_state" | sed -n 's/.* req_prod=\([0-9]*\) .*/\1/p')
		req_cons=$(printf '%s\n' "$kernel_state" | sed -n 's/.* req_cons=\([0-9]*\) .*/\1/p')
		comp_prod=$(printf '%s\n' "$kernel_state" | sed -n 's/.* comp_prod=\([0-9]*\) .*/\1/p')
		comp_cons=$(printf '%s\n' "$kernel_state" | sed -n 's/.* comp_cons=\([0-9]*\) .*/\1/p')
		kernel_state_pass=0
		case "$kernel_state" in
			*' failed=0 pending=0 inflight_count=0 inflight_slots=0 '*\
' req_used=0 '*' comp_ready=0 '*)
				if [ -n "$req_prod" ] && [ "$req_prod" = "$req_cons" ] &&
					[ -n "$comp_prod" ] && [ "$comp_prod" = "$comp_cons" ]; then
					kernel_state_pass=1
				fi
				;;
		esac
	else
		/bin/timeout 90 /zcnblk-order-smoke /dev/zcnblk0 8 > /order-smoke.log 2>&1
		cat /order-smoke.log
		grep -q 'zcnblk-order-smoke: PASS' /order-smoke.log
		grep -q 'sync_terminal_state=true' /order-smoke.log
	fi

	onramp_pid=$(cat "$pid_file")
	case "$onramp_pid" in ''|*[!0-9]*) return 1 ;; esac
	[ -r "/proc/$onramp_pid/comm" ]
	onramp_comm=$(cat "/proc/$onramp_pid/comm")
	printf 'onramp-cleanup-inspect: pid=%s comm=%s\n' "$onramp_pid" "$onramp_comm"
	[ "$onramp_comm" = zcnblk-shm-targ ]
	kill -INT "$onramp_pid"
	set +e
	wait "$onramp_job_pid"
	onramp_status=$?
	set -e
	cat /onramp.log
	[ "$onramp_status" -eq 0 ]
	grep -q '^zcnblk-shm-target-summary:' /onramp.log
	grep -q '^zcnblk-shm-target-remote-leaf-summary:' /onramp.log
	if [ "$phase" = softroce ]; then
		grep -q 'remote_transport=ofi' /onramp.log
	elif [ "$phase" = socketsrma ]; then
		grep -q 'remote_transport=ofi' /onramp.log
		grep -Eq 'read_posts=[1-9][0-9]* .*rma_read_forced_markers=[1-9][0-9]* .*rma_read_flush_posts=0' /onramp.log
		grep -Eq 'rma_read_marker_posts=[1-9][0-9]* rma_read_unsignaled_fast_posts=[1-9][0-9]*' /onramp.log
		grep -Eq 'rma_read_more_posts=[1-9][0-9]*' /onramp.log
		awk '
			/zcofi-endpoint-stats:/ {
				posts = markers = fast = -1
				for (i = 1; i <= NF; i++) {
					split($i, field, "=")
					if (field[1] == "read_posts") posts = field[2] + 0
					if (field[1] == "rma_read_marker_posts") markers = field[2] + 0
					if (field[1] == "rma_read_unsignaled_fast_posts") fast = field[2] + 0
				}
				if (posts > 0) {
					seen++
					if (markers <= 0 || fast <= 0 || markers + fast != posts) bad = 1
				}
			}
			END { exit seen == 0 || bad }
		' /onramp.log
		grep -q 'deferred_real_tail_marker=true synthetic_partial_flush_policy=fallback-only' /onramp.log
		[ "$blockbench_pass" -eq 1 ]
		[ "$kernel_state_pass" -eq 1 ]
	else
		grep -q 'remote_transport=tcp' /onramp.log
	fi
	rmmod zcnblk_client_mod
	case "$phase" in
		softroce) block_transport=ofi-verbs-rxd-message ;;
		socketsrma) block_transport=ofi-sockets-rma ;;
		*) block_transport=tcp-zcnet ;;
	esac
	if [ "$phase" = socketsrma ]; then
		printf 'ZCCUSAN_BLOCK_PATH_PASS phase=%s block_edge=/dev/zcnblk0 userspace_stage=zcnblk-shm-target placement_owner=leaf transport=%s probe_blocks=8 stress_ops=%s workers=1 lanes=1 per_worker_qd=8 aggregate_outstanding_depth=8 raw_transport_rtt=not-measured theoretical_iops_ceiling=not-computed actual_theoretical_efficiency=not-reported classification=correctness-only selective_completion=true deferred_real_tail=true synthetic_flushes=0\n' \
			"$phase" "$block_transport" "$stress_ops"
	else
		printf 'ZCCUSAN_BLOCK_PATH_PASS phase=%s block_edge=/dev/zcnblk0 userspace_stage=zcnblk-shm-target placement_owner=leaf transport=%s order_pairs=8 sync_terminal_state=true\n' \
			"$phase" "$block_transport"
	fi
}

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev || true
mount -t debugfs debugfs /sys/kernel/debug
mkdir -p /run /tmp /var/run

role=$(cmdline_value zccusan_role || printf unknown)
phase=$(cmdline_value zccusan_phase || printf unknown)
trap finish EXIT HUP INT TERM

printf 'ZCCUSAN_GUEST_BOOT phase=%s role=%s kernel=%s representative=false lanes=1 per_worker_qd=correctness-only aggregate_outstanding_depth=not-benchmarked\n' \
	"$phase" "$role" "$(uname -r)"
[ "$(uname -r)" = 7.2.0-rc1-io-slots-nvme ]
load_virtio_net

case "$phase:$role" in
	softroce:target)
		setup_softroce
		run_softroce_rc_probe
		run_leaf 10.82.0.2
		;;
	softroce:client)
		setup_softroce
		run_softroce_rc_probe
		sleep 2
		run_block_client 10.82.0.2 10.82.0.1
		;;
	socketsrma:target)
		setup_sockets_rma
		run_leaf 10.84.0.2
		;;
	socketsrma:client)
		setup_sockets_rma
		sleep 2
		run_block_client 10.84.0.2 10.84.0.1
		;;
	zcnet:target)
		setup_zcnet
		run_zcrx_target_probe
		run_leaf 10.83.0.2
		printf 'ZCNET_FINAL_COUNTERS rx_netmem_packets=%s rx_netmem_bytes=%s rx_vdma_packets=%s rx_vdma_bytes=%s rx_vdma_drops=%s rx_vdma_alloc_fails=%s rx_vdma_no_iov=%s rx_vdma_copy_fails=%s\n' \
			"$(cat "$ZCRX_DFS/rx_netmem_packets")" "$(cat "$ZCRX_DFS/rx_netmem_bytes")" \
			"$(cat "$ZCRX_DFS/rx_vdma_packets")" "$(cat "$ZCRX_DFS/rx_vdma_bytes")" \
			"$(cat "$ZCRX_DFS/rx_vdma_drops")" "$(cat "$ZCRX_DFS/rx_vdma_alloc_fails")" \
			"$(cat "$ZCRX_DFS/rx_vdma_no_iov")" "$(cat "$ZCRX_DFS/rx_vdma_copy_fails")"
		;;
	zcnet:client)
		setup_zcnet
		run_zcrx_client_probe
		sleep 3
		run_block_client 10.83.0.2 10.83.0.1
		;;
	*)
		printf 'invalid zccusan guest mode phase=%s role=%s\n' "$phase" "$role" >&2
		exit 2
		;;
esac

printf 'ZCCUSAN_PHASE_PASS phase=%s role=%s kernel=%s representative=false\n' \
	"$phase" "$role" "$(uname -r)"
