#!/bin/sh
set -eu

export PATH=/bin:/usr/bin:/sbin:/usr/sbin
export FI_PROVIDER=sockets
export FI_SOCKETS_IFACE=eth0
export FI_LOG_LEVEL=warn

cmdline_value()
{
	key="$1"
	for argument in $(cat /proc/cmdline); do
		case "$argument" in
			"$key"=*) printf '%s\n' "${argument#*=}"; return 0 ;;
		esac
	done
	return 1
}

uptime_us()
{
	awk '{ split($1, part, "."); fraction=substr(part[2] "000000", 1, 6); printf "%.0f\n", part[1] * 1000000 + fraction }' /proc/uptime
}

finish()
{
	status=$?
	trap - EXIT HUP INT TERM
	printf 'ZCVOLUME_SCALE_GUEST_FINAL role=%s status=%s\n' "${role:-unknown}" "$status"
	sync || true
	poweroff -f || true
	sleep 3
	exit "$status"
}

load_network()
{
	insmod /modules/failover.ko
	insmod /modules/net_failover.ko
	insmod /modules/virtio_net.ko
	count=0
	while [ ! -e /sys/class/net/eth0 ] && [ "$count" -lt 200 ]; do
		sleep 0.05
		count=$((count + 1))
	done
	[ -e /sys/class/net/eth0 ]
	ip link set lo up
	ip link set eth0 mtu 1500
	ip link set eth0 up
}

flow_env()
{
	cpu="$1"
	shift
	taskset -c "$cpu" env \
		URING_PLAY_ZCOFI_VIRTUAL_VOLUMES="$virtual_volumes" \
		URING_PLAY_OFI_DOMAIN=eth0 \
		URING_PLAY_OFI_ACK_WINDOW=64 \
		URING_PLAY_OFI_PREPOST_ACK=1 \
		URING_PLAY_OFI_ACK_INJECT=0 \
		URING_PLAY_OFI_CQ_SLEEP_NS=0 \
		URING_PLAY_OFI_BUSY_POLL_ITERS=1000 \
		URING_PLAY_OFI_TIMEOUT_MS=60000 \
		URING_PLAY_PIN_CPUS=1 \
		URING_PLAY_PIN_CPU_LIST="$cpu" \
		URING_PLAY_TOPOLOGY_STRICT=0 \
		/uring-play "$@"
}

run_receiver()
{
	cpu="$1"
	virtual_volumes="$2"
	port="$3"
	bytes="$4"
	flow_env "$cpu" zcwal-ofi-recv sockets rdm "$address" "$port" 1 "$bytes" 4096 1 true
}

run_sender()
{
	cpu="$1"
	virtual_volumes="$2"
	server_address="$3"
	port="$4"
	bytes="$5"
	flow_env "$cpu" zcwal-ofi-send sockets rdm "$server_address" "$port" 1 "$bytes" 4096 1 true
}

wait_until_epoch()
{
	wanted="$1"
	while [ "$(date +%s)" -lt "$wanted" ]; do
		sleep 0.05
	done
}

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev || true
mkdir -p /run /tmp
role=$(cmdline_value zcscale.role || printf unknown)
start_epoch=$(cmdline_value zcscale.start_epoch || printf 0)
repeats=$(cmdline_value zcscale.repeats || printf 3)
trap finish EXIT HUP INT TERM
load_network
ulimit -l unlimited 2>/dev/null || true

case "$role" in
	storage-[1-7])
		ordinal=${role#storage-}
		address="10.95.0.$((10 + ordinal))"
		ip address add "$address/24" dev eth0
		if [ "$ordinal" -le 6 ]; then local_volumes=143; else local_volumes=142; fi
		printf 'ZCVOLUME_SCALE_TOPOLOGY role=%s ip=%s guest_vcpus=2 cold_worker_cpu=0 hot_worker_cpu=1 local_volume_namespace=%s global_volume_rule="zero_based_server_plus_local_index_times_7" heavy_global_volume=%s transport=ofi-sockets-over-virtio-tcp terminal=remote-volatile-userspace-wal-hwm block_device=false placement_owner=userspace-scheduler representative=false\n' \
			"$role" "$address" "$local_volumes" "$((ordinal - 1))"
		rep=1
		while [ "$rep" -le "$repeats" ]; do
			cold_port=$((30000 + rep * 100))
			hot_port=$((cold_port + 10))
			run_receiver 0 "$local_volumes" "$cold_port" 16777216 >"/tmp/cold-$rep.log" 2>&1 &
			cold_pid=$!
			run_receiver 1 1 "$hot_port" 67108864 >"/tmp/hot-$rep.log" 2>&1 &
			hot_pid=$!
			cold_status=0
			hot_status=0
			wait "$cold_pid" || cold_status=$?
			wait "$hot_pid" || hot_status=$?
			cat "/tmp/cold-$rep.log" "/tmp/hot-$rep.log"
			[ "$cold_status" -eq 0 ] && [ "$hot_status" -eq 0 ]
			grep -q "volume_count=$local_volumes active_volumes=$local_volumes" "/tmp/cold-$rep.log"
			grep -q 'volume_count=1 active_volumes=1' "/tmp/hot-$rep.log"
			printf 'ZCVOLUME_SCALE_STORAGE_WAVE role=%s repeat=%s cold_volumes=%s hot_volumes=1 cold_ops=4096 hot_ops=16384 remote_application_acks=20480\n' \
				"$role" "$rep" "$local_volumes"
			rep=$((rep + 1))
		done
		printf 'ZCVOLUME_SCALE_INITIAL_PASS role=%s repeats=%s local_volumes=%s heavy_volumes=1\n' \
			"$role" "$repeats" "$local_volumes"
		;;
	storage-8)
		address=10.95.0.18
		ip address add "$address/24" dev eth0
		printf 'ZCVOLUME_SCALE_TOPOLOGY role=storage-8 ip=%s guest_vcpus=2 admitted_worker_cpu=0 transport=ofi-sockets-over-virtio-tcp terminal=remote-volatile-userspace-wal-hwm block_device=false placement_owner=userspace-scheduler representative=false\n' "$address"
		run_receiver 0 1 35000 4194304 >/tmp/admitted.log 2>&1 &
		receiver_pid=$!
		printf 'ZCVOLUME_SCALE_STORAGE8_READY ip=%s port=35000 volume=needs-storage-8\n' "$address"
		wait "$receiver_pid"
		cat /tmp/admitted.log
		grep -q 'volume_count=1 active_volumes=1' /tmp/admitted.log
		printf 'ZCVOLUME_SCALE_STORAGE8_DATA_PASS volume=needs-storage-8 ops=1024 completion=remote-application-ack\n'
		;;
	client-[0-2])
		client=${role#client-}
		address="10.95.0.$((21 + client))"
		ip address add "$address/24" dev eth0
		case "$client" in
			0) servers="1 4 7" ;;
			1) servers="2 5" ;;
			2) servers="3 6" ;;
		esac
		printf 'ZCVOLUME_SCALE_TOPOLOGY role=%s ip=%s guest_vcpus=2 assigned_storage_nodes="%s" per_flow_worker=lane0 worker_cpu="server_ordinal_mod_2" per_worker_qd=64 aggregate_client_outstanding=%s transport=ofi-sockets-over-virtio-tcp raw_transport_rtt=ping-reported-below theoretical_iops_ceiling=not_computed reason=shared-host-qemu-nonrepresentative block_device=false userspace_placement=true\n' \
			"$role" "$address" "$servers" "$(( $(echo "$servers" | wc -w) * 2 * 64 ))"
		first_server=$(echo "$servers" | awk '{print $1}')
		ping -c 10 -W 1 "10.95.0.$((10 + first_server))" || true
		rep=1
		while [ "$rep" -le "$repeats" ]; do
			wait_until_epoch $((start_epoch + (rep - 1) * 15))
			started_us=$(uptime_us)
			pids=""
			for server in $servers; do
				if [ "$server" -le 6 ]; then local_volumes=143; else local_volumes=142; fi
				cpu=$((server % 2))
				cold_port=$((30000 + rep * 100))
				hot_port=$((cold_port + 10))
				run_sender "$cpu" "$local_volumes" "10.95.0.$((10 + server))" "$cold_port" 16777216 >"/tmp/client-$client-server-$server-cold-$rep.log" 2>&1 &
				pids="$pids $!"
				run_sender "$cpu" 1 "10.95.0.$((10 + server))" "$hot_port" 67108864 >"/tmp/client-$client-server-$server-hot-$rep.log" 2>&1 &
				pids="$pids $!"
			done
			flow_status=0
			for pid in $pids; do wait "$pid" || flow_status=1; done
			ended_us=$(uptime_us)
			for server in $servers; do
				cat "/tmp/client-$client-server-$server-cold-$rep.log" "/tmp/client-$client-server-$server-hot-$rep.log"
			done
			[ "$flow_status" -eq 0 ]
			server_count=$(echo "$servers" | wc -w)
			ops=$((server_count * 20480))
			elapsed_us=$((ended_us - started_us))
			[ "$elapsed_us" -gt 0 ]
			printf 'ZCVOLUME_SCALE_CLIENT_WAVE role=%s repeat=%s server_count=%s cold_ops=%s hot_ops=%s total_ops=%s elapsed_us=%s hot_share_pct=80 completion=remote-application-ack\n' \
				"$role" "$rep" "$server_count" "$((server_count * 4096))" "$((server_count * 16384))" "$ops" "$elapsed_us"
			rep=$((rep + 1))
		done
		printf 'ZCVOLUME_SCALE_INITIAL_PASS role=%s repeats=%s assigned_storage_nodes="%s"\n' "$role" "$repeats" "$servers"
		if [ "$client" -eq 0 ]; then
			count=0
			while ! ping -c 1 -W 1 10.95.0.18 >/dev/null 2>&1 && [ "$count" -lt 2400 ]; do
				sleep 0.05
				count=$((count + 1))
			done
			[ "$count" -lt 2400 ]
			sleep 1
			run_sender 0 1 10.95.0.18 35000 4194304 >/tmp/admitted-client.log 2>&1
			cat /tmp/admitted-client.log
			grep -q 'volume_count=1 active_volumes=1' /tmp/admitted-client.log
			printf 'ZCVOLUME_SCALE_CLIENT8_DATA_PASS volume=needs-storage-8 ops=1024 selected_host=storage-8 request_mutated=false\n'
		fi
		;;
	*)
		printf 'unknown zcscale role %s\n' "$role" >&2
		exit 2
		;;
esac

printf 'ZCVOLUME_SCALE_GUEST_PASS role=%s kernel=%s\n' "$role" "$(uname -r)"
