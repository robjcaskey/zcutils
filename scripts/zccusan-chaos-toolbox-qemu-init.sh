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
	echo "ZCCUSAN_CHAOS_QEMU_FAIL reason=$*"
	for log in /tmp/k3s.log /tmp/install.log /tmp/process-fault.log /tmp/network-fault.log; do
		[ ! -s "$log" ] || { echo "== $log =="; tail -300 "$log"; }
	done
	if [ -x /k3s ]; then
		/k3s kubectl get nodes -o wide 2>/dev/null || true
		/k3s kubectl get pods -A -o wide 2>/dev/null || true
		/k3s kubectl get events -A --sort-by=.lastTimestamp 2>/dev/null | tail -100 || true
		for pod in $(/k3s kubectl -n zccusan-chaos get pods -o name 2>/dev/null); do
			echo "== zccusan-chaos $pod =="
			/k3s kubectl -n zccusan-chaos logs "$pod" --all-containers --tail=100 2>/dev/null || true
		done
	fi
	dmesg | tail -100
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

if [ ! -e /.zccusan-chaos-system-root ]; then
	/sbin/modprobe virtio_blk || fail pivot-virtio-blk
	/sbin/modprobe ext4 || fail pivot-ext4
	wait_path /dev/vda || fail pivot-system-device
	mkdir -p /newroot
	mount -t ext4 /dev/vda /newroot || fail pivot-system-mount
	mount --move /proc /newroot/proc || fail pivot-proc
	mount --move /sys /newroot/sys || fail pivot-sys
	mount --move /dev /newroot/dev || fail pivot-dev
	mount --move /run /newroot/run || fail pivot-run
	# Success deliberately replaces this init process.
	# shellcheck disable=SC2093
	exec switch_root /newroot /init
	fail pivot-switch-root-returned
fi

mount --make-rshared / || fail shared-root
for module in virtio_net failover net_failover bridge br_netfilter overlay nf_tables; do
	/sbin/modprobe "$module" || fail "module-$module"
done
echo 1 >/proc/sys/net/ipv4/ip_forward
[ ! -e /proc/sys/net/bridge/bridge-nf-call-iptables ] || \
	echo 1 >/proc/sys/net/bridge/bridge-nf-call-iptables

echo 52aa0001000000000000000000000001 >/etc/machine-id
hostname qemu-chaos || fail hostname
ip link set lo up || fail loopback
ip link set eth0 up || fail link
ip address add 10.52.0.1/24 dev eth0 || fail address

echo "ZCCUSAN_CHAOS_QEMU_TOPOLOGY qemu_vms=1 kubernetes=k3s node=qemu-chaos comparator=hostpath representative_benchmark=false"
echo "ZCCUSAN_CHAOS_QEMU_ARTIFACT $(cat /chart-proof.txt)"

/k3s server \
	--cluster-init --node-name=qemu-chaos \
	--bind-address=10.52.0.1 --advertise-address=10.52.0.1 --node-ip=10.52.0.1 \
	--tls-san=10.52.0.1 --token=zccusan-chaos-qemu-token \
	--pause-image=registry.k8s.io/pause:3.10 \
	--flannel-backend=host-gw --flannel-iface=eth0 \
	--disable=traefik --disable=metrics-server --disable=local-storage \
	--disable=servicelb --disable=coredns --write-kubeconfig-mode=0600 \
	>/tmp/k3s.log 2>&1 &

count=0
while ! /k3s kubectl get --raw=/readyz >/dev/null 2>&1 && [ "$count" -lt 2400 ]; do
	sleep 0.1
	count=$((count + 1))
done
[ "$count" -lt 2400 ] || fail apiserver-not-ready
count=0
while [ "$(/k3s kubectl get node qemu-chaos -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)" != True ] \
	&& [ "$count" -lt 2400 ]; do
	sleep 0.1
	count=$((count + 1))
done
[ "$count" -lt 2400 ] || fail node-not-ready

/k3s kubectl label node qemu-chaos chaos.zcutils.io/allowed=true \
	>/tmp/install.log 2>&1 || fail label-node
/k3s kubectl create namespace zccusan-chaos >>/tmp/install.log 2>&1 || fail create-namespace
/k3s kubectl apply -f /chaos-chart.yaml >>/tmp/install.log 2>&1 || fail install-chart
/k3s kubectl -n zccusan-chaos rollout status daemonset/zccusan-chaos \
	--timeout=180s >>/tmp/install.log 2>&1 || fail toolbox-rollout

/k3s kubectl apply -f /chaos-victims.yaml >>/tmp/install.log 2>&1 || fail install-victims
/k3s kubectl -n zccusan-chaos wait --for=condition=Ready pod/hostpath-comparator \
	--timeout=120s >>/tmp/install.log 2>&1 || fail comparator-not-ready
/k3s kubectl -n zccusan-chaos wait --for=condition=Ready pod/network-victim \
	--timeout=120s >>/tmp/install.log 2>&1 || fail network-victim-not-ready

toolbox="$(/k3s kubectl -n zccusan-chaos get pod \
	-l app.kubernetes.io/name=zccusan-chaos-toolbox \
	-o jsonpath='{.items[0].metadata.name}')"
[ -n "$toolbox" ] || fail toolbox-not-found

before_sequence="$(cat /var/lib/zccusan-chaos-comparator/sequence 2>/dev/null || true)"
case "$before_sequence" in ''|*[!0-9]*) fail comparator-sequence-invalid;; esac
container_id="$(/k3s kubectl -n zccusan-chaos get pod hostpath-comparator \
	-o jsonpath='{.status.containerStatuses[0].containerID}')"
container_id="${container_id#containerd://}"
[ -n "$container_id" ] || fail comparator-container-not-found
before_restarts="$(/k3s kubectl -n zccusan-chaos get pod hostpath-comparator \
	-o jsonpath='{.status.containerStatuses[0].restartCount}')"

/k3s kubectl -n zccusan-chaos exec "$toolbox" -- \
	/usr/local/bin/zccusan-chaos-toolbox process-kill \
	--cgroup-contains "$container_id" --all --signal KILL >/tmp/process-fault.log 2>&1 \
	|| fail process-fault-command
grep -q '"event":"process_killed"' /tmp/process-fault.log || fail process-fault-marker

count=0
while [ "$count" -lt 600 ]; do
	after_restarts="$(/k3s kubectl -n zccusan-chaos get pod hostpath-comparator \
		-o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null || echo 0)"
	after_sequence="$(cat /var/lib/zccusan-chaos-comparator/sequence 2>/dev/null || echo 0)"
	if [ "$after_restarts" -gt "$before_restarts" ] 2>/dev/null \
		&& [ "$after_sequence" -gt "$before_sequence" ] 2>/dev/null; then
		break
	fi
	sleep 0.1
	count=$((count + 1))
done
[ "$count" -lt 600 ] || fail comparator-did-not-recover
echo "ZCCUSAN_CHAOS_QEMU_PROCESS_PASS target=exact-container-cgroup restart_observed=true hostpath_sequence_before=$before_sequence hostpath_sequence_after=$after_sequence"

printf 'zccusan-chaos-probe\n' | nc -w 2 127.0.0.1 29000 \
	| grep -q 'zccusan-chaos-probe' || fail network-baseline
/k3s kubectl -n zccusan-chaos exec "$toolbox" -- \
	/usr/local/bin/zccusan-chaos-toolbox network-blackhole \
	--experiment qemu-port --port 29000 --duration-seconds 3 \
	>/tmp/network-fault.log 2>&1 &
network_fault_pid=$!
count=0
while ! grep -q '"event":"network_blackhole_applied"' /tmp/network-fault.log 2>/dev/null \
	&& [ "$count" -lt 100 ]; do
	sleep 0.1
	count=$((count + 1))
done
[ "$count" -lt 100 ] || fail network-fault-not-applied
if printf 'zccusan-chaos-probe\n' | nc -w 1 127.0.0.1 29000 \
	| grep -q 'zccusan-chaos-probe'; then
	fail network-not-blackholed
fi
wait "$network_fault_pid" || fail network-fault-command
grep -q '"event":"network_restored"' /tmp/network-fault.log || fail network-restore-marker
printf 'zccusan-chaos-probe\n' | nc -w 2 127.0.0.1 29000 \
	| grep -q 'zccusan-chaos-probe' || fail network-not-restored
echo "ZCCUSAN_CHAOS_QEMU_NETWORK_PASS port=29000 bounded_seconds=3 blocked=true restored=true"

automount="$(/k3s kubectl -n zccusan-chaos get pod "$toolbox" \
	-o jsonpath='{.spec.automountServiceAccountToken}')"
[ "$automount" = false ] || fail service-account-token-mounted
echo "ZCCUSAN_CHAOS_QEMU_PASS chart=published-or-local image=published-or-local process=pass network=pass hostpath_comparator=pass node_poweroff=starting"

# This is intentionally last: success means QEMU exits because the separately
# enabled and acknowledged SYS_BOOT path really shut down the guest.
/k3s kubectl -n zccusan-chaos exec "$toolbox" -- \
	/usr/local/bin/zccusan-chaos-toolbox node-poweroff --confirm-node qemu-chaos
fail node-poweroff-returned
