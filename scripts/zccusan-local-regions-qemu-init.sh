#!/bin/sh
set -u
export PATH=/bin:/sbin:/usr/bin:/usr/sbin:/usr/local/bin
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /sys/fs/cgroup /run /tmp /etc
mount -t cgroup2 none /sys/fs/cgroup || true
mount -t tmpfs tmpfs /run || true

fail()
{
	echo "ZCCUSAN_LOCAL_REGIONS_QEMU_FAIL reason=$*"
	for log in /tmp/k3s.log /tmp/install.log /tmp/failover.log; do
		[ ! -s "$log" ] || { echo "== $log =="; tail -500 "$log"; }
	done
	if [ -x /k3s ]; then
		/k3s kubectl get nodes -o wide 2>/dev/null || true
		/k3s kubectl get pods -A -o wide 2>/dev/null || true
		/k3s kubectl get events -A --sort-by=.lastTimestamp 2>/dev/null | tail -120 || true
		for namespace in zcblock-csi-a zcblock-csi-b zcblock-csi-c zcblock-local-regions-failover; do
			for pod in $(/k3s kubectl -n "$namespace" get pods -o name 2>/dev/null); do
				echo "== $namespace $pod =="
				/k3s kubectl -n "$namespace" logs "$pod" --all-containers --tail=160 2>/dev/null || true
			done
		done
	fi
	dmesg | tail -120
	poweroff -f
}

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

stop_pid()
{
	pid="$1"
	[ -n "$pid" ] || return 0
	[ -e "/proc/$pid" ] || return 0
	kill -TERM "$pid" 2>/dev/null || true
	count=0
	while [ -e "/proc/$pid" ] && [ "$count" -lt 1200 ]; do
		sleep 0.05
		count=$((count + 1))
	done
	[ ! -e "/proc/$pid" ]
}

if [ ! -e /.zclocal-regions-system-root ]; then
	modprobe virtio_blk || fail pivot-virtio-blk
	modprobe ext4 || fail pivot-ext4
	wait_path /dev/vda || fail pivot-system-device
	mkdir -p /newroot
	mount -t ext4 /dev/vda /newroot || fail pivot-system-mount
	mount --move /proc /newroot/proc || fail pivot-proc
	mount --move /sys /newroot/sys || fail pivot-sys
	mount --move /dev /newroot/dev || fail pivot-dev
	mount --move /run /newroot/run || fail pivot-run
	# If switch_root cannot exec /init, fall through to the explicit failure path.
	# shellcheck disable=SC2093
	exec switch_root /newroot /init
	fail pivot-switch-root-returned
fi

mount --make-rshared / || fail shared-root
for module in virtio_net failover net_failover bridge br_netfilter overlay configfs loop; do
	modprobe "$module" || fail "module-$module"
done
mkdir -p /sys/kernel/config
mount -t configfs configfs /sys/kernel/config 2>/dev/null || true
echo 1 >/proc/sys/net/ipv4/ip_forward
[ ! -e /proc/sys/net/bridge/bridge-nf-call-iptables ] || \
	echo 1 >/proc/sys/net/bridge/bridge-nf-call-iptables

echo 49aa0001000000000000000000000001 >/etc/machine-id
hostname qemu-local-regions || fail hostname
ip link set lo up || fail loopback
ip link set eth0 up || fail link
ip address add 10.49.0.1/24 dev eth0 || fail address

echo "ZCCUSAN_LOCAL_REGIONS_QEMU_TOPOLOGY qemu_vms=1 kubernetes_nodes=1 simulated_regions=3 namespaces=zcblock-csi-a,zcblock-csi-b,zcblock-csi-c transport=tcp-unicast placement=userspace block_raid=false representative_benchmark=false"

/k3s server \
	--cluster-init --node-name=qemu-local-regions \
	--bind-address=10.49.0.1 --advertise-address=10.49.0.1 --node-ip=10.49.0.1 \
	--tls-san=10.49.0.1 --token=zccusan-local-regions-qemu-token \
	--pause-image=registry.k8s.io/pause:3.10 \
	--flannel-backend=host-gw --flannel-iface=eth0 \
	--disable=traefik --disable=metrics-server --disable=local-storage \
	--disable=servicelb --disable=coredns --write-kubeconfig-mode=0600 \
	>/tmp/k3s.log 2>&1 &
k3s_pid=$!

count=0
while ! /k3s kubectl get --raw=/readyz >/dev/null 2>&1 && [ "$count" -lt 2400 ]; do
	sleep 0.1
	count=$((count + 1))
done
[ "$count" -lt 2400 ] || fail apiserver-not-ready
count=0
while [ "$(/k3s kubectl get node qemu-local-regions -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)" != True ] \
	&& [ "$count" -lt 2400 ]; do
	sleep 0.1
	count=$((count + 1))
done
[ "$count" -lt 2400 ] || fail node-not-ready

/k3s kubectl apply -f /snapshot-crds.yaml >>/tmp/install.log 2>&1 || fail snapshot-crds
for region in a b c; do
	/k3s kubectl create namespace "zcblock-csi-${region}" >>/tmp/install.log 2>&1 \
		|| fail "create-region-namespace-$region"
	/k3s kubectl label namespace "zcblock-csi-${region}" \
		"zcutils.io/local-region=${region}" >>/tmp/install.log 2>&1 \
		|| fail "label-region-namespace-$region"
done
for region in a b c; do
	/k3s kubectl apply -f "/region-${region}.yaml" >>/tmp/install.log 2>&1 \
		|| fail "install-region-$region"
	/k3s kubectl -n "zcblock-csi-${region}" rollout status \
		"daemonset/zcblock-csi-${region}-node" --timeout=300s >>/tmp/install.log 2>&1 \
		|| fail "rollout-region-$region"
done

image_a="$(/k3s kubectl -n zcblock-csi-a get daemonset zcblock-csi-a-node -o jsonpath='{.spec.template.spec.containers[?(@.name=="zcblock-csi")].image}')"
image_b="$(/k3s kubectl -n zcblock-csi-b get daemonset zcblock-csi-b-node -o jsonpath='{.spec.template.spec.containers[?(@.name=="zcblock-csi")].image}')"
image_c="$(/k3s kubectl -n zcblock-csi-c get daemonset zcblock-csi-c-node -o jsonpath='{.spec.template.spec.containers[?(@.name=="zcblock-csi")].image}')"
[ "$image_a" = docker.io/robjcaskey/zcblock-csi:0.1.4 ] || fail "unexpected-image-a-$image_a"
[ "$image_b" = docker.io/robjcaskey/zcblock-csi:0.1.5 ] || fail "unexpected-image-b-$image_b"
[ "$image_c" = docker.io/robjcaskey/zcblock-csi:0.1.6 ] || fail "unexpected-image-c-$image_c"

if ! CLEANUP=0 /test-local-regions-failover.sh >/tmp/failover.log 2>&1; then
	fail failover-suite
fi
cat /tmp/failover.log
grep -q 'ZCCUSAN_LOCAL_REGIONS_FAILOVER_PASS.*volume_handles_distinct=true.*source_writer_fenced=true.*first_promotion=b.*second_promotion=c' \
	/tmp/failover.log || fail failover-proof-marker

/k3s kubectl get namespace zcblock-csi-a zcblock-csi-b zcblock-csi-c \
	-o custom-columns=NAME:.metadata.name,REGION:.metadata.labels.zcutils\\.io/local-region
/k3s kubectl get storageclass zcfile-a zcfile-b zcfile-c \
	-o custom-columns=NAME:.metadata.name,DRIVER:.provisioner
/k3s kubectl -n zcblock-local-regions-failover get pvc,pod -o wide

echo "ZCCUSAN_LOCAL_REGIONS_QEMU_PASS qemu_vms=1 kubernetes_cluster=single instances=3 namespaces=zcblock-csi-a,zcblock-csi-b,zcblock-csi-c versions=0.1.4,0.1.5,0.1.6 volumes=3 cross_region_replication=pass planned_failover=a-to-b-to-c source_fence=pass placement=userspace block_raid=false"

/k3s kubectl delete namespace zcblock-local-regions-failover --wait=true --timeout=180s >/dev/null 2>&1 || true
stop_pid "$k3s_pid" || true
poweroff -f
