#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KREL="${KREL:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-${KREL}}"
QEMU_BIN="${QEMU_BIN:-qemu-system-x86_64}"
OUTDIR="${OUTDIR:-${ROOT}/bench-results/zcglobal-multifederation-qemu-$(date -u +%Y%m%dT%H%M%SZ)}"
BASE_ROOTFS="${OUTDIR}/rootfs-base"
tag="$(printf '%04x' $(( $$ % 65536 )))"
bridge="zgm${tag}b"
roles=(use usw uk pe pw)
addresses=(10.242.0.11 10.242.0.12 10.242.0.21 10.242.0.31 10.242.0.32)
ready_counts=(1 2 3 2 1)
taps=()
pidfiles=()
jobs=()
network_created=0

log() { printf '[zcglobal-multifederation-qemu] %s\n' "$*"; }
fail() { printf '[zcglobal-multifederation-qemu] FAIL: %s\n' "$*" >&2; exit 1; }

copy_runtime_file() {
	local source_path="$1" dest="${BASE_ROOTFS}${2}"
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
	*.xz) xz -dc "$source_path" > "$BASE_ROOTFS/modules/$name.ko" ;;
	*.zst) zstd -dc "$source_path" > "$BASE_ROOTFS/modules/$name.ko" ;;
	*.ko) cp "$source_path" "$BASE_ROOTFS/modules/$name.ko" ;;
	*) fail "unsupported module path $source_path" ;;
	esac
}

verified_stop_qemu() {
	local role="$1" pidfile="$2" pid comm cmdline
	[ -s "$pidfile" ] || return 0
	pid="$(<"$pidfile")"
	case "$pid" in ''|*[!0-9]*) return 1 ;; esac
	[ -r "/proc/$pid/comm" ] || return 0
	comm="$(<"/proc/$pid/comm")"
	cmdline="$(tr '\0' ' ' < "/proc/$pid/cmdline")"
	case "$comm:$cmdline" in
	qemu-system-x86*:*$OUTDIR*"zcglobal_role=$role"*) ;;
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

token_file() { printf '%s/%s.token\n' "$OUTDIR" "$1"; }

run_cli() {
	local federation="$1"; shift
	ZCGLOBAL_FEDERATION_ID="$federation" \
	ZCGLOBAL_ADMIN_TOKEN_FILE="$(token_file "$federation")" \
		"$ROOT/target/release/zcglobal-policy-node" "$@"
}

status_json() { run_cli "$1" status "$2" 2>/dev/null; }

find_leader() {
	local federation="$1"; shift
	local address json
	for _ in $(seq 1 160); do
		for address in "$@"; do
			json="$(status_json "$federation" "$address" || true)"
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
	local federation="$1" address="$2" expected="$3" json
	for _ in $(seq 1 160); do
		json="$(status_json "$federation" "$address" || true)"
		[ "$(jq -r '.status.commit_index // 0' <<<"$json")" -ge "$expected" ] && return 0
		sleep 0.05
	done
	return 1
}

[ -r "$KERNEL" ] || fail "missing kernel $KERNEL"
[ -c /dev/kvm ] || fail '/dev/kvm unavailable'
command -v "$QEMU_BIN" >/dev/null
command -v jq >/dev/null
sudo -n true
[ ! -e "$OUTDIR" ] || fail "refusing existing OUTDIR=$OUTDIR"
mkdir -p "$BASE_ROOTFS"/{bin,usr/bin,proc,sys,dev,run,tmp,modules,etc} "$OUTDIR"

log 'building federation-bound Raft node and role-specific initramfs images'
cargo build --release --manifest-path "$ROOT/Cargo.toml" --bin zcglobal-policy-node
copy_binary /bin/busybox /bin/busybox
for applet in sh mount poweroff sync cat sleep seq mkdir insmod; do ln -s busybox "$BASE_ROOTFS/bin/$applet"; done
copy_binary /usr/bin/ip /usr/bin/ip
copy_binary "$ROOT/target/release/zcglobal-policy-node" /zcglobal-policy-node
for module in failover net_failover virtio_net; do copy_module "$module"; done
cp "$ROOT/scripts/zcglobal-multifederation-qemu-init.sh" "$BASE_ROOTFS/init"
chmod 0755 "$BASE_ROOTFS/init" "$BASE_ROOTFS/zcglobal-policy-node"

"$ROOT/target/release/zcglobal-policy-node" credential-init "$(token_file atlas)" 1h
"$ROOT/target/release/zcglobal-policy-node" credential-init "$(token_file borealis)" 1h
"$ROOT/target/release/zcglobal-policy-node" credential-init "$(token_file concord)" 1h

for role in "${roles[@]}"; do
	rootfs="$OUTDIR/rootfs-$role"
	cp -a "$BASE_ROOTFS" "$rootfs"
	case "$role" in
	use) cp "$(token_file atlas)" "$rootfs/etc/atlas.token" ;;
	usw) cp "$(token_file atlas)" "$rootfs/etc/atlas.token"; cp "$(token_file concord)" "$rootfs/etc/concord.token" ;;
	uk) cp "$(token_file atlas)" "$rootfs/etc/atlas.token"; cp "$(token_file borealis)" "$rootfs/etc/borealis.token"; cp "$(token_file concord)" "$rootfs/etc/concord.token" ;;
	pe) cp "$(token_file borealis)" "$rootfs/etc/borealis.token"; cp "$(token_file concord)" "$rootfs/etc/concord.token" ;;
	pw) cp "$(token_file borealis)" "$rootfs/etc/borealis.token" ;;
	esac
	(
		cd "$rootfs"
		find . -print0 | cpio --null -o --format=newc > "$OUTDIR/$role.cpio" 2> "$OUTDIR/$role-cpio.log"
	)
done

for i in "${!roles[@]}"; do
	tap="zgm${tag}${i}"
	[ ${#tap} -le 15 ]
	! ip link show dev "$tap" >/dev/null 2>&1
	taps+=("$tap")
	pidfiles+=("$OUTDIR/${roles[$i]}.pid")
done
! ip link show dev "$bridge" >/dev/null 2>&1
sudo -n ip link add "$bridge" type bridge
network_created=1
sudo -n ip addr add 10.242.0.1/24 dev "$bridge"
sudo -n ip link set "$bridge" type bridge stp_state 0
sudo -n ip link set "$bridge" up
for tap in "${taps[@]}"; do
	sudo -n ip tuntap add dev "$tap" mode tap user "$(id -un)"
	sudo -n ip link set "$tap" master "$bridge"
	sudo -n ip link set "$tap" up
done

{
	printf 'classification=correctness-and-isolation representative-performance=false benchmark_numbers=none\n'
	printf 'federations=atlas,borealis,concord raft_groups=3 voters_per_group=3 total_processes=9\n'
	printf 'pops=us-east,us-west,uk,pottsylvania-east,pottsylvania-west overlapping_membership=us-west,uk,pottsylvania-east\n'
	printf 'atlas=us-east#leader,us-west#leader,uk#blind-voter\n'
	printf 'borealis=pottsylvania-east#leader,pottsylvania-west#leader,uk#blind-voter\n'
	printf 'concord=us-west#leader,uk#leader,pottsylvania-east#blind-voter\n'
	printf 'isolation=federation-id,state-file,management-token policy=directional,default-deny,non-transitive\n'
	printf 'data-plane=not-exercised destructive-test=control-plane-unlink\n'
} > "$OUTDIR/topology.log"

for i in "${!roles[@]}"; do
	mac="52:54:00:f2:${tag:0:2}:$(printf '%02x' $((i + 1)))"
	"$QEMU_BIN" -name "guest=zcglobal-multi-${roles[$i]},debug-threads=on" \
		-machine q35,accel=kvm -cpu host -m 256M -smp 1 \
		-display none -monitor none -serial "file:$OUTDIR/${roles[$i]}-console.log" \
		-no-reboot -nodefaults -pidfile "${pidfiles[$i]}" \
		-kernel "$KERNEL" -initrd "$OUTDIR/${roles[$i]}.cpio" \
		-append "console=ttyS0 panic=-1 oops=panic zcglobal_role=${roles[$i]}" \
		-netdev "tap,id=link0,ifname=${taps[$i]},script=no,downscript=no" \
		-device "virtio-net-pci,netdev=link0,mac=$mac" &
	jobs+=("$!")
done

for i in "${!roles[@]}"; do
	for _ in $(seq 1 400); do
		ready="$(grep -c 'GLOBAL_RAFT_READY' "$OUTDIR/${roles[$i]}-console.log" 2>/dev/null || :)"
		[ "${ready:-0}" -ge "${ready_counts[$i]}" ] && break
		sleep 0.05
	done
	ready="$(grep -c 'GLOBAL_RAFT_READY' "$OUTDIR/${roles[$i]}-console.log" 2>/dev/null || :)"
	[ "${ready:-0}" -ge "${ready_counts[$i]}" ] \
		|| fail "${roles[$i]} did not start all federation members"
done

atlas_nodes=(10.242.0.11:9921 10.242.0.12:9921 10.242.0.21:9921)
borealis_nodes=(10.242.0.31:9922 10.242.0.32:9922 10.242.0.21:9922)
concord_nodes=(10.242.0.12:9923 10.242.0.21:9923 10.242.0.31:9923)
atlas_leader="$(find_leader atlas "${atlas_nodes[@]}")" || fail 'atlas has no leader'
borealis_leader="$(find_leader borealis "${borealis_nodes[@]}")" || fail 'borealis has no leader'
concord_leader="$(find_leader concord "${concord_nodes[@]}")" || fail 'concord has no leader'
printf 'atlas=%s borealis=%s concord=%s\n' "$atlas_leader" "$borealis_leader" "$concord_leader" > "$OUTDIR/leaders.log"

log 'committing distinct business policies with deliberately colliding record names'
run_cli atlas set-rate "$atlas_leader" 1 10000000 20000000 'us-east:5000000:12000000:2,us-west:5000000:12000000:2,uk:0:1000000:1' 'us-east:9000000,us-west:7000000,uk:0' 1 2 > "$OUTDIR/atlas-rate.json"
run_cli atlas grant-region "$atlas_leader" shared-trust us-east uk 1 true false true on-demand us-east,us-west > "$OUTDIR/atlas-trust.json"
run_cli atlas set-inbound-policy "$atlas_leader" uk 1 us-east true false on-demand 1099511627776 financial residency=atlantic employee_region=pottsylvania > "$OUTDIR/atlas-inbound.json"
run_cli atlas link-clusters "$atlas_leader" shared-link us-ledger uk-vault us-east uk 1 100000 1000000 shared-trust 1 > "$OUTDIR/atlas-link.json"

run_cli borealis set-rate "$borealis_leader" 1 2000000 6000000 'pottsylvania-east:1000000:4000000:2,pottsylvania-west:1000000:4000000:2,uk:0:500000:1' 'pottsylvania-east:3000000,pottsylvania-west:2000000,uk:0' 1 2 > "$OUTDIR/borealis-rate.json"
run_cli borealis grant-region "$borealis_leader" shared-trust pottsylvania-east uk 1 true false true denied - > "$OUTDIR/borealis-trust.json"
run_cli borealis set-inbound-policy "$borealis_leader" uk 1 pottsylvania-east true false denied 274877906944 industrial residency=pottsylvania employee_region=us > "$OUTDIR/borealis-inbound.json"
run_cli borealis link-clusters "$borealis_leader" shared-link potts-ledger uk-vault pottsylvania-east uk 1 25000 250000 shared-trust 1 > "$OUTDIR/borealis-link.json"

run_cli concord set-rate "$concord_leader" 1 1000000 3000000 'us-west:500000:2000000:2,uk:500000:2000000:2,pottsylvania-east:0:250000:1' 'us-west:1500000,uk:1000000,pottsylvania-east:0' 1 2 > "$OUTDIR/concord-rate.json"
run_cli concord grant-region "$concord_leader" encrypted-archive us-west pottsylvania-east 1 true false false denied - > "$OUTDIR/concord-trust.json"
run_cli concord set-inbound-policy "$concord_leader" pottsylvania-east 1 us-west true false denied 10995116277760 encrypted_archive purpose=dr residency=us > "$OUTDIR/concord-inbound.json"
run_cli concord link-clusters "$concord_leader" archive-link us-archive potts-ciphertext us-west pottsylvania-east 1 10000 100000 encrypted-archive 1 > "$OUTDIR/concord-link.json"

for node in "${atlas_nodes[@]}"; do wait_commit atlas "$node" 4 || fail "atlas member $node did not commit"; done
for node in "${borealis_nodes[@]}"; do wait_commit borealis "$node" 4 || fail "borealis member $node did not commit"; done
for node in "${concord_nodes[@]}"; do wait_commit concord "$node" 4 || fail "concord member $node did not commit"; done

atlas_before="$(status_json atlas "$atlas_leader")"
borealis_before="$(status_json borealis "$borealis_leader")"
concord_before="$(status_json concord "$concord_leader")"
atlas_index="$(jq -r '.status.commit_index' <<<"$atlas_before")"
borealis_index="$(jq -r '.status.commit_index' <<<"$borealis_before")"
jq -e '([.status.region_trust_grants[] | select(.delegate_region_id | startswith("pottsylvania"))] | length) == 0 and ([.status.cluster_links[] | select(.target_region_id | startswith("pottsylvania"))] | length) == 0' <<<"$atlas_before" >/dev/null \
	|| fail 'Atlas unexpectedly delegated US authority to Pottsylvania'
jq -e '([.status.region_trust_grants[] | select(.delegate_region_id | startswith("us-"))] | length) == 0 and ([.status.cluster_links[] | select(.target_region_id | startswith("us-"))] | length) == 0' <<<"$borealis_before" >/dev/null \
	|| fail 'Borealis unexpectedly delegated Pottsylvanian authority to the US'
jq -e '([.status.region_trust_grants[] | select(.grant_id == "encrypted-archive" and .permissions.store_encrypted_replicas == true and .permissions.store_unencrypted_replicas == false and .permissions.serve_encrypted_restore == false and .permissions.key_escrow == "denied")] | length) == 1 and ([.status.regional_inbound_policies[] | select(.region_id == "pottsylvania-east" and .accept_encrypted_volumes == true and .accept_unencrypted_volumes == false and .accept_key_escrow == "denied")] | length) == 1' <<<"$concord_before" >/dev/null \
	|| fail 'Concord archive is not ciphertext-only without escrow or restore authority'
concord_witness="$(status_json concord 10.242.0.31:9923)"
jq -e '.status.blind_witness == true and .status.policy_revision == 0 and (.status.cluster_links | length) == 0 and (.status.region_trust_grants | length) == 0 and (.status.regional_inbound_policies | length) == 0' <<<"$concord_witness" >/dev/null \
	|| fail 'Pottsylvanian Concord witness received plaintext policy state'

log 'attempting reciprocal cross-federation reads and destructive unlinks'
set +e
ZCGLOBAL_FEDERATION_ID=atlas ZCGLOBAL_ADMIN_TOKEN_FILE="$(token_file borealis)" \
	"$ROOT/target/release/zcglobal-policy-node" status "$atlas_leader" > "$OUTDIR/potts-read-atlas.log" 2>&1
potts_read_rc=$?
ZCGLOBAL_FEDERATION_ID=atlas ZCGLOBAL_ADMIN_TOKEN_FILE="$(token_file borealis)" \
	"$ROOT/target/release/zcglobal-policy-node" unlink-clusters "$atlas_leader" shared-link 2 > "$OUTDIR/potts-destroy-atlas.log" 2>&1
potts_destroy_rc=$?
ZCGLOBAL_FEDERATION_ID=borealis ZCGLOBAL_ADMIN_TOKEN_FILE="$(token_file atlas)" \
	"$ROOT/target/release/zcglobal-policy-node" status "$borealis_leader" > "$OUTDIR/us-read-borealis.log" 2>&1
us_read_rc=$?
ZCGLOBAL_FEDERATION_ID=borealis ZCGLOBAL_ADMIN_TOKEN_FILE="$(token_file atlas)" \
	"$ROOT/target/release/zcglobal-policy-node" unlink-clusters "$borealis_leader" shared-link 2 > "$OUTDIR/us-destroy-borealis.log" 2>&1
us_destroy_rc=$?
ZCGLOBAL_FEDERATION_ID=borealis ZCGLOBAL_ADMIN_TOKEN_FILE="$(token_file borealis)" \
	"$ROOT/target/release/zcglobal-policy-node" unlink-clusters "$atlas_leader" shared-link 2 > "$OUTDIR/wrong-federation-atlas.log" 2>&1
wrong_federation_rc=$?
set -e

[ "$potts_read_rc" -ne 0 ] && [ "$potts_destroy_rc" -ne 0 ] || fail 'Pottsylvanian credential accessed or mutated Atlas'
[ "$us_read_rc" -ne 0 ] && [ "$us_destroy_rc" -ne 0 ] || fail 'US credential accessed or mutated Borealis'
[ "$wrong_federation_rc" -ne 0 ] || fail 'wrong federation envelope was accepted'
grep -q 'management_authentication_failed' "$OUTDIR/potts-read-atlas.log"
grep -q 'management_authentication_failed' "$OUTDIR/potts-destroy-atlas.log"
grep -q 'management_authentication_failed' "$OUTDIR/us-read-borealis.log"
grep -q 'management_authentication_failed' "$OUTDIR/us-destroy-borealis.log"
grep -q 'federation_mismatch' "$OUTDIR/wrong-federation-atlas.log"

atlas_after="$(status_json atlas "$atlas_leader")"
borealis_after="$(status_json borealis "$borealis_leader")"
jq -e --argjson index "$atlas_index" '.status.federation_id == "atlas" and .status.commit_index == $index and (.status.cluster_links | length) == 1 and .status.cluster_links[0].link_id == "shared-link"' <<<"$atlas_after" >/dev/null || fail 'Atlas changed after foreign destructive request'
jq -e --argjson index "$borealis_index" '.status.federation_id == "borealis" and .status.commit_index == $index and (.status.cluster_links | length) == 1 and .status.cluster_links[0].link_id == "shared-link"' <<<"$borealis_after" >/dev/null || fail 'Borealis changed after foreign destructive request'

log 'proving same identifiers and overlapping PoPs remain independently mutable'
run_cli borealis unlink-clusters "$borealis_leader" shared-link 2 > "$OUTDIR/borealis-own-unlink.json"
wait_commit borealis "$borealis_leader" 5 || fail 'Borealis own destructive operation did not commit'
atlas_final="$(status_json atlas "$atlas_leader")"
borealis_final="$(status_json borealis "$borealis_leader")"
jq -e '.status.commit_index == 4 and (.status.cluster_links | length) == 1 and .status.cluster_links[0].link_id == "shared-link"' <<<"$atlas_final" >/dev/null || fail 'Borealis mutation crossed into Atlas'
jq -e '.status.commit_index == 5 and (.status.cluster_links | length) == 0' <<<"$borealis_final" >/dev/null || fail 'Borealis own unlink did not remain local'

for federation in atlas borealis concord; do
	case "$federation" in
	atlas) nodes=("${atlas_nodes[@]}") ;;
	borealis) nodes=("${borealis_nodes[@]}") ;;
	concord) nodes=("${concord_nodes[@]}") ;;
	esac
	for node in "${nodes[@]}"; do status_json "$federation" "$node" >> "$OUTDIR/all-status.ndjson"; done
done

printf 'ZCGLOBAL_MULTIFEDERATION_QEMU_PASS vms=5 pops=5 raft_groups=3 processes=9 overlapping_pops=3 policies=3 federation_bound_state=pass management_auth=pass cross_federation_read=denied reciprocal_read=denied cross_federation_destroy=denied reciprocal_destroy=denied wrong_federation_rpc=denied colliding_ids=isolated encrypted_archive_to_pottsylvania=ciphertext_only key_escrow_to_pottsylvania=denied plaintext_to_pottsylvania=denied data_plane=not_exercised artifact=%s\n' "$OUTDIR" | tee "$OUTDIR/result.log"
