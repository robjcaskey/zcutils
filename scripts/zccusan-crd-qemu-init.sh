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
lanes=1
for argument in $(cat /proc/cmdline); do
	case "$argument" in
		zccrd.role=*) role="${argument#zccrd.role=}" ;;
		zccrd.lanes=*) lanes="${argument#zccrd.lanes=}" ;;
	esac
done

fail()
{
	echo "ZCCUSAN_CRD_QEMU_FAIL role=${role:-early} reason=$*"
	if [ "${role:-}" = controller ] && [ -x /k3s ]; then
		/k3s kubectl get pods -A -o wide 2>/dev/null || true
		/k3s kubectl describe pods -n default 2>/dev/null || true
		for pod in $(/k3s kubectl get pods -n default -o name 2>/dev/null); do
			echo "== current logs $pod =="
			/k3s kubectl logs -n default "$pod" --all-containers 2>/dev/null || true
			echo "== previous logs $pod =="
			/k3s kubectl logs -n default "$pod" --all-containers --previous 2>/dev/null || true
		done
	fi
	for log in /tmp/k3s.log /tmp/operator.log /tmp/kubernetes.log; do
		[ ! -s "$log" ] || { echo "== $log =="; cat "$log"; }
	done
	dmesg | tail -80
	if [ "${role:-}" = controller ]; then
		for peer in 10.46.0.2 10.46.0.3; do
			echo stop | nc "$peer" 29990 2>/dev/null || true
		done
	fi
	poweroff -f
}

case "$lanes" in
	''|*[!0-9]*) fail invalid-lane-count ;;
esac
[ "$lanes" -ge 1 ] && [ "$lanes" -le 64 ] || fail invalid-lane-count

wait_path()
{
	path="$1"
	count=0
	while [ ! -e "$path" ] && [ "$count" -lt 400 ]; do
		sleep 0.05
		count=$((count + 1))
	done
	[ -e "$path" ]
}

load_module()
{
	name="$1"
	[ ! -e "/modules/$name.builtin" ] || return 0
	insmod "/modules/$name.ko"
}

stop_pid()
{
	pid="$1"
	[ -n "$pid" ] || return 0
	[ -e "/proc/$pid" ] || return 0
	kill -TERM "$pid" 2>/dev/null || true
	count=0
	while [ -e "/proc/$pid" ] && [ "$count" -lt 400 ]; do
		sleep 0.05
		count=$((count + 1))
	done
	[ ! -e "/proc/$pid" ]
}

if [ ! -e /.zccrd-system-root ]; then
	load_module virtio_blk || fail pivot-virtio-blk
	load_module crc16 || fail pivot-crc16
	load_module crc32c_generic || fail pivot-crc32c
	load_module mbcache || fail pivot-mbcache
	load_module jbd2 || fail pivot-jbd2
	load_module ext4 || fail pivot-ext4
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

load_module failover || fail failover-module
load_module net_failover || fail net-failover-module
load_module virtio_net || fail virtio-net-module
load_module aead || fail aead-module

case "$role" in
	controller) address=10.46.0.1; machine_id=46aa0001000000000000000000000001 ;;
	region-us) address=10.46.0.2; machine_id=46aa0002000000000000000000000002 ;;
	region-uk) address=10.46.0.3; machine_id=46aa0003000000000000000000000003 ;;
	*) fail unknown-role ;;
esac
echo "$machine_id" >/etc/machine-id
hostname "$role" || fail hostname
ip link set lo up || fail loopback
ip link set eth0 up || fail link
ip address add "$address/24" dev eth0 || fail address

echo "ZCCUSAN_CRD_QEMU_TOPOLOGY role=$role transport=tcp-unicast encryption=aes-256-authenticated placement=userspace lanes=$lanes qemu_l2=tap-linux-bridge representative_benchmark=false"

if [ "$role" = controller ]; then
	/k3s server \
		--cluster-init --disable-agent --snapshotter=native \
		--bind-address=10.46.0.1 --advertise-address=10.46.0.1 --node-ip=10.46.0.1 \
		--tls-san=10.46.0.1 --token=zccusan-crd-qemu-token \
		--disable=traefik --disable=coredns --disable=metrics-server \
		--disable=local-storage --disable=servicelb --disable=network-policy \
		--disable-kube-proxy --flannel-backend=none --write-kubeconfig-mode=0600 \
		>/tmp/k3s.log 2>&1 &
	k3s_pid=$!
	count=0
	while ! /k3s kubectl get --raw=/readyz >/dev/null 2>&1 && [ "$count" -lt 1200 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1200 ] || fail apiserver-not-ready
	for node in region-us region-uk; do
		count=0
		while ! /k3s kubectl get node "$node" >/dev/null 2>&1 && [ "$count" -lt 1200 ]; do
			sleep 0.1
			count=$((count + 1))
		done
		[ "$count" -lt 1200 ] || fail "node-not-registered-$node"
	done

	/k3s kubectl apply -f /storage-crds.yaml >>/tmp/kubernetes.log 2>&1 || fail apply-crds
	/k3s kubectl apply -f /operator.yaml >>/tmp/kubernetes.log 2>&1 || fail apply-operator
	count=0
	while ! /k3s kubectl get pod -l app=zccusan-crd-operator -o jsonpath='{.items[0].status.phase}' 2>/dev/null | grep -q Running && [ "$count" -lt 1200 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1200 ] || fail operator-not-running

	/k3s kubectl apply -f /test-intents.yaml >>/tmp/kubernetes.log 2>&1 || fail apply-test-intents
	count=0
	while [ "$(/k3s kubectl get tieringpolicy qemu-hot-spill -o jsonpath='{.status.phase}' 2>/dev/null)" != Ready ] && [ "$count" -lt 1200 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1200 ] || fail tiering-policy-not-ready
	count=0
	while [ "$(/k3s kubectl get zcvolume qemu-tiered-volume -o jsonpath='{.status.phase}' 2>/dev/null)" != Ready ] && [ "$count" -lt 800 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 800 ] || fail tiered-volume-not-ready
	/k3s kubectl get zcvolume qemu-tiered-volume -o yaml >>/tmp/kubernetes.log 2>&1

	/k3s kubectl apply -f /tier-writer.yaml >>/tmp/kubernetes.log 2>&1 || fail apply-tier-writer
	count=0
	while [ "$(/k3s kubectl get pod tier-writer -o jsonpath='{.status.phase}' 2>/dev/null)" != Succeeded ] && [ "$count" -lt 1200 ]; do
		phase="$(/k3s kubectl get pod tier-writer -o jsonpath='{.status.phase}' 2>/dev/null || true)"
		[ "$phase" != Failed ] || fail tier-writer-failed
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1200 ] || fail tier-writer-timeout
	/k3s kubectl logs tier-writer >>/tmp/kubernetes.log 2>&1

	/k3s kubectl apply -f /tier-verify.yaml >>/tmp/kubernetes.log 2>&1 || fail apply-tier-verify
	for pod in tier-verify-us tier-verify-uk; do
		count=0
		while [ "$(/k3s kubectl get pod "$pod" -o jsonpath='{.status.phase}' 2>/dev/null)" != Succeeded ] && [ "$count" -lt 1200 ]; do
			phase="$(/k3s kubectl get pod "$pod" -o jsonpath='{.status.phase}' 2>/dev/null || true)"
			[ "$phase" != Failed ] || fail "$pod-failed"
			sleep 0.1
			count=$((count + 1))
		done
		[ "$count" -lt 1200 ] || fail "$pod-timeout"
		/k3s kubectl logs "$pod" >>/tmp/kubernetes.log 2>&1
	done

	count=0
	while [ "$(/k3s kubectl get crossregionreplication qemu-us-to-uk -o jsonpath='{.status.phase}' 2>/dev/null)" != Ready ] && [ "$count" -lt 1200 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1200 ] || fail cross-region-not-ready
	accepted="$(/k3s kubectl get crossregionreplication qemu-us-to-uk -o jsonpath='{.status.acceptedHwm}')"
	durable="$(/k3s kubectl get crossregionreplication qemu-us-to-uk -o jsonpath='{.status.remoteDurableHwm}')"
	applied="$(/k3s kubectl get crossregionreplication qemu-us-to-uk -o jsonpath='{.status.remoteAppliedHwm}')"
	[ "$accepted:$durable:$applied" = 4096:4096:4096 ] || fail "cross-region-hwm-$accepted-$durable-$applied"
	/k3s kubectl apply -f /cross-verify.yaml >>/tmp/kubernetes.log 2>&1 || fail apply-cross-verify
	count=0
	while [ "$(/k3s kubectl get pod cross-verify-uk -o jsonpath='{.status.phase}' 2>/dev/null)" != Succeeded ] && [ "$count" -lt 1200 ]; do
		phase="$(/k3s kubectl get pod cross-verify-uk -o jsonpath='{.status.phase}' 2>/dev/null || true)"
		[ "$phase" != Failed ] || fail cross-verify-failed
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1200 ] || fail cross-verify-timeout
	/k3s kubectl logs cross-verify-uk >>/tmp/kubernetes.log 2>&1

	/k3s kubectl apply -f /cross-fail-closed.yaml >>/tmp/kubernetes.log 2>&1 || fail apply-cross-fail-closed
	count=0
	while [ "$(/k3s kubectl get crossregionreplication qemu-auto-failover-rejected -o jsonpath='{.status.phase}' 2>/dev/null)" != Failed ] && [ "$count" -lt 1200 ]; do
		sleep 0.1
		count=$((count + 1))
	done
	[ "$count" -lt 1200 ] || fail cross-fail-closed-status
	bad_hwm="$(/k3s kubectl get crossregionreplication qemu-auto-failover-rejected -o jsonpath='{.status.remoteDurableHwm}')"
	[ "$bad_hwm" = 0 ] || fail cross-fail-closed-nonzero-hwm

	/k3s kubectl logs -l app=zccusan-crd-operator >/tmp/operator.log 2>&1 || fail operator-logs
	if grep -q 'zct1[.]' /tmp/operator.log; then
		fail credential-leaked-to-operator-log
	fi
	/k3s kubectl get pod -l storage.zcutils.io/cross-region-replication=qemu-us-to-uk -o yaml >/tmp/cross-pods.yaml 2>&1
	if grep -q 'zct1[.]' /tmp/cross-pods.yaml; then
		fail credential-leaked-to-pod-spec
	fi

	echo "ZCCUSAN_CRD_TIER_PASS policy=Ready volume=Ready placement=userspace-mirror leaves=2 hot=MemoryEmptyDir spill=HostPathFile ack=hot-only"
	echo "ZCCUSAN_CRD_CROSS_REGION_PASS phase=Ready accepted_hwm=$accepted remote_durable_hwm=$durable remote_applied_hwm=$applied transport=aes-256-authenticated-tcp services=none automatic_failover=fail-closed"

	/k3s kubectl delete crossregionreplication qemu-us-to-uk --wait=true >>/tmp/kubernetes.log 2>&1 || fail delete-cross-region
	remaining="$(/k3s kubectl get pods -l storage.zcutils.io/cross-region-replication=qemu-us-to-uk --no-headers 2>/dev/null | wc -l)"
	[ "$remaining" = 0 ] || fail cross-region-finalizer-pods-remain
	/k3s kubectl delete zcvolume qemu-tiered-volume --wait=true >>/tmp/kubernetes.log 2>&1 || fail delete-tier-volume
	/k3s kubectl delete deployment zccusan-crd-operator --wait=true >>/tmp/kubernetes.log 2>&1 || fail delete-operator

	for peer in 10.46.0.2 10.46.0.3; do
		count=0
		while ! echo stop | nc "$peer" 29990 2>/dev/null && [ "$count" -lt 200 ]; do
			sleep 0.05
			count=$((count + 1))
		done
		[ "$count" -lt 200 ] || fail "signal-$peer"
	done
	stop_pid "$k3s_pid" || fail stop-k3s-server
	echo "ZCCUSAN_CRD_QEMU_PASS role=controller tier=true cross_region=true fail_closed=true"
	sleep 0.2
	poweroff -f
fi

mkdir -p /var/lib/zcutils/checkpoints /var/lib/zcutils/tier-spill
if [ "$role" = region-us ]; then
	dd if=/dev/zero of=/var/lib/zcutils/checkpoints/source.bin bs=4096 count=1 2>/dev/null || fail source-create
	printf 'cross-region-qemu-pass' | dd of=/var/lib/zcutils/checkpoints/source.bin conv=notrunc 2>/dev/null || fail source-marker
fi

insmod /modules/zcnblk_client_mod.ko \
	transport=shm lanes="$lanes" connections_per_lane=1 size_mib=16 \
	queues="$lanes" queue_depth=64 max_frame_bytes=4096 pipeline_depth=64 \
	shm_ring_entries=64 shm_payload_entries=256 shm_poll_us=50 \
	hctx_affinity=1 pin_threads=1 pin_base_cpu=1 pin_cpu_count=1 pin_stride=1 \
	shm_bio_arena_zero_copy=0 || fail zcnblk-module
wait_path /dev/zcnblk0 || fail zcnblk-device
wait_path /dev/zcnblk-shmctl || fail zcnblk-control

/k3s agent --server=https://10.46.0.1:6443 --token=zccusan-crd-qemu-token \
	--node-name="$role" --node-ip="$address" --snapshotter=native \
	--node-label="zcutils.io/region=${role#region-}" \
	--pause-image=registry.k8s.io/pause:3.10 \
	>/tmp/k3s.log 2>&1 &
k3s_pid=$!
nc -l -p 29990 >/tmp/stop-signal || fail stop-signal

grep -a -q 'tier-qemu-pass' /var/lib/zcutils/tier-spill/qemu-tiered-volume.spill || fail local-tier-spill-marker
if [ "$role" = region-uk ]; then
	grep -a -q 'cross-region-qemu-pass' /var/lib/zcutils/checkpoints/target.bin || fail local-cross-region-marker
fi
stop_pid "$k3s_pid" || fail stop-k3s-agent
rmmod zcnblk_client_mod || fail zcnblk-unload
echo "ZCCUSAN_CRD_QEMU_PASS role=$role tier_spill=true cross_region=$([ "$role" = region-uk ] && echo true || echo source)"
sleep 0.2
poweroff -f
