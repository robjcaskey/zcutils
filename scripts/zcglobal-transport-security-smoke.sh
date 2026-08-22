#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTDIR="${OUTDIR:-$(mktemp -d /tmp/zcglobal-transport-security.XXXXXX)}"
CERTDIR="$OUTDIR/identity"

fail() { printf '[zcglobal-transport-security] FAIL: %s\n' "$*" >&2; exit 1; }

command -v openssl >/dev/null || fail 'openssl is required'
command -v jq >/dev/null || fail 'jq is required'
mkdir -p "$CERTDIR"

openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 1 \
	-subj '/CN=zcglobal-smoke-ca' \
	-addext 'basicConstraints=critical,CA:TRUE' \
	-addext 'keyUsage=critical,keyCertSign,cRLSign' \
	-keyout "$CERTDIR/ca.key" -out "$CERTDIR/ca.pem" >/dev/null 2>&1
openssl req -new -newkey rsa:2048 -sha256 -nodes \
	-subj '/CN=localhost' \
	-addext 'subjectAltName=DNS:localhost,IP:127.0.0.1' \
	-addext 'basicConstraints=critical,CA:FALSE' \
	-addext 'keyUsage=critical,digitalSignature,keyEncipherment' \
	-addext 'extendedKeyUsage=serverAuth,clientAuth' \
	-keyout "$CERTDIR/identity.key" -out "$CERTDIR/identity.csr" >/dev/null 2>&1
openssl x509 -req -sha256 -days 1 -in "$CERTDIR/identity.csr" \
	-CA "$CERTDIR/ca.pem" -CAkey "$CERTDIR/ca.key" -CAcreateserial \
	-copy_extensions copy -out "$CERTDIR/identity.pem" >/dev/null 2>&1
chmod 600 "$CERTDIR/ca.key" "$CERTDIR/identity.key"

ZCGLOBAL_RPC_TRANSPORT=native-aead+tls \
ZCGLOBAL_TLS_CA_FILE="$CERTDIR/ca.pem" \
ZCGLOBAL_TLS_CERT_FILE="$CERTDIR/identity.pem" \
ZCGLOBAL_TLS_KEY_FILE="$CERTDIR/identity.key" \
ZCGLOBAL_TLS_SERVER_NAME=localhost \
ZCGLOBAL_MTLS_REJECTION_CHECK=1 \
OUTDIR="$OUTDIR/run" \
"$ROOT/scripts/zcglobal-credential-rotation-smoke.sh"

jq -e '
  .ok == true and
  .status.rpc_transport == "native-aead+tls" and
  .status.headline_performance_eligible == false
' "$OUTDIR/run/rotation-3.json" >/dev/null \
	|| fail 'TLS result metadata did not remain headline-ineligible'

printf 'ZCGLOBAL_TRANSPORT_SECURITY_PASS native_aead_inside_tls=yes tls_version=1.3 mtls_required=yes credential_rotations=3 headline_performance_eligible=false artifact=%s\n' "$OUTDIR"
