#!/bin/sh
set -u
export PATH=/bin:/sbin:/usr/bin:/usr/sbin:/usr/local/bin

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /sys/fs/cgroup /run /tmp /etc
mount -t cgroup2 none /sys/fs/cgroup || true
mount -t tmpfs tmpfs /run || true

role=""
for argument in $(cat /proc/cmdline); do
	case "$argument" in
		zccap.role=*) role="${argument#zccap.role=}" ;;
	esac
done

kubectl()
{
	/k3s kubectl "$@"
}

dump_cluster()
{
	kubectl get nodes -o wide 2>/dev/null || true
	kubectl get mediagrant,storageprofile,zcvolume -A -o wide 2>/dev/null || true
	kubectl get pods -A -o wide 2>/dev/null || true
	kubectl describe zcvolume -A 2>/dev/null || true
	for pod in $(kubectl get pods -A -o name 2>/dev/null); do
		echo "== logs $pod =="
		kubectl logs -n default "$pod" --all-containers 2>/dev/null || true
	done
}

fail()
{
	echo "ZCCUSAN_CAPACITY_QEMU_FAIL role=${role:-early} reason=$*"
	[ "${role:-}" != controller ] || dump_cluster
	for log in /tmp/k3s.log /tmp/operator.log /tmp/kubernetes.log; do
		[ ! -s "$log" ] || { echo "== $log =="; tail -300 "$log"; }
	done
	dmesg | tail -80
	if [ "${role:-}" = controller ]; then
		for ordinal in 1 2 3 4 5 6 7 8; do
			echo stop | nc "10.96.1.$((10 + ordinal))" 29990 2>/dev/null || true
		done
	fi
	poweroff -f
}

wait_path()
{
	path="$1"
	count=0
	while [ ! -e "$path" ] && [ "$count" -lt 600 ]; do
		sleep 0.05
		count=$((count + 1))
	done
	[ -e "$path" ]
}

stop_pid()
{
	pid="$1"
	[ -n "$pid" ] || return 0
	[ -e "/proc/$pid" ] || return 0
	kill -TERM "$pid" 2>/dev/null || true
	count=0
	while [ -e "/proc/$pid" ] && [ "$count" -lt 600 ]; do
		sleep 0.05
		count=$((count + 1))
	done
	[ ! -e "/proc/$pid" ]
}

if [ ! -e /.zccap-system-root ]; then
	insmod /modules/virtio_blk.ko || fail pivot-virtio-blk
	insmod /modules/crc16.ko || fail pivot-crc16
	insmod /modules/mbcache.ko || fail pivot-mbcache
	insmod /modules/jbd2.ko || fail pivot-jbd2
	insmod /modules/ext4.ko || fail pivot-ext4
	wait_path /dev/vda || fail pivot-system-device
	mkdir -p /newroot
	mount -t ext4 /dev/vda /newroot || fail pivot-system-mount
	mount --move /proc /newroot/proc || fail pivot-proc
	mount --move /sys /newroot/sys || fail pivot-sys
	mount --move /dev /newroot/dev || fail pivot-dev
	mount --move /run /newroot/run || fail pivot-run
	exec switch_root /newroot /init
	fail pivot-switch-root-returned
fi

insmod /modules/failover.ko || fail failover-module
insmod /modules/net_failover.ko || fail net-failover-module
insmod /modules/virtio_net.ko || fail virtio-net-module
insmod /modules/aead.ko || fail aead-module

case "$role" in
	controller)
		address=10.96.1.1
		machine_id=96cc0001000000000000000000000001
		;;
	storage-[1-8])
		ordinal=${role#storage-}
		address="10.96.1.$((10 + ordinal))"
		machine_id="96cc00$(printf '%02d' "$ordinal")0000000000000000000000$(printf '%02d' "$ordinal")"
		;;
	*) fail unknown-role ;;
esac
echo "$machine_id" >/etc/machine-id
hostname "$role" || fail hostname
ip link set lo up || fail loopback
ip link set eth0 up || fail link
ip address add "$address/24" dev eth0 || fail address

echo "ZCCUSAN_CAPACITY_QEMU_TOPOLOGY role=$role ip=$address transport=tcp-unicast placement=userspace-mirror block_placement=false node_capacity_bytes=8388608 node_capacity_iops=100 representative_benchmark=false"

if [ "$role" = controller ]; then
	/k3s server \
		--cluster-init --disable-agent --snapshotter=native \
		--bind-address=10.96.1.1 --advertise-address=10.96.1.1 --node-ip=10.96.1.1 \
		--tls-san=10.96.1.1 --token=zccusan-capacity-qemu-token \
		--disable=traefik --disable=coredns --disable=metrics-server \
		--disable=local-storage --disable=servicelb --disable=network-policy \
		--disable-kube-proxy --flannel-backend=none --write-kubeconfig-mode=0600 \
		>/tmp/k3s.log 2>&1 &
	k3s_pid=$!
	count=0
	while ! kubectl get --raw=/readyz >/dev/null 2>&1 && [ "$count" -lt 1800 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1800 ] || fail apiserver-not-ready

	for ordinal in 1 2 3 4 5 6 7; do
		count=0
		while ! kubectl get node "storage-$ordinal" >/dev/null 2>&1 && [ "$count" -lt 1800 ]; do
			sleep 0.1
			count=$((count + 1))
		done
		[ "$count" -lt 1800 ] || fail "initial-node-not-registered-storage-$ordinal"
	done
	initial_nodes="$(kubectl get nodes -l zcutils.io/storage-node=true --no-headers | wc -l)"
	[ "$initial_nodes" = 7 ] || fail "initial-storage-node-count-$initial_nodes"

	kubectl apply -f /storage-crds.yaml >>/tmp/kubernetes.log 2>&1 || fail apply-crds
	kubectl apply -f /operator.yaml >>/tmp/kubernetes.log 2>&1 || fail apply-operator
	count=0
	while [ "$(kubectl get pod -l app=zccusan-capacity-operator -o jsonpath='{.items[0].status.phase}' 2>/dev/null)" != Running ] && [ "$count" -lt 1800 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1800 ] || fail operator-not-running
	kubectl apply -f /intents.yaml >>/tmp/kubernetes.log 2>&1 || fail apply-capacity-intents

	count=0
	while [ "$(kubectl get mediagrant qemu-storage-node-memory -o jsonpath='{.status.phase}' 2>/dev/null)" != Ready ] && [ "$count" -lt 1200 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1200 ] || fail media-grant-not-ready
	count=0
	while [ "$(kubectl get storageprofile qemu-bounded-userspace-mirror -o jsonpath='{.status.phase}' 2>/dev/null)" != Ready ] && [ "$count" -lt 1200 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1200 ] || fail storage-profile-not-ready

	create_volume()
	{
		volume_name="$1"
		client_node="$2"
		kubectl apply -f - >>/tmp/kubernetes.log 2>&1 <<EOF
apiVersion: storage.zcutils.io/v1alpha1
kind: ZcVolume
metadata:
  name: $volume_name
spec:
  profileRef: qemu-bounded-userspace-mirror
  capacityBytes: 8388608
  provisionedIops: 75
  clientNode: $client_node
  frontend: LinuxBlock
EOF
	}

	wait_volume_ready()
	{
		volume_name="$1"
		count=0
		while [ "$(kubectl get zcvolume "$volume_name" -o jsonpath='{.status.phase}' 2>/dev/null)" != Ready ] && [ "$count" -lt 1800 ]; do
			sleep 0.1
			count=$((count + 1))
		done
		[ "$count" -lt 1800 ] || fail "volume-not-ready-$volume_name"
	}

	create_volume capacity-volume-1 storage-7 || fail create-volume-1
	wait_volume_ready capacity-volume-1
	create_volume capacity-volume-2 storage-5 || fail create-volume-2
	wait_volume_ready capacity-volume-2
	create_volume capacity-volume-3 storage-3 || fail create-volume-3
	wait_volume_ready capacity-volume-3

	reserved_leaves="$(kubectl get zcvolume capacity-volume-1 capacity-volume-2 capacity-volume-3 -o jsonpath='{range .items[*].status.runtime.leaves[*]}{.nodeName}{"\n"}{end}' | wc -l)"
	[ "$reserved_leaves" = 6 ] || fail "initial-reserved-leaves-$reserved_leaves"
	unique_leaves="$(kubectl get zcvolume capacity-volume-1 capacity-volume-2 capacity-volume-3 -o jsonpath='{range .items[*].status.runtime.leaves[*]}{.nodeName}{"\n"}{end}' | sort -u | wc -l)"
	[ "$unique_leaves" = 6 ] || fail "initial-unique-leaves-$unique_leaves"
	echo "ZCCUSAN_CAPACITY_INITIAL_PASS storage_nodes=7 ready_mirrored_volumes=3 reserved_userspace_leaves=6 free_leaf_slots=1 per_node_bytes=8388608 per_node_provisioned_iops=100"

	create_volume capacity-needs-storage-8 storage-1 || fail create-capacity-needs-storage-8
	request_generation="$(kubectl get zcvolume capacity-needs-storage-8 -o jsonpath='{.metadata.generation}')"
	count=0
	while [ "$(kubectl get zcvolume capacity-needs-storage-8 -o jsonpath='{.status.phase}' 2>/dev/null)" != Failed ] && [ "$count" -lt 1200 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1200 ] || fail pending-volume-did-not-fail-capacity-admission
	message="$(kubectl get zcvolume capacity-needs-storage-8 -o jsonpath='{.status.message}')"
	echo "$message" | grep -q 'found 1; capacity-rejected candidates=6' || fail "unexpected-capacity-message-$message"
	runtime_before="$(kubectl get zcvolume capacity-needs-storage-8 -o jsonpath='{.status.runtime}' 2>/dev/null || true)"
	[ -z "$runtime_before" ] || fail pending-volume-partially-reserved-capacity
	sleep 6
	[ "$(kubectl get zcvolume capacity-needs-storage-8 -o jsonpath='{.status.phase}')" = Failed ] || fail pending-volume-did-not-remain-failed
	[ "$(kubectl get zcvolume capacity-needs-storage-8 -o jsonpath='{.metadata.generation}')" = "$request_generation" ] || fail pending-request-mutated-before-node
	echo "ZCCUSAN_K8S_CAPACITY_NEEDS_NODE storage_nodes=7 request=capacity-needs-storage-8 generation=$request_generation existing_state_unchanged=true partial_reservation=false"

	count=0
	while ! kubectl get node storage-8 >/dev/null 2>&1 && [ "$count" -lt 1800 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1800 ] || fail storage-8-not-registered
	count=0
	while [ "$(kubectl get zcvolume capacity-needs-storage-8 -o jsonpath='{.status.phase}' 2>/dev/null)" != Ready ] && [ "$count" -lt 1800 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1800 ] || fail pending-volume-not-ready-after-storage-8
	final_generation="$(kubectl get zcvolume capacity-needs-storage-8 -o jsonpath='{.metadata.generation}')"
	[ "$final_generation" = "$request_generation" ] || fail request-was-mutated-for-admission
	final_leaves="$(kubectl get zcvolume capacity-needs-storage-8 -o jsonpath='{range .status.runtime.leaves[*]}{.nodeName}{"\n"}{end}' | sort | tr '\n' ',')"
	[ "$final_leaves" = 'storage-7,storage-8,' ] || fail "unexpected-final-leaves-$final_leaves"
	final_nodes="$(kubectl get nodes -l zcutils.io/storage-node=true --no-headers | wc -l)"
	[ "$final_nodes" = 8 ] || fail "final-storage-node-count-$final_nodes"
	echo "ZCCUSAN_K8S_CAPACITY_ADD_NODE_PASS storage_nodes_initial=7 storage_nodes_final=8 volume=capacity-needs-storage-8 request_generation=$request_generation request_mutated=false admitted_leaves=$final_leaves admission_trigger=node-registration-only userspace_placement=true block_placement=false"

	kubectl get zcvolume -o yaml >>/tmp/kubernetes.log 2>&1
	kubectl logs -l app=zccusan-capacity-operator >/tmp/operator.log 2>&1 || fail operator-logs
	kubectl delete zcvolume capacity-needs-storage-8 capacity-volume-1 capacity-volume-2 capacity-volume-3 --wait=true >>/tmp/kubernetes.log 2>&1 || fail delete-volumes
	kubectl delete deployment zccusan-capacity-operator --wait=true >>/tmp/kubernetes.log 2>&1 || fail delete-operator
	for ordinal in 1 2 3 4 5 6 7 8; do
		count=0
		while ! echo stop | nc "10.96.1.$((10 + ordinal))" 29990 2>/dev/null && [ "$count" -lt 400 ]; do
			sleep 0.05
			count=$((count + 1))
		done
		[ "$count" -lt 400 ] || fail "signal-storage-$ordinal"
	done
	stop_pid "$k3s_pid" || fail stop-k3s-server
	echo "ZCCUSAN_CAPACITY_QEMU_PASS role=controller manual_journey=separate userspace_capacity_contract=true kubernetes_nodes_initial=7 kubernetes_nodes_final=8 node_registration_only=true"
	sleep 0.2
	poweroff -f
fi

insmod /modules/zcnblk_client_mod.ko \
	transport=shm lanes=1 connections_per_lane=1 size_mib=16 \
	queues=1 queue_depth=64 max_frame_bytes=4096 pipeline_depth=64 \
	shm_ring_entries=64 shm_payload_entries=256 shm_poll_us=50 \
	hctx_affinity=1 pin_threads=1 pin_base_cpu=1 pin_cpu_count=1 pin_stride=1 \
	shm_bio_arena_zero_copy=0 || fail zcnblk-module
wait_path /dev/zcnblk0 || fail zcnblk-device
wait_path /dev/zcnblk-shmctl || fail zcnblk-control

/k3s agent --server=https://10.96.1.1:6443 --token=zccusan-capacity-qemu-token \
	--node-name="$role" --node-ip="$address" --snapshotter=native \
	--node-label='zcutils.io/storage-node=true' \
	--node-label='topology.kubernetes.io/zone=qemu-zone-a' \
	--pause-image=registry.k8s.io/pause:3.10 \
	>/tmp/k3s.log 2>&1 &
k3s_pid=$!
nc -l -p 29990 >/tmp/stop-signal || fail stop-signal
stop_pid "$k3s_pid" || fail stop-k3s-agent
rmmod zcnblk_client_mod || fail zcnblk-unload
echo "ZCCUSAN_CAPACITY_QEMU_PASS role=$role node_registered=true storage_capacity_bytes=8388608 provisioned_iops=100"
sleep 0.2
poweroff -f
