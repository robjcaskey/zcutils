#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAMESPACE=zccusan
CHAOS_NAMESPACE=zccusan-chaos
CLAIM=zc-mirror
CONFIRM_CONTEXT=
FAULT_SECONDS=5
KEEP_TEST_PODS=0

usage()
{
	cat <<'EOF'
usage: scripts/zccusan-validate-single-region-ha.sh --confirm-context CONTEXT [OPTIONS]

Runs a bounded single-region reliability check against the mirrored volume
created by the Kubernetes getting-started guide.

options:
  --chaos-namespace NAME    toolbox namespace (default: zccusan-chaos)
  --fault-seconds N         bounded network fault duration (default: 5)
  --keep-test-pods          leave the canary and comparator running
  --confirm-context NAME    required exact kubectl context safety check
EOF
}

while (( $# )); do
	case "$1" in
		--chaos-namespace) CHAOS_NAMESPACE="${2:?missing chaos namespace}"; shift 2 ;;
		--fault-seconds) FAULT_SECONDS="${2:?missing fault duration}"; shift 2 ;;
		--keep-test-pods) KEEP_TEST_PODS=1; shift ;;
		--confirm-context) CONFIRM_CONTEXT="${2:?missing context}"; shift 2 ;;
		-h|--help) usage; exit 0 ;;
		*) printf 'unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
	esac
done

for command in kubectl jq; do
	command -v "$command" >/dev/null || { printf '%s is required\n' "$command" >&2; exit 1; }
done
if [[ ! "$FAULT_SECONDS" =~ ^[0-9]+$ ]] \
	|| (( FAULT_SECONDS < 1 || FAULT_SECONDS > 60 )); then
	printf -- '--fault-seconds must be in 1..60\n' >&2
	exit 2
fi
context="$(kubectl config current-context)"
[[ -n "$CONFIRM_CONTEXT" && "$CONFIRM_CONTEXT" = "$context" ]] || {
	printf 'refusing to inject faults: pass --confirm-context %q\n' "$context" >&2
	exit 2
}

CANARY="$ROOT/zccusan/deploy/zcblock-csi/getting-started/single-region-ha-canary.yaml"
COMPARATOR="$ROOT/zccusan/deploy/zcblock-csi/getting-started/single-region-ha-hostpath-comparator.yaml"
tmp_dir="$(mktemp -d)"
cleanup()
{
	if [[ "$KEEP_TEST_PODS" != 1 ]]; then
		kubectl -n "$NAMESPACE" delete pod zc-single-region-ha-canary \
			zc-single-region-ha-hostpath --ignore-not-found --wait=false >/dev/null 2>&1 || true
	fi
	rm -rf -- "$tmp_dir"
}
trap cleanup EXIT

pass()
{
	printf 'PASS  %-24s %s\n' "$1" "$2"
}

fail()
{
	printf 'FAIL  %-24s %s\n' "$1" "$2" >&2
	exit 1
}

pod_sequence()
{
	local pod="$1" event="$2"
	kubectl -n "$NAMESPACE" logs "$pod" 2>/dev/null \
		| jq -Rr --arg event "$event" \
			'fromjson? | select(.event == $event) | .sequence' | tail -n 1
}

wait_for_progress()
{
	local pod="$1" event="$2" before="$3" label="$4" count=0 after
	while (( count < 450 )); do
		after="$(pod_sequence "$pod" "$event")"
		if [[ "$after" =~ ^[0-9]+$ ]] && (( after > before )); then
			pass "$label" "sequence $before -> $after"
			return 0
		fi
		sleep 0.1
		count=$((count + 1))
	done
	fail "$label" "no successful commit after sequence $before within 45 seconds"
}

toolbox_on_node()
{
	local node="$1" pod
	pod="$(kubectl -n "$CHAOS_NAMESPACE" get pods \
		-l app.kubernetes.io/name=zccusan-chaos-toolbox \
		--field-selector "spec.nodeName=$node" \
		-o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)"
	[[ -n "$pod" ]] || fail toolbox "no toolbox Pod is Ready on node $node"
	printf '%s\n' "$pod"
}

container_id()
{
	local namespace="$1" pod="$2" container="$3" id
	id="$(kubectl -n "$namespace" get pod "$pod" \
		-o jsonpath="{.status.containerStatuses[?(@.name==\"$container\")].containerID}")"
	id="${id#*://}"
	[[ "$id" =~ ^[0-9a-fA-F]{32,64}$ ]] || fail container-selector "invalid container ID for $pod/$container"
	printf '%s\n' "$id"
}

wait_for_restart()
{
	local namespace="$1" pod="$2" container="$3" before="$4" count=0 after
	while (( count < 450 )); do
		after="$(kubectl -n "$namespace" get pod "$pod" \
			-o jsonpath="{.status.containerStatuses[?(@.name==\"$container\")].restartCount}" 2>/dev/null || echo 0)"
		if [[ "$after" =~ ^[0-9]+$ ]] && (( after > before )); then return 0; fi
		sleep 0.1
		count=$((count + 1))
	done
	return 1
}

printf 'zccusan single-region reliability check\n'
printf 'context=%s namespace=%s claim=%s fault_seconds=%s\n\n' \
	"$context" "$NAMESPACE" "$CLAIM" "$FAULT_SECONDS"

kubectl -n "$NAMESPACE" get pvc "$CLAIM" >/dev/null \
	|| fail preflight "PVC $NAMESPACE/$CLAIM does not exist"
volume="$(kubectl -n "$NAMESPACE" get zcvolumes -o json \
	| jq -er --arg claim "$CLAIM" \
		'[.items[] | select(.spec.claimRef.name == $claim)] | if length == 1 then .[0].metadata.name else error("expected exactly one matching ZcVolume") end')" \
	|| fail preflight "could not resolve exactly one ZcVolume for PVC $CLAIM"
leaf="$(kubectl -n "$NAMESPACE" get pods \
	-l "storage.zcutils.io/volume=$volume,storage.zcutils.io/stage=terminal-leaf" \
	-o json | jq -er '.items | sort_by(.metadata.name) | .[0].metadata.name')" \
	|| fail preflight "no terminal storage worker found"
fan="$(kubectl -n "$NAMESPACE" get pods \
	-l "storage.zcutils.io/volume=$volume,storage.zcutils.io/stage=userspace-wal-mirror" \
	-o jsonpath='{.items[0].metadata.name}')"
[[ -n "$fan" ]] || fail preflight "no userspace mirror found"
pass preflight "volume=$volume storage_worker=$leaf mirror=$fan"

kubectl apply -f "$CANARY" >/dev/null
kubectl -n "$NAMESPACE" wait --for=condition=Ready pod/zc-single-region-ha-canary --timeout=120s >/dev/null \
	|| fail canary "did not become Ready"
baseline="$(pod_sequence zc-single-region-ha-canary canary_commit)"
[[ "$baseline" =~ ^[0-9]+$ ]] || fail canary "did not emit a commit"
wait_for_progress zc-single-region-ha-canary canary_commit "$baseline" baseline

before="$(pod_sequence zc-single-region-ha-canary canary_commit)"
operator_deployment="$(kubectl -n "$NAMESPACE" get deployment \
	-l app.kubernetes.io/component=operator -o jsonpath='{.items[0].metadata.name}')"
[[ -n "$operator_deployment" ]] || fail control-restart "operator Deployment not found"
kubectl -n "$NAMESPACE" delete pod -l app.kubernetes.io/component=operator --wait=false >/dev/null
kubectl -n "$NAMESPACE" rollout status "deployment/$operator_deployment" --timeout=120s >/dev/null \
	|| fail control-restart "operator did not recover"
wait_for_progress zc-single-region-ha-canary canary_commit "$before" control-restart

# First prove that the selected fault really reaches a normal host-path-backed
# container. This comparator is not a zccusan durability test.
kubectl apply -f "$COMPARATOR" >/dev/null
kubectl -n "$NAMESPACE" wait --for=condition=Ready pod/zc-single-region-ha-hostpath --timeout=120s >/dev/null \
	|| fail comparator "did not become Ready"
comparator_node="$(kubectl -n "$NAMESPACE" get pod zc-single-region-ha-hostpath -o jsonpath='{.spec.nodeName}')"
comparator_toolbox="$(toolbox_on_node "$comparator_node")"
comparator_id="$(container_id "$NAMESPACE" zc-single-region-ha-hostpath writer)"
comparator_restarts="$(kubectl -n "$NAMESPACE" get pod zc-single-region-ha-hostpath \
	-o jsonpath='{.status.containerStatuses[0].restartCount}')"
kubectl -n "$CHAOS_NAMESPACE" exec "$comparator_toolbox" -- \
	/usr/local/bin/zccusan-chaos-toolbox process-kill \
	--cgroup-contains "$comparator_id" --all --signal KILL \
	>"$tmp_dir/comparator-fault.ndjson"
grep -q '"event":"process_killed"' "$tmp_dir/comparator-fault.ndjson" \
	|| fail comparator "toolbox did not report a killed process"
wait_for_restart "$NAMESPACE" zc-single-region-ha-hostpath writer "$comparator_restarts" \
	|| fail comparator "Kubernetes did not observe the injected process failure"
pass comparator "fault reached host-path-backed control workload"

leaf_node="$(kubectl -n "$NAMESPACE" get pod "$leaf" -o jsonpath='{.spec.nodeName}')"
leaf_toolbox="$(toolbox_on_node "$leaf_node")"
leaf_id="$(container_id "$NAMESPACE" "$leaf" leaf)"
leaf_restarts="$(kubectl -n "$NAMESPACE" get pod "$leaf" \
	-o jsonpath='{.status.containerStatuses[0].restartCount}')"
before="$(pod_sequence zc-single-region-ha-canary canary_commit)"
kubectl -n "$CHAOS_NAMESPACE" exec "$leaf_toolbox" -- \
	/usr/local/bin/zccusan-chaos-toolbox process-kill \
	--cgroup-contains "$leaf_id" --all --signal KILL \
	>"$tmp_dir/storage-process-fault.ndjson"
grep -q '"event":"process_killed"' "$tmp_dir/storage-process-fault.ndjson" \
	|| fail storage-process "toolbox did not report a killed process"
wait_for_restart "$NAMESPACE" "$leaf" leaf "$leaf_restarts" \
	|| fail storage-process "storage worker was not restarted"
wait_for_progress zc-single-region-ha-canary canary_commit "$before" storage-process

leaf_ip="$(kubectl -n "$NAMESPACE" get pod "$leaf" \
	-o jsonpath='{.metadata.annotations.storage\.zcutils\.io/backplane-address}')"
[[ -n "$leaf_ip" ]] || leaf_ip="$(kubectl get node "$leaf_node" \
	-o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')"
leaf_port="$(kubectl -n "$NAMESPACE" get pod "$leaf" \
	-o jsonpath='{.spec.containers[?(@.name=="leaf")].ports[?(@.name=="wal")].hostPort}')"
fan_node="$(kubectl -n "$NAMESPACE" get pod "$fan" -o jsonpath='{.spec.nodeName}')"
fan_toolbox="$(toolbox_on_node "$fan_node")"
[[ -n "$leaf_ip" && "$leaf_port" =~ ^[0-9]+$ ]] \
	|| fail network-link "could not resolve the exact storage peer and port"
before="$(pod_sequence zc-single-region-ha-canary canary_commit)"
kubectl -n "$CHAOS_NAMESPACE" exec "$fan_toolbox" -- \
	/usr/local/bin/zccusan-chaos-toolbox network-blackhole \
	--experiment single-region-link --peer "$leaf_ip" --port "$leaf_port" \
	--duration-seconds "$FAULT_SECONDS" >"$tmp_dir/network-fault.ndjson"
grep -q '"event":"network_blackhole_applied"' "$tmp_dir/network-fault.ndjson" \
	|| fail network-link "bounded network fault was not applied"
grep -q '"event":"network_restored"' "$tmp_dir/network-fault.ndjson" \
	|| fail network-link "network fault was not restored"
wait_for_progress zc-single-region-ha-canary canary_commit "$before" network-link

printf '\nRESULT: PASS — every injected fault was observed and the mirrored-volume canary resumed valid commits.\n'
printf 'This result covers process restart and one bounded storage-link loss; it is not a power-loss durability claim.\n'
