#!/usr/bin/env bash
set -euo pipefail

MODE=check
CACHE_VOLUME_ID="${CACHE_VOLUME_ID:-}"
CACHE_DEVICE="${CACHE_DEVICE:-}"
CACHE_MOUNT="${CACHE_MOUNT:-/mnt/zcutils-build-cache}"
ENABLE_SQUID=0
ALLOW_FORMAT=0
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  cat <<'EOF'
usage: configure-runner-build-cache.sh --volume-id vol-... --device /dev/disk/by-id/... [options]
  --check                 validate the volume and host only (default)
  --apply                 mount and configure native dependency caches
  --allow-format-empty    permit --apply to format only a volume with no filesystem
  --enable-squid          install loopback-only plain-HTTP Squid cache
  --mount PATH            cache mount (default /mnt/zcutils-build-cache)
EOF
}

while (($#)); do
  case "$1" in
    --check) MODE=check ;;
    --apply) MODE=apply ;;
    --allow-format-empty) ALLOW_FORMAT=1 ;;
    --enable-squid) ENABLE_SQUID=1 ;;
    --volume-id) shift; CACHE_VOLUME_ID="${1:-}" ;;
    --device) shift; CACHE_DEVICE="${1:-}" ;;
    --mount) shift; CACHE_MOUNT="${1:-}" ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

[[ "$CACHE_VOLUME_ID" =~ ^vol-[0-9a-f]{8}([0-9a-f]{9})?$ ]] || { echo 'a concrete EBS volume ID is required' >&2; exit 2; }
[[ "$CACHE_DEVICE" == /dev/disk/by-id/* ]] || { echo 'device must use a stable /dev/disk/by-id path' >&2; exit 2; }
[[ "$CACHE_MOUNT" =~ ^/[A-Za-z0-9._/-]+$ && "$CACHE_MOUNT" != / ]] || { echo 'unsafe cache mount path' >&2; exit 2; }
# shellcheck source=/dev/null
. /etc/os-release
[[ "${ID:-}" == amzn && "${VERSION_ID:-}" == 2023 ]] || { echo 'this bootstrap supports Amazon Linux 2023 only' >&2; exit 1; }
for command in aws curl python3 readlink; do command -v "$command" >/dev/null || { echo "missing command: $command" >&2; exit 1; }; done

token="$(curl -fsS -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 60' http://169.254.169.254/latest/api/token)"
metadata() { curl -fsS -H "X-aws-ec2-metadata-token: $token" "http://169.254.169.254/latest/meta-data/$1"; }
instance_id="$(metadata instance-id)"
instance_az="$(metadata placement/availability-zone)"
region="${instance_az::-1}"
volume_json="$(mktemp)"
trap 'rm -f "$volume_json"' EXIT
aws ec2 describe-volumes --region "$region" --volume-ids "$CACHE_VOLUME_ID" --output json >"$volume_json"
python3 - "$volume_json" "$CACHE_VOLUME_ID" "$instance_id" "$instance_az" <<'PY'
import json, sys
path, volume_id, instance_id, availability_zone = sys.argv[1:]
volumes = json.load(open(path, encoding="utf-8")).get("Volumes", [])
if len(volumes) != 1:
    raise SystemExit("expected exactly one cache volume")
volume = volumes[0]
expected = {"VolumeId": volume_id, "AvailabilityZone": availability_zone,
            "VolumeType": "gp3", "Size": 20, "Encrypted": True,
            "MultiAttachEnabled": False}
for key, value in expected.items():
    if volume.get(key) != value:
        raise SystemExit(f"cache volume {key} mismatch: {volume.get(key)!r} != {value!r}")
attachments = volume.get("Attachments", [])
if len(attachments) != 1 or attachments[0].get("InstanceId") != instance_id or attachments[0].get("State") != "attached":
    raise SystemExit("cache volume must be attached only to this runner")
tags = {item["Key"]: item["Value"] for item in volume.get("Tags", [])}
if tags.get("ZcutilsBuildCacheAuthority") != "Rob-J-Caskey" or tags.get("ZcutilsSingleWriter") != "true":
    raise SystemExit("cache volume is missing required authority/single-writer tags")
PY

resolved_device="$(readlink -f "$CACHE_DEVICE")"
[[ -b "$resolved_device" ]] || { echo 'cache device is not a block device' >&2; exit 1; }
if command -v nvme >/dev/null; then
  serial="$(nvme id-ctrl "$resolved_device" 2>/dev/null | awk -F: '/^sn / {gsub(/[[:space:]]/, "", $2); print $2}')"
  [[ "${serial//-/}" == "${CACHE_VOLUME_ID//-/}" ]] || { echo 'NVMe serial does not match requested EBS volume' >&2; exit 1; }
fi
[[ "$MODE" == apply ]] || { echo "CACHE_VOLUME_CHECK_PASS volume=$CACHE_VOLUME_ID instance=$instance_id az=$instance_az"; exit 0; }
[[ "$(id -u)" -eq 0 ]] || { echo '--apply requires root' >&2; exit 1; }
dnf -y install xfsprogs util-linux

filesystem="$(blkid -s TYPE -o value "$resolved_device" 2>/dev/null || true)"
if [[ -z "$filesystem" ]]; then
  [[ "$ALLOW_FORMAT" -eq 1 ]] || { echo 'volume is empty; rerun with --allow-format-empty only after reviewing the exact device' >&2; exit 1; }
  mkfs.xfs -L zcutils-build-cache "$resolved_device"
  filesystem=xfs
fi
[[ "$filesystem" == xfs ]] || { echo "expected xfs cache volume, found $filesystem" >&2; exit 1; }
uuid="$(blkid -s UUID -o value "$resolved_device")"
install -d -m 0755 "$CACHE_MOUNT"
if ! mountpoint -q "$CACHE_MOUNT"; then
  if ! grep -Fq "UUID=$uuid $CACHE_MOUNT " /etc/fstab; then
    printf 'UUID=%s %s xfs noatime,nofail 0 2\n' "$uuid" "$CACHE_MOUNT" >>/etc/fstab
  fi
  mount "$CACHE_MOUNT"
fi
install -d -m 0755 "$CACHE_MOUNT/cargo-registry" "$CACHE_MOUNT/cargo-git" "$CACHE_MOUNT/dnf" "$CACHE_MOUNT/squid"
if id gha >/dev/null 2>&1; then
  chown -R gha:gha "$CACHE_MOUNT/cargo-registry" "$CACHE_MOUNT/cargo-git"
  install -d -o gha -g gha -m 0755 /home/gha/.cargo
  for name in registry git; do
    [[ ! -e "/home/gha/.cargo/$name" && ! -L "/home/gha/.cargo/$name" ]] || { echo "refusing to replace /home/gha/.cargo/$name" >&2; exit 1; }
  done
  ln -s "$CACHE_MOUNT/cargo-registry" /home/gha/.cargo/registry
  ln -s "$CACHE_MOUNT/cargo-git" /home/gha/.cargo/git
fi
cat >/etc/zcutils-build-cache.env <<EOF
ZCUTILS_DNF_CACHE_DIR=$CACHE_MOUNT/dnf
EOF
chmod 0644 /etc/zcutils-build-cache.env
cat >/usr/local/bin/zcutils-dnf-cache <<EOF
#!/usr/bin/env bash
exec dnf --setopt=cachedir=$CACHE_MOUNT/dnf --setopt=keepcache=True "\$@"
EOF
chmod 0755 /usr/local/bin/zcutils-dnf-cache

if [[ "$ENABLE_SQUID" -eq 1 ]]; then
  dnf -y install squid
  sed "s|@CACHE_DIR@|$CACHE_MOUNT/squid|g" "$ROOT/config/squid-runner-build-cache.conf" >/etc/squid/squid.conf
  chown -R squid:squid "$CACHE_MOUNT/squid"
  squid -z
  systemctl enable --now squid
fi
echo "CACHE_VOLUME_APPLY_PASS volume=$CACHE_VOLUME_ID mount=$CACHE_MOUNT squid=$ENABLE_SQUID"
