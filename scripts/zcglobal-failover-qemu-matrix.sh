#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
OUTDIR="${OUTDIR:-$ROOT/bench-results/zcglobal-failover-qemu-$(date -u +%Y%m%dT%H%M%SZ)}"
OPERATIONS="${OPERATIONS:-16}"
MOVE_END="${MOVE_END:-24}"
LOSS_CHECKPOINT="${DECLARED_LOSS_CHECKPOINT:-8}"
K3S_VERSION="${K3S_VERSION:-v1.36.1+k3s1}"
K3S_BIN="${K3S_BIN:-$ROOT/target/qemu-zcglobal-volume-failover/k3s-$K3S_VERSION}"

fail() { printf 'ZCGLOBAL_FAILOVER_QEMU_COMPLETE_FAIL reason=%s artifact=%s\n' "$*" "$OUTDIR" >&2; exit 1; }
log() { printf '[zcglobal-failover-qemu-matrix] %s\n' "$*"; }

if [[ "${ZCGLOBAL_FAILOVER_MATRIX_COORDINATED:-0}" != 1 && -x "$COORD_BIN" ]]; then
	exec "$COORD_BIN" run --owner codex:zcutils-global-failover-matrix \
		--mode soft-exclusive --sensitivity high --priority 65 --ttl 3600 \
		--resource 'cpu=*;memory-bandwidth=*;kvm=*' \
		--note 'complete global failover correctness and fault matrix' \
		-- env ZCGLOBAL_FAILOVER_MATRIX_COORDINATED=1 "$0" "$@"
fi

[[ ! -e "$OUTDIR" ]] || fail "refusing-existing-outdir-$OUTDIR"
[[ -c /dev/kvm ]] || fail kvm-unavailable
[[ -x "$K3S_BIN" ]] || fail "missing-k3s-$K3S_BIN"
command -v jq >/dev/null || fail missing-jq
command -v podman >/dev/null || fail missing-podman
sudo -n true || fail sudo-noninteractive-unavailable
(( OPERATIONS >= 8 && OPERATIONS % 4 == 0 )) || fail invalid-operations
(( LOSS_CHECKPOINT >= 4 && LOSS_CHECKPOINT < OPERATIONS && LOSS_CHECKPOINT % 4 == 0 )) || fail invalid-loss-checkpoint
(( MOVE_END > OPERATIONS && MOVE_END < 4096 )) || fail invalid-move-end
mkdir -p "$OUTDIR"

run_step()
{
	local name="$1"
	shift
	log "running $name"
	"$@" 2>&1 | tee "$OUTDIR/$name.log"
}

active_harnesses=(
	"$ROOT/scripts/zcglobal-policy-qemu.sh"
	"$ROOT/scripts/zcglobal-regional-ha-qemu.sh"
	"$ROOT/scripts/zcglobal-volume-failover-qemu.sh"
)
if rg -n 'socket,id=[^ ]*,mcast=|mcast=' "${active_harnesses[@]}" >"$OUTDIR/multicast-audit.log"; then
	fail multicast-backed-active-harness
fi

run_step shell-syntax bash -n \
	"$ROOT/scripts/zcglobal-policy-qemu.sh" \
	"$ROOT/scripts/zcglobal-policy-qemu-init.sh" \
	"$ROOT/scripts/zcglobal-regional-ha-qemu.sh" \
	"$ROOT/scripts/zcglobal-regional-ha-qemu-init.sh" \
	"$ROOT/scripts/zcglobal-volume-failover-qemu.sh" \
	"$ROOT/scripts/zcglobal-volume-failover-qemu-init.sh"
run_step rust-lib cargo test --manifest-path "$ROOT/Cargo.toml" --lib
run_step kubernetes-adapter cargo test --manifest-path "$ROOT/Cargo.toml" --bin zcglobal-kubernetes-adapter

run_step global-raft env \
	OUTDIR="$OUTDIR/global-raft" ZCGLOBAL_POLICY_FULL_REPLICAS=1 \
	"$ROOT/scripts/zcglobal-policy-qemu.sh"

for failure_suffix in a b c; do
	run_step "regional-clean-$failure_suffix" env \
		ZCGLOBAL_REGIONAL_HA_COORDINATED=1 ZCGLOBAL_SCENARIO=clean \
		ZCGLOBAL_REGIONAL_FAILURE_SUFFIX="$failure_suffix" \
		OPERATIONS="$OPERATIONS" MOVE_END="$MOVE_END" DECLARED_LOSS_CHECKPOINT="$LOSS_CHECKPOINT" \
		WORK_DIR="$OUTDIR/regional-clean-$failure_suffix" TIMEOUT_SECONDS=240 \
		"$ROOT/scripts/zcglobal-regional-ha-qemu.sh"
done

run_step regional-declared-loss env \
	ZCGLOBAL_REGIONAL_HA_COORDINATED=1 ZCGLOBAL_SCENARIO=declared-loss \
	ZCGLOBAL_REGIONAL_FAILURE_SUFFIX=a \
	OPERATIONS="$OPERATIONS" MOVE_END="$MOVE_END" DECLARED_LOSS_CHECKPOINT="$LOSS_CHECKPOINT" \
	WORK_DIR="$OUTDIR/regional-declared-loss" TIMEOUT_SECONDS=240 \
	"$ROOT/scripts/zcglobal-regional-ha-qemu.sh"

run_step kubernetes-clean env \
	ZCGLOBAL_VOLUME_QEMU_COORDINATED=1 ZCGLOBAL_KUBERNETES=1 \
	ZCGLOBAL_REPLICATION_MODE=async ZCGLOBAL_SCENARIO=clean \
	OPERATIONS="$OPERATIONS" MOVE_END="$MOVE_END" DECLARED_LOSS_CHECKPOINT="$LOSS_CHECKPOINT" \
	WORK_DIR="$OUTDIR/kubernetes-clean" TIMEOUT_SECONDS=300 K3S_BIN="$K3S_BIN" \
	"$ROOT/scripts/zcglobal-volume-failover-qemu.sh"

mkdir -p "$OUTDIR/kubernetes-declared-loss"
cp --reflink=auto "$OUTDIR/kubernetes-clean/zcglobal-volume-workload.tar" \
	"$OUTDIR/kubernetes-clean/pause.tar" "$OUTDIR/kubernetes-declared-loss/"
run_step kubernetes-declared-loss env \
	ZCGLOBAL_VOLUME_QEMU_COORDINATED=1 ZCGLOBAL_KUBERNETES=1 REUSE_K8S_ARTIFACTS=1 \
	ZCGLOBAL_REPLICATION_MODE=async ZCGLOBAL_SCENARIO=declared-loss \
	OPERATIONS="$OPERATIONS" MOVE_END="$MOVE_END" DECLARED_LOSS_CHECKPOINT="$LOSS_CHECKPOINT" \
	WORK_DIR="$OUTDIR/kubernetes-declared-loss" TIMEOUT_SECONDS=300 K3S_BIN="$K3S_BIN" \
	"$ROOT/scripts/zcglobal-volume-failover-qemu.sh"

grep -q 'ZCGLOBAL_POLICY_QEMU_PASS.*control_regions=3.*region_loss_survivable=any-one.*quorum_loss=fail-closed.*failover_api=raft-committed.*clean_sessions=transparent-rebind.*stale_sessions=fenced' \
	"$OUTDIR/global-raft/result.log" || fail global-raft-evidence
for failure_suffix in a b c; do
	case "$failure_suffix" in
		a|b) frontend_failure=true ;;
		c) frontend_failure=false ;;
	esac
	grep -q "ZCGLOBAL_REGIONAL_HA_QEMU_MATRIX_PASS.*scenario=clean.*regional_replicas=3.*regional_quorum=2.*regional_frontends=2.*single_storage_vm_failures=one-per-region.*failed_vm_suffix=$failure_suffix.*failed_vm_includes_frontend=$frontend_failure.*client_reconnects=0.*acknowledged_data_loss=0.*qemu_l2_backend=tap-linux-bridge" \
		"$OUTDIR/regional-clean-$failure_suffix.log" || fail "regional-clean-$failure_suffix-evidence"
done
grep -q "ZCGLOBAL_REGIONAL_HA_QEMU_MATRIX_PASS.*scenario=declared-loss.*regional_replicas=3.*regional_quorum=2.*regional_frontends=2.*client_reconnects=0.*acknowledged_data_loss=booked-$((LOSS_CHECKPOINT + 1))..$OPERATIONS.*qemu_l2_backend=tap-linux-bridge" \
	"$OUTDIR/regional-declared-loss.log" || fail regional-declared-loss-evidence
grep -q 'ZCGLOBAL_KUBERNETES_STAY_PASS.*pod_uid_stable=true.*node_stable=true.*restart_count=0.*open_fd_stable=true' \
	"$OUTDIR/kubernetes-clean/logs/gateway.log" || fail kubernetes-stay-evidence
grep -q 'ZCGLOBAL_KUBERNETES_MOVE_PASS scenario=clean.*pod_uid_changed=true.*node_changed=true.*source_replicas=0.*target_replicas=1.*source_taint=NoSchedule.*target_taint=absent.*service_uid_stable=true.*service_ip_stable=true.*acknowledged_data_loss=0' \
	"$OUTDIR/kubernetes-clean/logs/gateway.log" || fail kubernetes-clean-move-evidence
grep -q "ZCGLOBAL_KUBERNETES_MOVE_PASS scenario=declared-loss source_region_lost=true.*source_replicas=0.*target_replicas=1.*adapter_ack=emitted.*acknowledged_data_loss=booked-$((LOSS_CHECKPOINT + 1))..$OPERATIONS" \
	"$OUTDIR/kubernetes-declared-loss/logs/gateway.log" || fail kubernetes-declared-loss-evidence
grep -q "ZCGLOBAL_VOLUME_DECLARED_LOSS_MOVE_PASS accepted_checkpoint=$LOSS_CHECKPOINT booked_missing=$((LOSS_CHECKPOINT + 1))..$OPERATIONS.*destination_tail_absent_before_reuse=true.*stale_clients_must_reconnect=true" \
	"$OUTDIR/kubernetes-declared-loss/logs/gateway.log" || fail exact-loss-and-fencing-evidence

jq -e '.status.failover.operations["clean-app-cut"].quiesce_order == ["kafka","postgres","cassandra"]
	and .status.failover.operations["clean-app-cut"].resume_order == ["cassandra","postgres","kafka"]
	and .status.failover.sessions["postgres-client-a"].state == "active"
	and .status.failover.sessions["postgres-client-a"].observed_placement_epoch == 8
	and .status.failover.sessions["loss-client-a"].state == "fenced"
	and .status.failover.loss_records[0].losses[0].first_missing == 33
	and .status.failover.loss_records[0].losses[0].last_missing == 64' \
	"$OUTDIR/global-raft/failover-committed.json" >/dev/null || fail global-state-evidence

cat >"$OUTDIR/requirements.tsv" <<EOF
requirement	evidence
regional zero-SPOF	regional clean A/B/C matrices remove each storage VM role in both 3-replica/2-quorum regions; A/B include a frontend and leaf, C is the non-frontend leaf
async clean cut	three regional clean permutations and Kubernetes clean gateway log: zero loss, same live FD for Stay workload
declared source-region loss	regional-declared-loss.log: source region destroyed, exact booked range $((LOSS_CHECKPOINT + 1))..$OPERATIONS
pod stay/follow	Kubernetes clean gateway log: stable Stay UID/node/FD; Follow ReplicaSets 1->0 and 0->1 with custody taints
stale client fencing	global Raft committed state and declared-loss gateway log
consistency relation	global Raft committed kafka->postgres->cassandra order and reverse resume order
interlocked relation rejection	Rust global_failover unit tests
global API HA	three full Raft replicas in three control regions, elected-region loss survived, quorum loss fail-closed
network portability	all active QEMU harnesses use TAP/Linux bridge with guest unicast TCP; multicast audit empty
placement ownership	userspace regional quorum and routing; terminal virtio-blk leaves only; no block RAID
EOF

printf 'ZCGLOBAL_FAILOVER_QEMU_COMPLETE_PASS qemu_matrices=7 data_regions=2 regional_replicas=3 regional_quorum=2 regional_frontends=2 regional_all_single_storage_vm_roles=survived global_control_regions=3 global_control_replicas=3 global_control_quorum=2 elected_control_region_loss=survived quorum_loss=fail-closed replication=async clean_loss=0 declared_loss=exact-%s..%s clean_session=transparent-rebind declared_session=fenced pod_stay=stable pod_follow=replicaset-and-taint consistency_group=postgres+kafka+cassandra dependency_cycles=rejected qemu_l2=tap-linux-bridge guest_transport=tcp-unicast userspace_placement=true block_raid=false artifact=%s\n' \
	"$((LOSS_CHECKPOINT + 1))" "$OPERATIONS" "$OUTDIR" | tee "$OUTDIR/result.log"
