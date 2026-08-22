#!/bin/sh
set -u
export PATH=/bin:/sbin:/usr/bin:/usr/sbin

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /sys/kernel/debug /tmp
mount -t debugfs debugfs /sys/kernel/debug
mkdir -p /sys/fs/cgroup /run /etc
mount -t cgroup2 none /sys/fs/cgroup || true
mount -t tmpfs tmpfs /run || true

role=""
operations=64
move_end=96
kubernetes=0
replication=sync
scenario=clean
loss_checkpoint=32
for argument in $(cat /proc/cmdline); do
	case "$argument" in
		zcgf.role=*) role="${argument#zcgf.role=}" ;;
		zcgf.operations=*) operations="${argument#zcgf.operations=}" ;;
		zcgf.move_end=*) move_end="${argument#zcgf.move_end=}" ;;
		zcgf.kubernetes=*) kubernetes="${argument#zcgf.kubernetes=}" ;;
		zcgf.replication=*) replication="${argument#zcgf.replication=}" ;;
		zcgf.scenario=*) scenario="${argument#zcgf.scenario=}" ;;
		zcgf.loss_checkpoint=*) loss_checkpoint="${argument#zcgf.loss_checkpoint=}" ;;
	esac
done

fail()
{
	echo "ZCGLOBAL_FAILOVER_QEMU_FAIL role=${role:-early} reason=$*"
	for log in /tmp/leaf.log /tmp/target.log /tmp/workload.log /tmp/grade.log /tmp/failover.log /tmp/k3s.log /tmp/kubernetes.log; do
		[ ! -s "$log" ] || { echo "== $log =="; cat "$log"; }
	done
	dmesg | tail -80
	poweroff -f
}

wait_path()
{
	path="$1"
	count=0
	while [ ! -e "$path" ] && [ "$count" -lt 200 ]; do
		sleep 0.05
		count=$((count + 1))
	done
	[ -e "$path" ]
}

stop_pid()
{
	pid="$1"
	signal="$2"
	[ -n "$pid" ] || return 0
	[ -e "/proc/$pid" ] || return 0
	kill "-$signal" "$pid" 2>/dev/null || true
	count=0
	while [ -e "/proc/$pid" ] && [ "$count" -lt 200 ]; do
		sleep 0.05
		count=$((count + 1))
	done
	[ ! -e "/proc/$pid" ]
}

unload_module_retry()
{
	module="$1"
	attempts="${2:-100}"
	count=0
	while ! rmmod "$module" 2>/tmp/rmmod-error; do
		count=$((count + 1))
		if [ "$count" -ge "$attempts" ]; then
			cat /tmp/rmmod-error
			return 1
		fi
		sleep 0.1
	done
	echo "ZCGLOBAL_REGION_STAGE role=$role stage=module-unloaded module=$module retries=$count"
}

if [ "$kubernetes" = 1 ] && [ ! -e /.zcgf-system-root ]; then
	insmod /modules/virtio_blk.ko || fail pivot-virtio-blk-module
	insmod /modules/crc16.ko || fail pivot-crc16-module
	insmod /modules/mbcache.ko || fail pivot-mbcache-module
	insmod /modules/jbd2.ko || fail pivot-jbd2-module
	insmod /modules/ext4.ko || fail pivot-ext4-module
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
[ -e /sys/module/virtio_blk ] || insmod /modules/virtio_blk.ko || fail virtio-blk-module

case "$role" in
	region-us) address=10.45.0.1; partuuid=45aa0001-01; machine_id=45aa0001000000000000000000000001 ;;
	gateway) address=10.45.0.2; machine_id=45aa0002000000000000000000000002 ;;
	region-eu) address=10.45.0.3; partuuid=45aa0003-01; machine_id=45aa0003000000000000000000000003 ;;
	*) fail unknown-role ;;
esac
echo "$machine_id" >/etc/machine-id
hostname "$role" || fail hostname-set
ip link set lo up || fail loopback-up
ip link set eth0 up || fail link-up
ip address add "$address/24" dev eth0 || fail address-add

echo "ZCGLOBAL_FAILOVER_TOPOLOGY role=$role lane=0 worker=0 vcpu=0 lane_to_worker=0:0 lane_to_cpu=0:0 worker_qd=64 lanes=1 aggregate_outstanding=64 completion=remote-write-ack-plus-explicit-sync"
echo "ZCGLOBAL_FAILOVER_TOPOLOGY_WARNING functional_qemu=true representative_benchmark=false hugetlb=absent memlock_headroom=unverified kthread_affinity=guest-vcpu1 hctx_affinity=1 batching=lane-batch io_uring_fast_path=wal-leaf raw_transport_rtt=not_measured theoretical_iops_ceiling=not_reported"
echo "ZCGLOBAL_FAILOVER_NETWORK qemu_l2_backend=tap-linux-bridge guest_storage_transport=tcp-unicast guest_control_transport=tcp-unicast multicast_product_dependency=false rdma_emulation=false"

if [ "$role" = gateway ]; then
	export ZCNBLK_WAL_FAILOVER_MODE="$replication"
	export ZCNBLK_WAL_FAILOVER_FENCE_SOURCE_IP=10.45.0.1
	/zcnblk-wal-failover 10.45.0.2:29000 10.45.0.1:30000 10.45.0.3:30000 10.45.0.2:29110 1 \
		>/tmp/failover.log 2>&1 &
	gateway_pid=$!
	if [ "$kubernetes" = 1 ]; then
		/k3s server \
			--cluster-init --disable-agent --snapshotter=native \
			--bind-address=10.45.0.2 --advertise-address=10.45.0.2 --node-ip=10.45.0.2 \
			--tls-san=10.45.0.2 --token=zcglobal-qemu-k3s-token \
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
		[ "$count" -lt 1200 ] || fail kubernetes-apiserver-not-ready
		for node in region-us region-eu; do
			count=0
			while ! /k3s kubectl get node "$node" >/dev/null 2>&1 && [ "$count" -lt 1200 ]; do
				sleep 0.1
				count=$((count + 1))
			done
			[ "$count" -lt 1200 ] || fail "kubernetes-node-not-registered-$node"
		done
		/k3s kubectl label node region-us topology.zcutils.io/region=region-us --overwrite >>/tmp/kubernetes.log 2>&1 || fail kubernetes-label-us
		/k3s kubectl label node region-eu topology.zcutils.io/region=region-eu --overwrite >>/tmp/kubernetes.log 2>&1 || fail kubernetes-label-eu
		if [ "$scenario" = clean ]; then
		/k3s kubectl apply -f - >/tmp/kubernetes.log 2>&1 <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: zcglobal-stay
  labels:
    app: zcglobal-stay
spec:
  nodeName: region-us
  hostNetwork: true
  restartPolicy: Never
  terminationGracePeriodSeconds: 0
  tolerations:
  - operator: Exists
  containers:
  - name: workload
    image: localhost/zcglobal-volume-workload:qemu
    imagePullPolicy: Never
    args: ["stay", "/host-dev/zcnblk0", "10.45.0.2:29110", "$operations"]
    securityContext:
      privileged: true
    volumeMounts:
    - name: host-dev
      mountPath: /host-dev
  volumes:
  - name: host-dev
    hostPath:
      path: /dev
      type: Directory
EOF
		stay_uid_before="$(/k3s kubectl get pod zcglobal-stay -o 'jsonpath={.metadata.uid}')"
		count=0
		while ! /k3s kubectl logs zcglobal-stay 2>/dev/null | grep -q ZCGLOBAL_VOLUME_STAY_PASS && [ "$count" -lt 1200 ]; do
			sleep 0.1
			count=$((count + 1))
		done
		[ "$count" -lt 1200 ] || fail kubernetes-stay-pod
		/k3s kubectl logs zcglobal-stay >>/tmp/kubernetes.log 2>&1
		stay_uid_after="$(/k3s kubectl get pod zcglobal-stay -o 'jsonpath={.metadata.uid}')"
		stay_node="$(/k3s kubectl get pod zcglobal-stay -o 'jsonpath={.spec.nodeName}')"
		stay_restarts="$(/k3s kubectl get pod zcglobal-stay -o 'jsonpath={.status.containerStatuses[0].restartCount}')"
		[ "$stay_uid_before" = "$stay_uid_after" ] || fail kubernetes-stay-uid-changed
		[ "$stay_node" = region-us ] || fail kubernetes-stay-node-changed
		[ "$stay_restarts" = 0 ] || fail kubernetes-stay-restarted
		echo "ZCGLOBAL_KUBERNETES_STAY_PASS pod_uid=$stay_uid_after pod_uid_stable=true node=$stay_node node_stable=true restart_count=$stay_restarts open_fd_stable=true"
		/k3s kubectl delete pod zcglobal-stay --wait=true >/dev/null || fail kubernetes-delete-stay
		source_args='["hold", "/host-dev/zcnblk0", "'$operations'", "120"]'
		target_args='["move-hold", "/host-dev/zcnblk0", "'$operations'", "'$move_end'", "120"]'
		source_ready_marker=ZCGLOBAL_VOLUME_HOLD_READY
		target_ready_marker=ZCGLOBAL_VOLUME_MOVE_PASS
		operation_id=clean-cut
		source_region_lost=false
		acknowledged_data_loss=0
		else
			source_args='["disaster-source-hold", "/host-dev/zcnblk0", "10.45.0.2:29110", "'$loss_checkpoint'", "'$operations'", "120"]'
			target_args='["move-loss-hold", "/host-dev/zcnblk0", "'$loss_checkpoint'", "'$operations'", "'$move_end'", "120"]'
			source_ready_marker=ZCGLOBAL_VOLUME_DISASTER_SOURCE_READY
			target_ready_marker=ZCGLOBAL_VOLUME_DECLARED_LOSS_MOVE_PASS
			operation_id=declared-loss-cut
			source_region_lost=true
			acknowledged_data_loss="booked-$((loss_checkpoint + 1))..$operations"
		fi

		/k3s kubectl apply -f - >>/tmp/kubernetes.log 2>&1 <<EOF
apiVersion: v1
kind: Service
metadata:
  name: zcglobal-follow
spec:
  selector:
    app: zcglobal-follow
  ports:
  - name: identity
    port: 9999
    targetPort: 9999
---
apiVersion: apps/v1
kind: ReplicaSet
metadata:
  name: zcglobal-follow-us
  labels:
    app: zcglobal-follow
    zcutils.io/failover-binding: zcglobal-follow
    topology.zcutils.io/region: region-us
spec:
  replicas: 1
  selector:
    matchLabels:
      zcutils.io/failover-instance: zcglobal-follow-us
  template:
    metadata:
      labels:
        app: zcglobal-follow
        zcutils.io/failover-binding: zcglobal-follow
        zcutils.io/failover-instance: zcglobal-follow-us
        topology.zcutils.io/region: region-us
    spec:
      nodeSelector:
        topology.zcutils.io/region: region-us
      hostNetwork: true
      terminationGracePeriodSeconds: 0
      # This deliberately CNI-less QEMU cluster runs host-network workloads.
      # Tolerate only Kubernetes' readiness taints; do not tolerate the
      # failover.zcutils.io/custody taint used to fence a region.
      tolerations:
      - key: node.kubernetes.io/not-ready
        operator: Exists
      - key: node.kubernetes.io/unreachable
        operator: Exists
      containers:
      - name: workload
        image: localhost/zcglobal-volume-workload:qemu
        imagePullPolicy: Never
        args: $source_args
        securityContext:
          privileged: true
        volumeMounts:
        - name: host-dev
          mountPath: /host-dev
      volumes:
      - name: host-dev
        hostPath:
          path: /dev
          type: Directory
---
apiVersion: apps/v1
kind: ReplicaSet
metadata:
  name: zcglobal-follow-eu
  labels:
    app: zcglobal-follow
    zcutils.io/failover-binding: zcglobal-follow
    topology.zcutils.io/region: region-eu
spec:
  replicas: 0
  selector:
    matchLabels:
      zcutils.io/failover-instance: zcglobal-follow-eu
  template:
    metadata:
      labels:
        app: zcglobal-follow
        zcutils.io/failover-binding: zcglobal-follow
        zcutils.io/failover-instance: zcglobal-follow-eu
        topology.zcutils.io/region: region-eu
    spec:
      nodeSelector:
        topology.zcutils.io/region: region-eu
      hostNetwork: true
      terminationGracePeriodSeconds: 0
      tolerations:
      - key: node.kubernetes.io/not-ready
        operator: Exists
      - key: node.kubernetes.io/unreachable
        operator: Exists
      containers:
      - name: workload
        image: localhost/zcglobal-volume-workload:qemu
        imagePullPolicy: Never
        args: $target_args
        securityContext:
          privileged: true
        volumeMounts:
        - name: host-dev
          mountPath: /host-dev
      volumes:
      - name: host-dev
        hostPath:
          path: /dev
          type: Directory
EOF
		/k3s kubectl taint node region-eu failover.zcutils.io/custody=moving:NoSchedule --overwrite >>/tmp/kubernetes.log 2>&1 || fail kubernetes-initial-target-taint
		count=0
		while ! /k3s kubectl logs -l zcutils.io/failover-instance=zcglobal-follow-us 2>/dev/null | grep -q "$source_ready_marker" && [ "$count" -lt 1200 ]; do
			sleep 0.1
			count=$((count + 1))
		done
		[ "$count" -lt 1200 ] || fail kubernetes-follow-source-not-ready
		follow_source_pod="$(/k3s kubectl get pod -l zcutils.io/failover-instance=zcglobal-follow-us -o 'jsonpath={.items[0].metadata.name}')"
		follow_uid_before="$(/k3s kubectl get pod "$follow_source_pod" -o 'jsonpath={.metadata.uid}')"
		follow_node_before="$(/k3s kubectl get pod "$follow_source_pod" -o 'jsonpath={.spec.nodeName}')"
		service_uid_before="$(/k3s kubectl get service zcglobal-follow -o 'jsonpath={.metadata.uid}')"
		service_ip_before="$(/k3s kubectl get service zcglobal-follow -o 'jsonpath={.spec.clusterIP}')"
		/k3s kubectl logs "$follow_source_pod" >>/tmp/kubernetes.log 2>&1
		if [ "$scenario" = declared-loss ]; then
			# The source kubelet and its regional storage edge disappear together.
			# The API object is intentionally left behind for the adapter to fence
			# and force-delete only after global loss acceptance.
			echo godzilla | nc 10.45.0.1 29995 || fail kubernetes-destroy-source-region
			loss_response="$(echo "secondary accept-loss $loss_checkpoint godzilla-destroyed-source-region" | nc 10.45.0.2 29110)"
			echo "ZCGLOBAL_DECLARED_LOSS_CONTROL_RESPONSE $loss_response"
			echo "$loss_response" | grep -q 'declared_loss=true' || fail kubernetes-declared-loss-promotion
		fi
		cat >/tmp/follow-action.json <<EOF
{"action_id":"$operation_id:volume:zcglobal-follow","operation_id":"$operation_id","volume_id":"volume","binding_id":"zcglobal-follow","adapter_id":"qemu-kubernetes","adapter_kind":"kubernetes","policy":"follow_volume","source_region":"region-us","target_region":"region-eu","source_replicas":0,"target_replicas":1,"add_source_taint":true,"remove_target_taint":true,"source_region_lost":$source_region_lost,"acknowledged":false}
EOF
		ZCGLOBAL_KUBECTL=/k3s ZCGLOBAL_KUBECTL_PREFIX=kubectl ZCGLOBAL_KUBERNETES_DRAIN_TIMEOUT=30s \
			/zcglobal-kubernetes-adapter apply /tmp/follow-action.json >/tmp/follow-ack.json 2>>/tmp/kubernetes.log \
			|| fail kubernetes-follow-adapter
		grep -q '"command":"acknowledge_workload_action"' /tmp/follow-ack.json || fail kubernetes-follow-ack
		count=0
		while ! /k3s kubectl logs -l zcutils.io/failover-instance=zcglobal-follow-eu 2>/dev/null | grep -q "$target_ready_marker" && [ "$count" -lt 1200 ]; do
			sleep 0.1
			count=$((count + 1))
		done
		[ "$count" -lt 1200 ] || fail kubernetes-follow-destination
		follow_target_pod="$(/k3s kubectl get pod -l zcutils.io/failover-instance=zcglobal-follow-eu -o 'jsonpath={.items[0].metadata.name}')"
		/k3s kubectl logs "$follow_target_pod" >>/tmp/kubernetes.log 2>&1
		follow_uid_after="$(/k3s kubectl get pod "$follow_target_pod" -o 'jsonpath={.metadata.uid}')"
		follow_node_after="$(/k3s kubectl get pod "$follow_target_pod" -o 'jsonpath={.spec.nodeName}')"
		follow_restarts="$(/k3s kubectl get pod "$follow_target_pod" -o 'jsonpath={.status.containerStatuses[0].restartCount}')"
		service_uid_after="$(/k3s kubectl get service zcglobal-follow -o 'jsonpath={.metadata.uid}')"
		service_ip_after="$(/k3s kubectl get service zcglobal-follow -o 'jsonpath={.spec.clusterIP}')"
		follow_source_replicas="$(/k3s kubectl get replicaset zcglobal-follow-us -o 'jsonpath={.spec.replicas}')"
		follow_target_replicas="$(/k3s kubectl get replicaset zcglobal-follow-eu -o 'jsonpath={.spec.replicas}')"
		source_taint="$(/k3s kubectl get node region-us -o 'jsonpath={.spec.taints[?(@.key=="failover.zcutils.io/custody")].effect}')"
		target_taint="$(/k3s kubectl get node region-eu -o 'jsonpath={.spec.taints[?(@.key=="failover.zcutils.io/custody")].effect}')"
		[ "$follow_uid_before" != "$follow_uid_after" ] || fail kubernetes-follow-uid-did-not-change
		[ "$follow_node_before" = region-us ] && [ "$follow_node_after" = region-eu ] || fail kubernetes-follow-node-did-not-change
		[ "$follow_restarts" = 0 ] || fail kubernetes-follow-restarted
		[ "$follow_source_replicas" = 0 ] && [ "$follow_target_replicas" = 1 ] || fail kubernetes-replicaset-scale
		[ "$source_taint" = NoSchedule ] && [ -z "$target_taint" ] || fail kubernetes-custody-taints
		[ "$service_uid_before" = "$service_uid_after" ] || fail kubernetes-service-uid-changed
		[ "$service_ip_before" = "$service_ip_after" ] || fail kubernetes-service-ip-changed
		echo "ZCGLOBAL_KUBERNETES_MOVE_PASS scenario=$scenario source_region_lost=$source_region_lost source_pod_uid=$follow_uid_before destination_pod_uid=$follow_uid_after pod_uid_changed=true source_node=$follow_node_before destination_node=$follow_node_after node_changed=true restart_count=$follow_restarts source_replicas=$follow_source_replicas target_replicas=$follow_target_replicas source_taint=$source_taint target_taint=absent adapter_ack=emitted service_uid=$service_uid_after service_uid_stable=true service_ip=$service_ip_after service_ip_stable=true acknowledged_data_loss=$acknowledged_data_loss"
		if [ "$scenario" = declared-loss ]; then destinations="10.45.0.3"; else destinations="10.45.0.1 10.45.0.3"; fi
		for destination in $destinations; do
			count=0
			while ! echo stop | nc "$destination" 29997 2>/dev/null && [ "$count" -lt 200 ]; do
				sleep 0.05
				count=$((count + 1))
			done
			[ "$count" -lt 200 ] || fail "kubernetes-stop-signal-$destination"
		done
	fi
	if [ "$kubernetes" != 1 ] && [ "$scenario" = declared-loss ]; then
		nc -l -p 29112 >/tmp/source-destroyed || fail source-destroyed-wait
		loss_response="$(echo "secondary accept-loss $loss_checkpoint godzilla-destroyed-source-region" | nc 10.45.0.2 29110)"
		echo "ZCGLOBAL_DECLARED_LOSS_CONTROL_RESPONSE $loss_response"
		echo "$loss_response" | grep -q 'declared_loss=true' || fail declared-loss-promotion
		echo "ZCGLOBAL_DECLARED_LOSS_BARRIER role=gateway phase=connecting destination=10.45.0.3:29999"
		count=0
		# The destination uses an exec-and-close listener, so the one-shot
		# control barrier cannot become a mutual EOF wait.
		while ! echo move-loss | nc 10.45.0.3 29999 2>/dev/null && [ "$count" -lt 200 ]; do
			sleep 0.05
			count=$((count + 1))
		done
		[ "$count" -lt 200 ] || fail declared-loss-destination-signal
		echo "ZCGLOBAL_DECLARED_LOSS_BARRIER role=gateway phase=delivered retries=$count"
		nc -l -p 29111 >/tmp/gateway-done-eu || fail gateway-done-eu-wait
	elif [ "$kubernetes" != 1 ]; then
		nc -l -p 29111 >/tmp/gateway-done-us || fail gateway-done-us-wait
		nc -l -p 29111 >/tmp/gateway-done-eu || fail gateway-done-eu-wait
	fi
	cat /tmp/failover.log
	if [ "$scenario" != declared-loss ] && grep -q 'zcnblk-wal-failover-session-error' /tmp/failover.log; then
		fail gateway-session-error
	fi
	grep -q 'initial_active=primary' /tmp/failover.log || fail gateway-not-started
	grep -q 'placement_epoch=2' /tmp/failover.log || fail custody-was-not-transferred
	stop_pid "$gateway_pid" TERM || fail gateway-stop
	if [ "$kubernetes" = 1 ]; then
		stop_pid "$k3s_pid" TERM || fail k3s-stop
		cat /tmp/kubernetes.log
		# Let the serial tty drain the copied pod proof before the forced-poweroff
		# printk. Otherwise QEMU can interleave "reboot: Power down" inside the
		# final workload marker even though the workload and adapter succeeded.
		sleep 0.5
	fi
	echo "ZCGLOBAL_FAILOVER_QEMU_PASS role=$role custody=primary-to-secondary placement_epoch=2"
	sleep 0.2
	poweroff -f
fi

if [ "$kubernetes" = 1 ]; then terminal_device=/dev/vdb; else terminal_device=/dev/vda; fi
wait_path "$terminal_device" || fail missing-terminal-device
wait_path "${terminal_device}1" || fail missing-terminal-partition
echo "ZCGLOBAL_REGION_STAGE role=$role stage=terminal-ready"
mkdir -p /dev/disk/by-partuuid
ln -s "../../${terminal_device#/dev/}1" "/dev/disk/by-partuuid/$partuuid"
echo "PARTUUID=$partuuid" >/tmp/raw-partitions.allow
export URING_PLAY_RAW_PARTITION_ALLOWLIST=/tmp/raw-partitions.allow
export URING_PLAY_ALLOW_RAW_BLOCK_WRITE=1
export URING_PLAY_RAW_TARGET_PARTUUID="$partuuid"
leaf_connections=2
leaf_workers=2
if [ "$replication" = async ] && [ "$role" = region-eu ]; then
	leaf_connections=4
	leaf_workers=4
fi
URING_PLAY_ZCNBLK_WAL_RESULT_RANGES=1 \
	/zcnblk-wal-leaf "PARTUUID=$partuuid" "$address" 30000 1 "$leaf_connections" 4096 "$leaf_workers" false blocking \
	>/tmp/leaf.log 2>&1 &
leaf_pid=$!
echo "ZCGLOBAL_REGION_STAGE role=$role stage=leaf-started pid=$leaf_pid"

insmod /modules/aead.ko || fail aead-module
insmod /modules/zcnblk_client_mod.ko \
	transport=shm lanes=1 connections_per_lane=1 size_mib=32 \
	queues=1 queue_depth=64 max_frame_bytes=4096 pipeline_depth=64 \
	shm_ring_entries=64 shm_payload_entries=256 shm_poll_us=50 \
	hctx_affinity=1 pin_threads=1 pin_base_cpu=1 pin_cpu_count=1 pin_stride=1 \
	shm_bio_arena_zero_copy=0 || fail zcnblk-module
wait_path /dev/zcnblk0 || fail missing-zcnblk-device
wait_path /dev/zcnblk-shmctl || fail missing-zcnblk-control
echo "ZCGLOBAL_REGION_STAGE role=$role stage=block-edge-ready"

sleep 2
export URING_PLAY_ZCNBLK_SHM_LEAF_ADDR=10.45.0.2:29000
export URING_PLAY_ZCNBLK_SHM_REMOTE_CONNECT_RETRY_MS=20000
export URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH=1
export URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS=1
export URING_PLAY_ZCNBLK_SHM_ARENA_BACKING=vmalloc
export URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE=/tmp/target.pid
/zcnblk-shm-target /dev/zcnblk-shmctl wal-tcp 32 2 1000 1000 10000 \
	>/tmp/target.log 2>&1 &
target_job_pid=$!
count=0
while ! grep -q '^zcnblk-shm-target:' /tmp/target.log 2>/dev/null && [ "$count" -lt 400 ]; do
	sleep 0.05
	count=$((count + 1))
done
grep -q '^zcnblk-shm-target:' /tmp/target.log || fail target-not-ready
echo "ZCGLOBAL_REGION_STAGE role=$role stage=userspace-target-ready"
target_pid="$target_job_pid"
[ ! -s /tmp/target.pid ] || target_pid="$(cat /tmp/target.pid)"

half=$((operations / 2))
if [ "$kubernetes" = 1 ]; then
	echo "ZCGLOBAL_REGION_STAGE role=$role stage=k3s-agent-starting"
	(
		/k3s agent --server=https://10.45.0.2:6443 --token=zcglobal-qemu-k3s-token \
			--node-name="$role" --node-ip="$address" --snapshotter=native \
			--node-label="zcutils.io/region=${role#region-}" \
			--pause-image=registry.k8s.io/pause:3.10 \
			>/tmp/k3s.log 2>&1
		rc=$?
		echo "ZCGLOBAL_KUBERNETES_AGENT_EXIT role=$role rc=$rc"
		cat /tmp/k3s.log
		exit "$rc"
	) &
	k3s_pid=$!
	(
		sleep 20
		if [ -e "/proc/$k3s_pid" ]; then
			echo "ZCGLOBAL_KUBERNETES_AGENT_DIAGNOSTIC role=$role"
			tail -120 /tmp/k3s.log
		fi
	) &
	if [ "$scenario" = declared-loss ] && [ "$role" = region-us ]; then
		nc -l -p 29995 >/tmp/region-loss-signal || fail kubernetes-region-loss-signal
		echo "ZCGLOBAL_FAILOVER_QEMU_PASS role=$role disaster_source_destroyed=true acknowledged_through=$operations remote_checkpoint=$loss_checkpoint kubernetes_node_lost=true"
		# Let the serial backend drain proof of the acknowledged source HWM,
		# then model abrupt loss of the whole regional worker/storage VM.
		sleep 0.2
		poweroff -f
	else
		nc -l -p 29997 >/tmp/kubernetes-stop || fail kubernetes-stop-wait
		stop_pid "$k3s_pid" TERM || fail k3s-agent-stop
	fi
else

case "$scenario:$role" in
	declared-loss:region-us)
		/zcglobal-volume-workload disaster-source /dev/zcnblk0 10.45.0.2:29110 "$loss_checkpoint" "$operations" \
			>/tmp/workload.log 2>&1 || fail disaster-source-workload
		count=0
		while ! echo destroyed | nc 10.45.0.2 29112 2>/dev/null && [ "$count" -lt 200 ]; do
			sleep 0.05
			count=$((count + 1))
		done
		[ "$count" -lt 200 ] || fail source-destroyed-signal
		cat /tmp/workload.log
		echo "ZCGLOBAL_FAILOVER_QEMU_PASS role=$role disaster_source_destroyed=true acknowledged_through=$operations remote_checkpoint=$loss_checkpoint"
		# Keep the simulated region alive just long enough for the serial backend
		# to drain the result marker before the abrupt power-loss line is emitted.
		sleep 0.2
		poweroff -f
		;;
	declared-loss:region-eu)
		# Execute a no-op and close as soon as the barrier connection is
		# accepted. This is a one-shot control signal, not a byte stream.
		echo "ZCGLOBAL_DECLARED_LOSS_BARRIER role=region-eu phase=listening address=10.45.0.3:29999"
		nc -l -p 29999 -e /bin/true || fail declared-loss-source-signal-wait
		echo "ZCGLOBAL_DECLARED_LOSS_BARRIER role=region-eu phase=accepted"
		/zcglobal-volume-workload move-loss /dev/zcnblk0 "$loss_checkpoint" "$operations" "$move_end" \
			>/tmp/workload.log 2>&1 &
		workload_pid=$!
		(
			sleep 10
			if [ -e "/proc/$workload_pid" ]; then
				echo "ZCGLOBAL_DECLARED_LOSS_DIAGNOSTIC workload_pid=$workload_pid status=still-running"
				for log in /tmp/workload.log /tmp/target.log /tmp/leaf.log; do
					[ ! -s "$log" ] || { echo "== $log =="; cat "$log"; }
				done
			fi
		) &
		wait "$workload_pid" || fail declared-loss-move-workload
		;;
	clean:region-us)
		nc -l -p 29998 >/tmp/destination-done &
		done_pid=$!
		/zcglobal-volume-workload stay /dev/zcnblk0 10.45.0.2:29110 "$operations" \
			>/tmp/workload.log 2>&1 || fail stay-workload
		count=0
		while ! echo move | nc 10.45.0.3 29999 2>/dev/null && [ "$count" -lt 200 ]; do
			sleep 0.05
			count=$((count + 1))
		done
		[ "$count" -lt 200 ] || fail destination-signal
		wait "$done_pid" || fail destination-done-wait
		;;
	clean:region-eu)
		nc -l -p 29999 >/tmp/source-done || fail source-signal-wait
		/zcglobal-volume-workload move /dev/zcnblk0 "$operations" "$move_end" \
			>/tmp/workload.log 2>&1 || fail move-workload
		count=0
		while ! echo done | nc 10.45.0.1 29998 2>/dev/null && [ "$count" -lt 200 ]; do
			sleep 0.05
			count=$((count + 1))
		done
		[ "$count" -lt 200 ] || fail source-done-signal
		;;
esac
fi

stop_pid "$target_pid" INT || fail target-stop
wait "$target_job_pid" 2>/dev/null || true
wait "$leaf_pid" || fail leaf-exit

case "$role" in
	region-us)
		/zcglobal-volume-workload grade "${terminal_device}1" "$half" $((half + 1)) "$move_end" \
			>/tmp/grade.log 2>&1 || fail primary-grade
		;;
	region-eu)
		/zcglobal-volume-workload grade "${terminal_device}1" "$move_end" \
			>/tmp/grade.log 2>&1 || fail secondary-grade
		;;
esac

[ ! -s /tmp/workload.log ] || cat /tmp/workload.log
cat /tmp/grade.log
cat /tmp/target.log
cat /tmp/leaf.log
if [ "$kubernetes" != 1 ]; then
	grep -q 'ZCGLOBAL_VOLUME_.*_PASS' /tmp/workload.log || fail workload-pass-marker
fi
grep -q 'ZCGLOBAL_VOLUME_GRADE_PASS' /tmp/grade.log || fail grade-pass-marker
if dmesg | grep -Eq 'BUG:|Oops:|KASAN:|general protection fault|kernel panic'; then
	fail kernel-diagnostic
fi
echo "ZCGLOBAL_REGION_STAGE role=$role stage=zcnblk-unload-start"
unload_module_retry zcnblk_client_mod 100 || fail zcnblk-unload
echo "ZCGLOBAL_REGION_STAGE role=$role stage=zcnblk-unload-complete"
unload_module_retry aead 100 || fail aead-unload
if [ "$kubernetes" != 1 ]; then
	count=0
	while ! echo "$role" | nc 10.45.0.2 29111 2>/dev/null && [ "$count" -lt 400 ]; do
		sleep 0.05
		count=$((count + 1))
	done
	[ "$count" -lt 400 ] || fail gateway-final-signal
fi
echo "ZCGLOBAL_FAILOVER_QEMU_PASS role=$role terminal=virtio-blk userspace_placement=true"
poweroff -f
