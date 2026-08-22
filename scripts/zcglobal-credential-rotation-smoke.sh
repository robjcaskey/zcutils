#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${OUTDIR:-$(mktemp -d /tmp/zcglobal-credential-rotation.XXXXXX)}"
BIN="${ZCGLOBAL_POLICY_BIN:-$ROOT/target/debug/zcglobal-policy-node}"
PEERS='a@127.0.0.1:19971#leader,b@127.0.0.1:19972#leader,c@127.0.0.1:19973#leader'
TOKEN_FILE="$OUTDIR/admin-credentials.json"
PIDS=()

export ZCGLOBAL_FEDERATION_ID=rotation-smoke
export ZCGLOBAL_ADMIN_TOKEN_FILE="$TOKEN_FILE"
export ZCGLOBAL_ADMIN_MAX_TTL=30s
export ZCGLOBAL_ADMIN_ROTATE_BEFORE=10s
export ZCGLOBAL_ADMIN_RELOAD_INTERVAL=100ms
export ZCGLOBAL_ADMIN_ACTIVATION_CLOCK_SKEW=250ms
export ZCGLOBAL_ADMIN_MAX_VERSIONS=8

fail() { printf '[zcglobal-credential-rotation] FAIL: %s\n' "$*" >&2; exit 1; }

cleanup() {
	local status=$? pid
	trap - EXIT INT TERM
	touch "$OUTDIR/stop-probe"
	for pid in "${PIDS[@]:-}"; do
		[ -z "$pid" ] || kill "$pid" 2>/dev/null || true
	done
	for pid in "${PIDS[@]:-}"; do
		[ -z "$pid" ] || wait "$pid" 2>/dev/null || true
	done
	exit "$status"
}
trap cleanup EXIT INT TERM

status_node() { "$BIN" status "$1" 2>/dev/null; }

find_leader() {
	local address json
	for _ in $(seq 1 200); do
		for address in 127.0.0.1:19971 127.0.0.1:19972 127.0.0.1:19973; do
			json="$(status_node "$address" || true)"
			if [ "$(jq -r '.status.role // empty' <<<"$json")" = leader ]; then
				printf '%s\n' "$address"
				return 0
			fi
		done
		sleep 0.05
	done
	return 1
}

command -v jq >/dev/null || fail 'jq is required'
if [ -z "${ZCGLOBAL_POLICY_BIN:-}" ]; then
	cargo build --manifest-path "$ROOT/Cargo.toml" --bin zcglobal-policy-node
elif [ ! -x "$BIN" ]; then
	fail "ZCGLOBAL_POLICY_BIN is not executable: $BIN"
fi
mkdir -p "$OUTDIR"
"$BIN" credential-init "$TOKEN_FILE" 30s

for spec in 'a 127.0.0.1:19971' 'b 127.0.0.1:19972' 'c 127.0.0.1:19973'; do
	read -r node address <<<"$spec"
	"$BIN" serve rotation-smoke "$node" test-region "$address" \
		"$OUTDIR/$node-state.json" "$PEERS" "$TOKEN_FILE" \
		>"$OUTDIR/$node.log" 2>&1 &
	PIDS+=("$!")
done

leader="$(find_leader)" || fail 'no leader elected'
if [ "${ZCGLOBAL_MTLS_REJECTION_CHECK:-0}" = 1 ]; then
	[ "${ZCGLOBAL_RPC_TRANSPORT:-native-aead}" = native-aead+tls ] \
		|| fail 'ZCGLOBAL_MTLS_REJECTION_CHECK requires native-aead+tls'
	command -v openssl >/dev/null || fail 'openssl is required for the mTLS rejection check'
	set +e
	timeout 3 openssl s_client -connect "$leader" \
		-servername "${ZCGLOBAL_TLS_SERVER_NAME:?}" \
		-CAfile "${ZCGLOBAL_TLS_CA_FILE:?}" -tls1_3 </dev/null \
		>"$OUTDIR/no-client-certificate.log" 2>&1
	no_client_certificate_rc=$?
	set -e
	[ "$no_client_certificate_rc" -ne 0 ] \
		|| fail 'TLS accepted a connection without a client certificate'
	grep -Eq 'alert certificate required|certificate required' \
		"$OUTDIR/no-client-certificate.log" \
		|| fail 'TLS rejection did not report a required client certificate'
fi
rm -f "$OUTDIR/stop-probe" "$OUTDIR/probe-failures.log"
(
	while [ ! -e "$OUTDIR/stop-probe" ]; do
		status_node "$leader" >/dev/null || printf 'status failed at %s\n' "$(date +%s%3N)" >>"$OUTDIR/probe-failures.log"
		sleep 0.05
	done
) &
probe_pid="$!"
PIDS+=("$probe_pid")

for generation in 1 2 3; do
	sleep 10
	"$BIN" credential-rotate "$TOKEN_FILE" 30s
	"$BIN" set-network-trust "$leader" "$generation" untrusted untrusted \
		>"$OUTDIR/rotation-$generation.json"
	jq -e --argjson generation "$generation" \
		'.ok == true and .status.network_trust_policy.generation == $generation' \
		"$OUTDIR/rotation-$generation.json" >/dev/null \
		|| fail "rotation $generation did not preserve authenticated Raft mutation"
	credential_reloaded=false
	for _ in $(seq 1 20); do
		if status_node "$leader" | jq -e --argjson generation "$((generation + 1))" \
			'.status.management_credential.generation == $generation and .status.management_credential.rotation_due == false' \
			>/dev/null; then
			credential_reloaded=true
			break
		fi
		sleep 0.05
	done
	[ "$credential_reloaded" = true ] \
		|| fail "rotation $generation was not reloaded within the bounded cache interval"
done

touch "$OUTDIR/stop-probe"
wait "$probe_pid"
[ ! -s "$OUTDIR/probe-failures.log" ] || fail 'status stream observed an authentication gap'
"$BIN" credential-status "$TOKEN_FILE" >"$OUTDIR/final-credential-status.json"
jq -e '.generation == 4 and .accepted_versions >= 2 and .rotation_due == false' \
	"$OUTDIR/final-credential-status.json" >/dev/null \
	|| fail 'final overlapping credential set is invalid'

printf 'ZCGLOBAL_CREDENTIAL_ROTATION_PASS ttl=30s rotation_interval=10s rotations=3 auth_gaps=0 raft_mutations=3 rpc_transport=%s mtls_rejection_check=%s artifact=%s\n' \
	"${ZCGLOBAL_RPC_TRANSPORT:-native-aead}" "${ZCGLOBAL_MTLS_REJECTION_CHECK:-0}" "$OUTDIR"
