#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KREL="${KREL:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-${KREL}}"
QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
OUTDIR="${OUTDIR:-${ROOT}/bench-results/zcglobal-policy-qemu-$(date -u +%Y%m%dT%H%M%SZ)}"
FULL_REPLICAS="${ZCGLOBAL_POLICY_FULL_REPLICAS:-0}"
ROOTFS="${OUTDIR}/rootfs"
INITRD="${OUTDIR}/zcglobal-policy-initramfs.cpio"
tag="$(printf '%04x' $(( $$ % 65536 )))"
bridge="zgp${tag}b"
roles=(a1 a2 b1)
addresses=(10.241.0.11:9910 10.241.0.12:9910 10.241.0.21:9910)
taps=()
pidfiles=()
jobs=()
network_created=0

log() { printf '[zcglobal-policy-qemu] %s\n' "$*"; }
fail() { printf '[zcglobal-policy-qemu] FAIL: %s\n' "$*" >&2; exit 1; }

copy_runtime_file() {
	local source_path="$1" dest="${ROOTFS}${2}"
	mkdir -p "$(dirname "$dest")"
	cp -L "$source_path" "$dest"
}

copy_binary() {
	local source_path="$1" dest_path="$2" library
	copy_runtime_file "$source_path" "$dest_path"
	while read -r library; do
		[ -n "$library" ] || continue
		copy_runtime_file "$library" "$library"
	done < <(ldd "$source_path" | awk '/=> \// {print $3; next} /^[[:space:]]*\/lib/ {print $1}')
}

copy_module() {
	local name="$1" source_path
	source_path="$(/usr/sbin/modinfo -k "$KREL" -n "$name")"
	case "$source_path" in
		*.xz) xz -dc "$source_path" > "$ROOTFS/modules/$name.ko" ;;
		*.zst) zstd -dc "$source_path" > "$ROOTFS/modules/$name.ko" ;;
		*.ko) cp "$source_path" "$ROOTFS/modules/$name.ko" ;;
		*) fail "unsupported module path $source_path" ;;
	esac
}

verified_stop_qemu() {
	local role="$1" pidfile="$2" pid comm cmdline
	[ -s "$pidfile" ] || return 0
	pid="$(cat "$pidfile")"
	case "$pid" in ''|*[!0-9]*) return 1 ;; esac
	[ -r "/proc/$pid/comm" ] || return 0
	comm="$(cat "/proc/$pid/comm")"
	cmdline="$(tr '\0' ' ' < "/proc/$pid/cmdline")"
	case "$comm:$cmdline" in
		qemu-system-x86*:*$INITRD*"zcglobal_role=$role"*) ;;
		*) printf 'refusing unverified pid=%s role=%s\n' "$pid" "$role" >&2; return 1 ;;
	esac
	kill -TERM "$pid"
	for _ in $(seq 1 50); do [ ! -e "/proc/$pid" ] && return 0; sleep 0.1; done
	[ ! -e "/proc/$pid" ] || kill -KILL "$pid"
}

cleanup() {
	local status=$? i
	trap - EXIT INT TERM
	set +e
	for i in "${!roles[@]}"; do verified_stop_qemu "${roles[$i]}" "${pidfiles[$i]:-}"; done
	for job in "${jobs[@]:-}"; do [ -z "$job" ] || wait "$job"; done
	if [ "$network_created" -eq 1 ]; then
		for tap in "${taps[@]:-}"; do [ -z "$tap" ] || sudo -n ip link del "$tap" 2>/dev/null || true; done
		sudo -n ip link del "$bridge" 2>/dev/null || true
	fi
	exit "$status"
}
trap cleanup EXIT INT TERM

status_json() { "$ROOT/target/release/zcglobal-policy-node" status "$1" 2>/dev/null; }

failover_apply() {
	local address="$1" label="$2" document="$3" command_file
	command_file="$OUTDIR/failover-$label-command.json"
	printf '%s\n' "$document" > "$command_file"
	"$ROOT/target/release/zcglobal-policy-node" failover-apply "$address" "$command_file" \
		> "$OUTDIR/failover-$label-response.json"
}

wait_all_ready() {
	local i
	for i in "${!roles[@]}"; do
		for _ in $(seq 1 300); do
			grep -q 'GLOBAL_RAFT_READY' "$OUTDIR/${roles[$i]}-console.log" 2>/dev/null && break
			sleep 0.05
		done
		grep -q 'GLOBAL_RAFT_READY' "$OUTDIR/${roles[$i]}-console.log" || fail "${roles[$i]} did not become ready"
	done
}

find_leader() {
	local address json
	for _ in $(seq 1 100); do
		for address in "$@"; do
			json="$(status_json "$address" || true)"
			if [ "$(jq -r '.status.role // empty' <<<"$json")" = leader ]; then
				printf '%s\n' "$address"
				return 0
			fi
		done
		sleep 0.05
	done
	return 1
}

wait_commit() {
	local address="$1" expected="$2" json
	for _ in $(seq 1 120); do
		json="$(status_json "$address" || true)"
		[ "$(jq -r '.status.commit_index // 0' <<<"$json")" -ge "$expected" ] && return 0
		sleep 0.05
	done
	return 1
}

[ -r "$KERNEL" ] || fail "missing kernel $KERNEL"
[[ "$FULL_REPLICAS" == 0 || "$FULL_REPLICAS" == 1 ]] \
	|| fail 'ZCGLOBAL_POLICY_FULL_REPLICAS must be 0 or 1'
[ -c /dev/kvm ] || fail '/dev/kvm unavailable'
command -v "$QEMU_BIN" >/dev/null
command -v jq >/dev/null
sudo -n true
[ ! -e "$OUTDIR" ] || fail "refusing existing OUTDIR=$OUTDIR"
mkdir -p "$ROOTFS"/{bin,usr/bin,proc,sys,dev,run,tmp,modules} "$OUTDIR"
mkdir -p "$ROOTFS/etc"

export ZCGLOBAL_FEDERATION_ID=qemu-default
export ZCGLOBAL_ADMIN_TOKEN_FILE="$OUTDIR/admin-token"

log 'building Raft policy node and initramfs'
cargo build --release --manifest-path "$ROOT/Cargo.toml" --bin zcglobal-policy-node
"$ROOT/target/release/zcglobal-policy-node" credential-init "$OUTDIR/admin-token" 1h
admin_token="$(jq -r --arg id "$(jq -r .active_id "$OUTDIR/admin-token")" '.credentials[] | select(.id == $id) | .secret' "$OUTDIR/admin-token")"
cp "$OUTDIR/admin-token" "$ROOTFS/etc/zcglobal-admin-token"
copy_binary /bin/busybox /bin/busybox
for applet in sh mount poweroff sync cat sleep seq mkdir insmod; do ln -s busybox "$ROOTFS/bin/$applet"; done
copy_binary /usr/bin/ip /usr/bin/ip
copy_binary "$ROOT/target/release/zcglobal-policy-node" /zcglobal-policy-node
for module in failover net_failover virtio_net; do copy_module "$module"; done
cp "$ROOT/scripts/zcglobal-policy-qemu-init.sh" "$ROOTFS/init"
chmod 0755 "$ROOTFS/init" "$ROOTFS/zcglobal-policy-node"
(
	cd "$ROOTFS"
	find . -print0 | cpio --null -o --format=newc > "$INITRD" 2> "$OUTDIR/cpio.log"
)

for i in "${!roles[@]}"; do
	tap="zgp${tag}${i}"
	[ ${#tap} -le 15 ]
	! ip link show dev "$tap" >/dev/null 2>&1
	taps+=("$tap")
	pidfiles+=("$OUTDIR/${roles[$i]}.pid")
done
! ip link show dev "$bridge" >/dev/null 2>&1
sudo -n ip link add "$bridge" type bridge
network_created=1
sudo -n ip addr add 10.241.0.1/24 dev "$bridge"
sudo -n ip link set "$bridge" type bridge stp_state 0
sudo -n ip link set "$bridge" up
for tap in "${taps[@]}"; do
	sudo -n ip tuntap add dev "$tap" mode tap user "$(id -un)"
	sudo -n ip link set "$tap" master "$bridge"
	sudo -n ip link set "$tap" up
done

{
	printf 'classification=correctness-only representative=false benchmark_numbers=none\n'
	if [[ "$FULL_REPLICAS" == 1 ]]; then
		printf 'regions=region-a:a1 region-b:a2 region-c:b1 voters=3 quorum=2 leader_eligible=a1,a2,b1 full_replicas=3 voting_only=none transport=tcp region_loss_survivable=any-one\n'
	else
		printf 'regions=region-a:a1,a2 region-b:b1 voters=3 quorum=2 leader_eligible=a1,a2 full_replicas=2 voting_only=b1 transport=tcp\n'
	fi
	printf 'control-plane=raft low-bandwidth=true low-message-rate=true authority-lease-ms=400\n'
	printf 'federation-trust=directional,default-deny,non-transitive keys-in-raft=false\n'
	printf 'data-plane=not-exercised block-placement=none kernel-placement=none\n'
	printf 'failure-injection=physical-tap-down hugetlb=not-applicable memlock=not-applicable\n'
} > "$OUTDIR/topology.log"

for i in "${!roles[@]}"; do
	mac="52:54:00:f1:${tag:0:2}:$(printf '%02x' $((i + 1)))"
	"$QEMU_BIN" -name "guest=zcglobal-${roles[$i]},debug-threads=on" \
		-machine q35,accel=kvm -cpu host -m 192M -smp 1 \
		-display none -monitor none -serial "file:$OUTDIR/${roles[$i]}-console.log" \
		-no-reboot -nodefaults -pidfile "${pidfiles[$i]}" \
		-kernel "$KERNEL" -initrd "$INITRD" \
		-append "console=ttyS0 panic=-1 oops=panic zcglobal_role=${roles[$i]} zcglobal_full_replicas=$FULL_REPLICAS" \
		-netdev "tap,id=link0,ifname=${taps[$i]},script=no,downscript=no" \
		-device "virtio-net-pci,netdev=link0,mac=$mac" &
	jobs+=("$!")
done

wait_all_ready
leader="$(find_leader "${addresses[@]}")" || fail 'no initial Raft leader'
leader_role="${roles[0]}"
for i in "${!addresses[@]}"; do [ "${addresses[$i]}" != "$leader" ] || leader_role="${roles[$i]}"; done
printf 'initial_leader=%s address=%s\n' "$leader_role" "$leader" | tee "$OUTDIR/failover.log"

log 'exercising Raft frame, structure, identity, and request-rate guards'
leader_term_before="$(status_json "$leader" | jq -r '.status.term')"
set +e
printf '{"federation_id":"qemu-default","admin_token":"%s","request":{"rpc":"status","unexpected":true}}\n' "$admin_token" | nc -w 1 "${leader%:*}" "${leader##*:}" \
	> "$OUTDIR/unknown-field-response.log" 2>&1
printf '{"federation_id":"qemu-default","request":{"rpc":"heartbeat","term":999999,"leader_id":"a1","committed":null,"witness_checkpoint":null}}\n' \
	| nc -w 1 "${leader%:*}" "${leader##*:}" > "$OUTDIR/spoofed-peer-response.log" 2>&1
head -c $((1024 * 1024 + 1)) /dev/zero | tr '\0' x \
	| nc -w 1 "${leader%:*}" "${leader##*:}" > "$OUTDIR/oversize-response.log" 2>&1
hostile_jobs=()
for _ in $(seq 1 24); do
	(sleep 1 | nc -w 2 "${leader%:*}" "${leader##*:}" >/dev/null 2>&1) &
	hostile_jobs+=("$!")
done
for job in "${hostile_jobs[@]}"; do wait "$job"; done
for _ in $(seq 1 80); do
	printf '{"federation_id":"qemu-default","admin_token":"%s","request":{"rpc":"status"}}\n' "$admin_token" | nc -w 1 "${leader%:*}" "${leader##*:}" >/dev/null 2>&1
done
set -e
sleep 2
leader_after_guards="$(status_json "$leader")"
[ "$(jq -r '.status.term' <<<"$leader_after_guards")" = "$leader_term_before" ] \
	|| fail 'spoofed peer RPC changed the Raft term'
grep -q 'global Raft RPC peer identity or rate rejected' "$OUTDIR/$leader_role-console.log" \
	|| fail 'request-rate or peer-identity guard did not reject hostile traffic'
grep -q 'reason=source_connection_limit' "$OUTDIR/$leader_role-console.log" \
	|| fail 'per-source concurrent connection guard did not reject hostile traffic'

log 'committing global rate envelope and imperative cross-region link'
"$ROOT/target/release/zcglobal-policy-node" set-rate "$leader" 1 10000000 20000000 \
	'region-a:6000000:14000000:3,region-b:4000000:10000000:1' \
	'region-a:14000000,region-b:1000000' 1 2 > "$OUTDIR/set-rate-1.json"
"$ROOT/target/release/zcglobal-policy-node" grant-region "$leader" trust-a-b region-a region-b \
	1 true false true automatic-on-loss region-a > "$OUTDIR/trust-a-b.json"
"$ROOT/target/release/zcglobal-policy-node" grant-region "$leader" trust-a-untrusted region-a region-untrusted \
	1 true false true denied - > "$OUTDIR/trust-a-untrusted.json"
"$ROOT/target/release/zcglobal-policy-node" set-inbound-policy "$leader" region-b 1 region-a \
	true false automatic-on-loss 1099511627776 backup residency=approved legal_hold=deny_export \
	> "$OUTDIR/inbound-region-b.json"
"$ROOT/target/release/zcglobal-policy-node" link-clusters "$leader" east-west cluster-a cluster-b \
	region-a region-b 1 100000 1000000 trust-a-b 1 > "$OUTDIR/link-1.json"
for address in "${addresses[@]}"; do wait_commit "$address" 5 || fail "$address did not commit initial trust/link state"; done

log 'committing clean grouped and declared-loss volume failovers through Raft'
failover_apply "$leader" put-postgres '{"command":"put_volume","expected_revision":0,"spec":{"volume_id":"postgres","authority_region":"region-a","placement_epoch":7,"consistency_set_id":null,"workload_bindings":[]}}'
failover_apply "$leader" put-kafka '{"command":"put_volume","expected_revision":1,"spec":{"volume_id":"kafka","authority_region":"region-a","placement_epoch":7,"consistency_set_id":null,"workload_bindings":[]}}'
failover_apply "$leader" put-cassandra '{"command":"put_volume","expected_revision":2,"spec":{"volume_id":"cassandra","authority_region":"region-a","placement_epoch":7,"consistency_set_id":null,"workload_bindings":[]}}'
failover_apply "$leader" put-app-set '{"command":"put_consistency_set","expected_revision":3,"spec":{"set_id":"app-stack","members":["cassandra","kafka","postgres"],"dependencies":[{"before":"kafka","after":"postgres"},{"before":"postgres","after":"cassandra"}]}}'
for volume in postgres kafka cassandra; do
	failover_apply "$leader" "observe-$volume-a" "{\"command\":\"observe_region\",\"progress\":{\"volume_id\":\"$volume\",\"region\":\"region-a\",\"observed_revision\":4,\"placement_epoch\":7,\"durable_lane_hwms\":{\"0\":100},\"applied_lane_hwms\":{\"0\":100},\"configured_replicas\":3,\"required_quorum\":2,\"live_replicas\":3,\"durable_failure_domains\":[\"az-a\",\"az-b\",\"az-c\"],\"reachable\":true,\"quiesced\":true}}"
	failover_apply "$leader" "observe-$volume-b" "{\"command\":\"observe_region\",\"progress\":{\"volume_id\":\"$volume\",\"region\":\"region-b\",\"observed_revision\":4,\"placement_epoch\":7,\"durable_lane_hwms\":{\"0\":100},\"applied_lane_hwms\":{\"0\":100},\"configured_replicas\":3,\"required_quorum\":2,\"live_replicas\":3,\"durable_failure_domains\":[\"az-a\",\"az-b\",\"az-c\"],\"reachable\":true,\"quiesced\":false}}"
done
failover_apply "$leader" checkpoint-app '{"command":"publish_checkpoint","checkpoint":{"checkpoint_id":"app-cut-100","set_id":"app-stack","sequence":100,"cuts":{"cassandra":{"0":100},"kafka":{"0":100},"postgres":{"0":100}},"application_consistent":true}}'
failover_apply "$leader" session-app '{"command":"register_session","session":{"session_id":"postgres-client-a","volume_id":"postgres","frontend":"failover-capable-client","region":"region-a","observed_placement_epoch":7,"supports_transparent_rebind":true,"state":"active","fence_reason":null}}'
failover_apply "$leader" request-app '{"command":"request_failover","request":{"operation_id":"clean-app-cut","requested_volume_ids":["postgres"],"source_region":"region-a","target_region":"region-b","expected_revision":4,"mode":{"mode":"clean"}}}'
failover_apply "$leader" reconcile-app '{"command":"reconcile","operation_id":"clean-app-cut"}'

failover_apply "$leader" put-loss-volume '{"command":"put_volume","expected_revision":7,"spec":{"volume_id":"loss-volume","authority_region":"region-a","placement_epoch":3,"consistency_set_id":null,"workload_bindings":[]}}'
failover_apply "$leader" put-loss-set '{"command":"put_consistency_set","expected_revision":8,"spec":{"set_id":"loss-set","members":["loss-volume"],"dependencies":[]}}'
failover_apply "$leader" observe-loss-a '{"command":"observe_region","progress":{"volume_id":"loss-volume","region":"region-a","observed_revision":9,"placement_epoch":3,"durable_lane_hwms":{"0":64},"applied_lane_hwms":{"0":64},"configured_replicas":3,"required_quorum":2,"live_replicas":0,"durable_failure_domains":["az-a","az-b","az-c"],"reachable":false,"quiesced":false}}'
failover_apply "$leader" observe-loss-b '{"command":"observe_region","progress":{"volume_id":"loss-volume","region":"region-b","observed_revision":9,"placement_epoch":3,"durable_lane_hwms":{"0":32},"applied_lane_hwms":{"0":32},"configured_replicas":3,"required_quorum":2,"live_replicas":3,"durable_failure_domains":["az-a","az-b","az-c"],"reachable":true,"quiesced":false}}'
failover_apply "$leader" checkpoint-loss '{"command":"publish_checkpoint","checkpoint":{"checkpoint_id":"loss-cut-32","set_id":"loss-set","sequence":32,"cuts":{"loss-volume":{"0":32}},"application_consistent":true}}'
failover_apply "$leader" session-loss '{"command":"register_session","session":{"session_id":"loss-client-a","volume_id":"loss-volume","frontend":"failover-capable-client","region":"region-a","observed_placement_epoch":3,"supports_transparent_rebind":true,"state":"active","fence_reason":null}}'
failover_apply "$leader" request-loss '{"command":"request_failover","request":{"operation_id":"declared-loss-cut","requested_volume_ids":["loss-volume"],"source_region":"region-a","target_region":"region-b","expected_revision":9,"mode":{"mode":"accept_declared_loss","reason":"region-a declared destroyed","max_missing_operations_per_lane":32}}}'
failover_apply "$leader" reconcile-loss '{"command":"reconcile","operation_id":"declared-loss-cut"}'
for address in "${addresses[@]}"; do wait_commit "$address" 27 || fail "$address did not commit failover state"; done
status_json "$leader" > "$OUTDIR/failover-committed.json"
jq -e '.status.commit_index == 27 and .status.failover.revision == 12 and .status.failover.operations["clean-app-cut"].phase == "completed" and .status.failover.operations["clean-app-cut"].quiesce_order == ["kafka","postgres","cassandra"] and .status.failover.operations["clean-app-cut"].resume_order == ["cassandra","postgres","kafka"] and .status.failover.operations["declared-loss-cut"].phase == "completed" and .status.failover.volumes.postgres.authority_region == "region-b" and .status.failover.volumes.kafka.authority_region == "region-b" and .status.failover.volumes.cassandra.authority_region == "region-b" and .status.failover.sessions["postgres-client-a"].state == "active" and .status.failover.sessions["postgres-client-a"].observed_placement_epoch == 8 and .status.failover.sessions["loss-client-a"].state == "fenced" and .status.failover.loss_records[0].losses[0].first_missing == 33 and .status.failover.loss_records[0].losses[0].last_missing == 64' "$OUTDIR/failover-committed.json" >/dev/null \
	|| fail 'Raft did not commit exact clean/loss failover state'

if [[ "$FULL_REPLICAS" == 1 ]]; then
	log "failing elected leader $leader_role and its one-node control region"
else
	log "failing elected leader $leader_role"
fi
leader_index=0
for i in "${!roles[@]}"; do [ "${roles[$i]}" != "$leader_role" ] || leader_index="$i"; done
sudo -n ip link set "${taps[$leader_index]}" down
remaining=()
for i in "${!addresses[@]}"; do [ "$i" -eq "$leader_index" ] || remaining+=("${addresses[$i]}"); done
new_leader="$(find_leader "${remaining[@]}")" || fail 'no leader after one-voter failure'
printf 'failed_leader=%s new_leader=%s\n' "$leader" "$new_leader" | tee -a "$OUTDIR/failover.log"

if [[ "$FULL_REPLICAS" == 1 ]]; then
	log 'committing through two surviving full replicas'
	mutation_leader="$new_leader"
	"$ROOT/target/release/zcglobal-policy-node" set-rate "$mutation_leader" 2 10000000 20000000 \
		'region-a:6000000:14000000:3,region-b:4000000:10000000:1' \
		'region-a:2000000,region-b:10000000' 3 4 > "$OUTDIR/set-rate-2.json"
else
	if "$ROOT/target/release/zcglobal-policy-node" set-rate "$new_leader" 2 10000000 20000000 \
		'region-a:6000000:14000000:3,region-b:4000000:10000000:1' \
		'region-a:2000000,region-b:10000000' 3 4 > "$OUTDIR/single-full-replica-mutation.json" 2>&1; then
		fail 'blind witness incorrectly counted as a recoverable policy copy'
	fi
	grep -q 'trusted_full_replica_unavailable' "$OUTDIR/single-full-replica-mutation.json"
	log 'restoring failed full replica before permitting policy mutation'
	sudo -n ip link set "${taps[$leader_index]}" up
	wait_commit "$leader" 5 || fail 'restored full replica did not catch up'
	mutation_leader="$(find_leader "${addresses[@]}")" || fail 'no leader after full-replica restore'
	"$ROOT/target/release/zcglobal-policy-node" set-rate "$mutation_leader" 2 10000000 20000000 \
		'region-a:6000000:14000000:3,region-b:4000000:10000000:1' \
		'region-a:2000000,region-b:10000000' 3 4 > "$OUTDIR/set-rate-2.json"
fi
"$ROOT/target/release/zcglobal-policy-node" grant-region "$mutation_leader" trust-b-a region-b region-a \
	1 true false true automatic-on-loss region-b > "$OUTDIR/trust-b-a.json"
"$ROOT/target/release/zcglobal-policy-node" set-inbound-policy "$mutation_leader" region-a 1 region-b \
	true false automatic-on-loss 1099511627776 backup residency=approved legal_hold=deny_export \
	> "$OUTDIR/inbound-region-a.json"
"$ROOT/target/release/zcglobal-policy-node" unlink-clusters "$mutation_leader" east-west 2 > "$OUTDIR/unlink-2.json"
"$ROOT/target/release/zcglobal-policy-node" link-clusters "$mutation_leader" east-west cluster-a cluster-b \
	region-a region-b 3 200000 2000000 trust-a-b 1 > "$OUTDIR/link-3.json"
if [[ "$FULL_REPLICAS" == 1 ]]; then
	log 'restoring failed full replica and verifying committed catch-up'
	sudo -n ip link set "${taps[$leader_index]}" up
fi
for address in "${addresses[@]}"; do wait_commit "$address" 32 || fail "$address did not commit post-failover changes"; done

log 'restoring old leader and verifying state catch-up'
wait_commit "$leader" 32 || fail 'restored voter did not catch up'
for i in "${!addresses[@]}"; do status_json "${addresses[$i]}" > "$OUTDIR/${roles[$i]}-caught-up.json"; done
jq -e '.status.policy_revision == 2 and .status.commit_index == 32 and (.status.cluster_links | length) == 1 and .status.cluster_links[0].generation == 3 and (.status.region_trust_grants | length) == 3 and (.status.regional_inbound_policies | length) == 2 and .status.failover.revision == 12 and .status.failover.operations["declared-loss-cut"].phase == "completed" and ([.status.region_trust_grants[] | select(.grant_id == "trust-a-untrusted" and .permissions.store_encrypted_replicas == true and .permissions.store_unencrypted_replicas == false and .permissions.key_escrow == "denied")] | length) == 1 and ([.status.region_trust_grants[] | select(.grant_id == "trust-a-b" and .permissions.key_escrow == "automatic_on_loss")] | length) == 1 and ([.status.region_trust_grants[] | select(.grant_id == "trust-b-a" and .permissions.key_escrow == "automatic_on_loss")] | length) == 1' "$OUTDIR/$leader_role-caught-up.json" >/dev/null

log 'isolating two voters and verifying lease expiry/fail-closed guarantee'
sudo -n ip link set "${taps[0]}" down
sudo -n ip link set "${taps[1]}" down
sleep 1
single="$(status_json "${addresses[2]}")"
printf '%s\n' "$single" > "$OUTDIR/quorum-loss.json"
if [[ "$FULL_REPLICAS" == 1 ]]; then
	jq -e '.status.authority_valid == false and .status.leader_eligible == true and .status.blind_witness == false and (.status.role == "follower" or .status.role == "candidate") and .status.policy_revision == 2 and .status.failover.operations["declared-loss-cut"].phase == "completed"' <<<"$single" >/dev/null \
		|| fail 'isolated full replica did not retain state while failing closed'
else
	jq -e '.status.authority_valid == false and .status.leader_eligible == false and .status.blind_witness == true and .status.role == "follower" and .status.policy_revision == 0 and .status.effective_iops == 0 and .status.protected_iops == 0 and (.status.cluster_links | length) == 0 and (.status.region_trust_grants | length) == 0 and (.status.regional_inbound_policies | length) == 0 and .status.failover == null' <<<"$single" >/dev/null \
		|| fail 'isolated voting-only member retained sensitive state or became authoritative'
fi
if "$ROOT/target/release/zcglobal-policy-node" unlink-clusters "${addresses[2]}" east-west 4 > "$OUTDIR/quorum-loss-proposal.json" 2>&1; then
	fail 'single voter unexpectedly accepted a mutation'
fi

log 'restoring quorum and checking retained policy/link state'
sudo -n ip link set "${taps[0]}" up
sudo -n ip link set "${taps[1]}" up
recovered_leader="$(find_leader "${addresses[@]}")" || fail 'Raft did not recover after quorum restore'
for address in "${addresses[@]}"; do wait_commit "$address" 32 || fail "$address lost committed state"; done
status_json "$recovered_leader" > "$OUTDIR/recovered.json"
jq -e '.status.authority_valid == true and .status.policy_revision == 2 and .status.commit_index == 32 and .status.cluster_links[0].generation == 3 and (.status.region_trust_grants | length) == 3 and (.status.regional_inbound_policies | length) == 2 and .status.failover.revision == 12 and .status.failover.loss_records[0].operation_id == "declared-loss-cut"' "$OUTDIR/recovered.json" >/dev/null

grep -hE 'GLOBAL_RAFT_(READY|LEADER|COMMIT)' "$OUTDIR"/*-console.log > "$OUTDIR/validation-summary.log"
if [[ "$FULL_REPLICAS" == 1 ]]; then
	printf 'ZCGLOBAL_POLICY_QEMU_PASS vms=3 control_regions=3 voters=3 quorum=2 leader_eligible=3 full_replicas=3 blind_voting_witness=0 elected_control_region_loss=survived region_loss_survivable=any-one leader_failover=pass mutation_after_one_controller_loss=pass restored_replica_catchup=pass commits=32 quorum_loss=fail-closed cluster_link_api=pass failover_api=raft-committed consistency_group=postgres+kafka+cassandra dependency_order=recorded clean_cut=pass clean_sessions=transparent-rebind declared_loss=exact-33..64 stale_sessions=fenced trust=directional-default-deny destination-admission=pass ciphertext-only=pass reciprocal-escrow=pass artifact=%s\n' "$OUTDIR" | tee "$OUTDIR/result.log"
else
	printf 'ZCGLOBAL_POLICY_QEMU_PASS vms=3 regions=2 voters=3 quorum=2 leader_eligible=2 full_replicas=2 blind_voting_witness=1 witness_plaintext_state=none witness_not_durability_copy=pass ineligible_leader=blocked commits=32 leader_failover=pass mutation_without_two_full_replicas=blocked quorum_loss=fail-closed cluster_link_api=pass failover_api=raft-committed consistency_group=postgres+kafka+cassandra dependency_order=recorded clean_cut=pass declared_loss=exact-33..64 stale_sessions=fenced trust=directional-default-deny destination-admission=pass ciphertext-only=pass reciprocal-escrow=pass artifact=%s\n' "$OUTDIR" | tee "$OUTDIR/result.log"
fi
