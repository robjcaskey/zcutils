#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COORD_BIN="${AGENT_COORD_BIN:-$HOME/.local/bin/agent-coord}"
HOST_ARCH="${HOST_ARCH:-$(uname -m)}"
KERNEL_RELEASE="${KERNEL_RELEASE:-$(uname -r)}"
KERNEL="${KERNEL:-/boot/vmlinuz-$KERNEL_RELEASE}"
VIRTIO_BLK_MODULE="${VIRTIO_BLK_MODULE:-}"
WORK_DIR="${WORK_DIR:-$ROOT/target/qemu-zcvhost-user-blk-smoke}"
ROOTFS="$WORK_DIR/rootfs"
INITRAMFS="$WORK_DIR/initramfs.cpio"
LEAF="$WORK_DIR/terminal-leaf.raw"
SOCKET="$WORK_DIR/vhost-user-blk.sock"
BACKEND_LOG="$WORK_DIR/backend.log"
QEMU_LOG="$WORK_DIR/qemu.log"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/qemu-zcvhost-cargo}"
DEFAULT_BACKEND="$CARGO_TARGET_DIR/release/zcvhost-user-blk"
BACKEND="${BACKEND:-$DEFAULT_BACKEND}"
QEMU_MEM="${QEMU_MEM:-512M}"
QEMU_HUGETLB="${QEMU_HUGETLB:-0}"
QEMU_MEM_NODE="${QEMU_MEM_NODE:-}"
QEMU_MEM_POLICY="${QEMU_MEM_POLICY:-bind}"
QEMU_DUAL_NUMA="${QEMU_DUAL_NUMA:-0}"
# Ordered host-NUMA nodes for queue-aligned guest NUMA domains. For example,
# "2,1,0" with 24 vCPUs maps guest vCPUs 0-7 to host node 2, 8-15 to
# host node 1, and 16-23 to host node 0. EXPECTED_HCTX_CPUS and
# QEMU_VCPU_CPUS make the resulting queue/worker locality explicit; the
# frontend intentionally does not infer placement.
QEMU_NUMA_HOST_NODES="${QEMU_NUMA_HOST_NODES:-}"
QEMU_MEM_PER_NODE="${QEMU_MEM_PER_NODE:-}"
QEMU_SMP="${QEMU_SMP:-4}"
QEMU_CPUS="${QEMU_CPUS:-}"
QEMU_VCPU_CPUS="${QEMU_VCPU_CPUS:-}"
QEMU_PIN_LOG="${QEMU_PIN_LOG:-$WORK_DIR/qemu-vcpu-pinning.log}"
QEMU_QMP_SOCKET="${QEMU_QMP_SOCKET:-$WORK_DIR/qemu-qmp.sock}"
QEMU_PIDFILE="${QEMU_PIDFILE:-$WORK_DIR/qemu.pid}"
EXPECTED_HCTX_CPUS="${EXPECTED_HCTX_CPUS:-}"
QUEUES="${QUEUES:-4}"
QUEUE_SIZE="${QUEUE_SIZE:-512}"
QUEUE_CPUS="${QUEUE_CPUS:-}"
POLL_US="${POLL_US:-0}"
EVENT_IDX="${EVENT_IDX:-1}"
GUEST_MODE="${GUEST_MODE:-smoke}"
FIO_JOBS="${FIO_JOBS:-$QUEUES}"
FIO_QD="${FIO_QD:-128}"
FIO_RUNTIME="${FIO_RUNTIME:-10}"
FIO_RW="${FIO_RW:-randread}"
FIO_BS="${FIO_BS:-4k}"
FIO_SIZE="${FIO_SIZE:-48M}"
FIO_HIPRI="${FIO_HIPRI:-0}"
FIO_FINAL_SYNC="${FIO_FINAL_SYNC:-1}"
FIO_NOMERGES="${FIO_NOMERGES:-}"
if [[ -n "${FIO_BATCH_SUBMIT+x}" || -n "${FIO_BATCH_COMPLETE_MIN+x}" || -n "${FIO_BATCH_COMPLETE_MAX+x}" ]]; then
    [[ -n "${FIO_BATCH_SUBMIT+x}" && -n "${FIO_BATCH_COMPLETE_MIN+x}" && -n "${FIO_BATCH_COMPLETE_MAX+x}" ]] || {
        printf 'FIO_BATCH_SUBMIT, FIO_BATCH_COMPLETE_MIN, and FIO_BATCH_COMPLETE_MAX must be set together\n' >&2
        exit 2
    }
    FIO_BATCH_SOURCE=explicit
elif (( FIO_QD >= 128 )); then
    # On the dedicated nested-KVM saturation topology, fio's 1/1/1 defaults
    # left the guest doing tiny io_uring submissions. 128/32/128 preserved
    # throughput while materially improving p50 and p99.5 latency. Keep the
    # low-depth efficiency curve untouched below this saturation threshold.
    FIO_BATCH_SUBMIT=128
    FIO_BATCH_COMPLETE_MIN=32
    FIO_BATCH_COMPLETE_MAX=128
    FIO_BATCH_SOURCE=adaptive-saturation
else
    FIO_BATCH_SUBMIT=1
    FIO_BATCH_COMPLETE_MIN=1
    FIO_BATCH_COMPLETE_MAX=1
    FIO_BATCH_SOURCE=low-depth-conservative
fi
TIMEOUT="${TIMEOUT:-120s}"
BUILD="${BUILD:-1}"
ARENA_SOCKET="${ARENA_SOCKET:-}"
ZCNBLK_DEVICE="${ZCNBLK_DEVICE:-/dev/zcnblk0}"
DIRECT_OFI_ADDRESS="${DIRECT_OFI_ADDRESS:-}"
DIRECT_OFI_PROVIDER="${DIRECT_OFI_PROVIDER:-sockets}"
DIRECT_OFI_ENDPOINT="${DIRECT_OFI_ENDPOINT:-rdm}"
DIRECT_OFI_DOMAIN="${DIRECT_OFI_DOMAIN:-}"
DIRECT_OFI_BASE_SERVICE="${DIRECT_OFI_BASE_SERVICE:-37000}"
DIRECT_OFI_CAPACITY_BYTES="${DIRECT_OFI_CAPACITY_BYTES:-67108864}"
DIRECT_OFI_SLOT_BYTES="${DIRECT_OFI_SLOT_BYTES:-4096}"
DIRECT_OFI_REQUIRE_HUGETLB="${DIRECT_OFI_REQUIRE_HUGETLB:-0}"
BACKEND_PERF_SECONDS="${BACKEND_PERF_SECONDS:-0}"
BACKEND_PERF_FREQUENCY="${BACKEND_PERF_FREQUENCY:-997}"
BACKEND_PERF_OUTPUT="${BACKEND_PERF_OUTPUT:-$WORK_DIR/backend-perf.data}"
DATA_DESCRIPTORS_PER_REQUEST=3
EFFECTIVE_FIO_QD=$((QUEUE_SIZE / DATA_DESCRIPTORS_PER_REQUEST))
if (( FIO_QD < EFFECTIVE_FIO_QD )); then
    EFFECTIVE_FIO_QD="$FIO_QD"
fi
EFFECTIVE_AGGREGATE_QD=$((FIO_JOBS * EFFECTIVE_FIO_QD))

need() {
    command -v "$1" >/dev/null || {
        printf 'missing required command: %s\n' "$1" >&2
        exit 1
    }
}

if [[ "${ZCVHOST_QEMU_COORDINATED:-0}" != 1 ]]; then
    need "$COORD_BIN"
    exec "$COORD_BIN" run \
        --owner codex:zcutils-vhost-user-blk-qemu \
        --mode soft-exclusive --sensitivity high --priority 60 --ttl 600 \
        --resource 'cpu=*;memory-bandwidth=*;kvm=*' \
        --note 'stock-QEMU vhost-user-blk protocol smoke against terminal file leaf' \
        -- env ZCVHOST_QEMU_COORDINATED=1 "$0" "$@"
fi

case "$HOST_ARCH" in
    x86_64) QEMU_BIN=qemu-system-x86_64; QEMU_MACHINE=q35; QEMU_CPU_HOST=host; GUEST_CONSOLE=ttyS0 ;;
    aarch64|arm64) QEMU_BIN=qemu-system-aarch64; QEMU_MACHINE=virt; QEMU_CPU_HOST=max; GUEST_CONSOLE=ttyAMA0 ;;
    *) printf 'unsupported QEMU host architecture: %s\n' "$HOST_ARCH" >&2; exit 1 ;;
esac
if [[ -c /dev/kvm && -r /dev/kvm && -w /dev/kvm ]]; then
    QEMU_ACCEL=kvm
    QEMU_CPU="$QEMU_CPU_HOST"
else
    QEMU_ACCEL=tcg
    QEMU_CPU=max
fi

if [[ -z "$VIRTIO_BLK_MODULE" ]]; then
    for candidate in \
        "/lib/modules/$KERNEL_RELEASE/kernel/drivers/block/virtio_blk.ko" \
        "/lib/modules/$KERNEL_RELEASE/kernel/drivers/block/virtio_blk.ko.xz" \
        "/lib/modules/$KERNEL_RELEASE/kernel/drivers/block/virtio_blk.ko.zst"; do
        [[ -r "$candidate" ]] || continue
        VIRTIO_BLK_MODULE="$candidate"
        break
    done
fi

for command in cpio ldd readlink sha256sum stat "$QEMU_BIN" timeout; do
    need "$command"
done
case "$GUEST_MODE" in
    smoke) GUEST_INIT="$ROOT/scripts/zcvhost-user-blk-qemu-init.sh" ;;
    fio)
        GUEST_INIT="$ROOT/scripts/zcvhost-user-blk-qemu-fio-init.sh"
        need fio
        ;;
    *) printf 'invalid GUEST_MODE=%s (expected smoke or fio)\n' "$GUEST_MODE" >&2; exit 2 ;;
esac
if [[ "$GUEST_MODE" == fio ]] && (( EFFECTIVE_FIO_QD < FIO_QD )); then
    printf 'PERF WARNING: requested per-worker QD=%s exceeds the split-ring data-request ceiling=%s for queue_size=%s and descriptors_per_request=%s; effective aggregate depth is at most %s, not %s\n' \
        "$FIO_QD" "$EFFECTIVE_FIO_QD" "$QUEUE_SIZE" "$DATA_DESCRIPTORS_PER_REQUEST" \
        "$EFFECTIVE_AGGREGATE_QD" "$((FIO_JOBS * FIO_QD))" >&2
    if [[ "${URING_PLAY_TOPOLOGY_STRICT:-0}" == 1 || "${URING_PLAY_TOPOLOGY_FATAL:-0}" == 1 ]]; then
        printf 'strict topology rejects a truncated virtqueue depth before benchmark execution\n' >&2
        exit 2
    fi
fi
case "$FIO_FINAL_SYNC" in
    0|1) ;;
    *) printf 'invalid FIO_FINAL_SYNC=%s (expected 0 or 1)\n' "$FIO_FINAL_SYNC" >&2; exit 2 ;;
esac
case "$FIO_HIPRI" in
    0|1) ;;
    *) printf 'invalid FIO_HIPRI=%s (expected 0 or 1)\n' "$FIO_HIPRI" >&2; exit 2 ;;
esac
case "$FIO_NOMERGES" in
    ''|0|1|2) ;;
    *) printf 'invalid FIO_NOMERGES=%s (expected empty, 0, 1, or 2)\n' "$FIO_NOMERGES" >&2; exit 2 ;;
esac
for fio_batch_value in "$FIO_BATCH_SUBMIT" "$FIO_BATCH_COMPLETE_MIN" "$FIO_BATCH_COMPLETE_MAX"; do
    [[ "$fio_batch_value" =~ ^[1-9][0-9]*$ ]] || {
        printf 'invalid fio batch value=%s (expected a positive integer)\n' "$fio_batch_value" >&2
        exit 2
    }
done
(( FIO_BATCH_SUBMIT <= FIO_QD )) || {
    printf 'FIO_BATCH_SUBMIT=%s exceeds FIO_QD=%s\n' "$FIO_BATCH_SUBMIT" "$FIO_QD" >&2
    exit 2
}
(( FIO_BATCH_COMPLETE_MIN <= FIO_BATCH_COMPLETE_MAX && FIO_BATCH_COMPLETE_MAX <= FIO_QD )) || {
    printf 'fio completion batch must satisfy min <= max <= qd (min=%s max=%s qd=%s)\n' \
        "$FIO_BATCH_COMPLETE_MIN" "$FIO_BATCH_COMPLETE_MAX" "$FIO_QD" >&2
    exit 2
}
case "$QEMU_HUGETLB" in
    0|1) ;;
    *) printf 'invalid QEMU_HUGETLB=%s (expected 0 or 1)\n' "$QEMU_HUGETLB" >&2; exit 2 ;;
esac
case "$QEMU_MEM_POLICY" in
    bind|interleave|preferred) ;;
    *) printf 'invalid QEMU_MEM_POLICY=%s (expected bind, interleave, or preferred)\n' "$QEMU_MEM_POLICY" >&2; exit 2 ;;
esac
case "$QEMU_DUAL_NUMA" in
    0|1) ;;
    *) printf 'invalid QEMU_DUAL_NUMA=%s (expected 0 or 1)\n' "$QEMU_DUAL_NUMA" >&2; exit 2 ;;
esac
if [[ "$QEMU_DUAL_NUMA" == 1 && -n "$QEMU_NUMA_HOST_NODES" ]]; then
    printf 'QEMU_DUAL_NUMA and QEMU_NUMA_HOST_NODES are mutually exclusive\n' >&2
    exit 2
fi
qemu_vcpu_cpus=()
if [[ -n "$QEMU_VCPU_CPUS" ]]; then
    [[ "$QEMU_ACCEL" == kvm ]] || {
        printf 'QEMU_VCPU_CPUS requires KVM, not qemu_accel=%s\n' "$QEMU_ACCEL" >&2
        exit 2
    }
    IFS=, read -r -a qemu_vcpu_cpus <<<"$QEMU_VCPU_CPUS"
    (( ${#qemu_vcpu_cpus[@]} == QEMU_SMP )) || {
        printf 'QEMU_VCPU_CPUS entries=%s must equal QEMU_SMP=%s\n' \
            "${#qemu_vcpu_cpus[@]}" "$QEMU_SMP" >&2
        exit 2
    }
    declare -A seen_qemu_vcpu_cpus=()
    for qemu_vcpu_cpu in "${qemu_vcpu_cpus[@]}"; do
        [[ "$qemu_vcpu_cpu" =~ ^[0-9]+$ ]] || {
            printf 'invalid CPU in QEMU_VCPU_CPUS: %q\n' "$qemu_vcpu_cpu" >&2
            exit 2
        }
        [[ -d "/sys/devices/system/cpu/cpu$qemu_vcpu_cpu" ]] || {
            printf 'QEMU vCPU host CPU does not exist: %s\n' "$qemu_vcpu_cpu" >&2
            exit 2
        }
        [[ -z "${seen_qemu_vcpu_cpus[$qemu_vcpu_cpu]:-}" ]] || {
            printf 'duplicate host CPU in QEMU_VCPU_CPUS: %s\n' "$qemu_vcpu_cpu" >&2
            exit 2
        }
        seen_qemu_vcpu_cpus[$qemu_vcpu_cpu]=1
    done
    need taskset
    need nc
    need jq
    nc -h 2>&1 | grep -Eq '(^|[[:space:]])-U([[:space:]]|$)|\[-[^]]*U' || {
        printf 'QEMU_VCPU_CPUS requires netcat with Unix-domain socket support (-U)\n' >&2
        exit 2
    }
elif [[ "$GUEST_MODE" == fio && "$QEMU_ACCEL" == kvm && \
        ( "${URING_PLAY_TOPOLOGY_STRICT:-0}" == 1 || "${URING_PLAY_TOPOLOGY_FATAL:-0}" == 1 ) ]]; then
    printf 'strict KVM benchmark requires explicit QEMU_VCPU_CPUS; QEMU_CPUS is only a process-wide allowed set\n' >&2
    exit 2
fi
expected_hctx_cpus=()
if [[ -n "$EXPECTED_HCTX_CPUS" ]]; then
    IFS=, read -r -a expected_hctx_cpus <<<"$EXPECTED_HCTX_CPUS"
    (( ${#expected_hctx_cpus[@]} == QUEUES )) || {
        printf 'EXPECTED_HCTX_CPUS entries=%s must equal QUEUES=%s\n' \
            "${#expected_hctx_cpus[@]}" "$QUEUES" >&2
        exit 2
    }
    for expected_hctx_cpu in "${expected_hctx_cpus[@]}"; do
        [[ "$expected_hctx_cpu" =~ ^[0-9]+$ ]] || {
            printf 'invalid singleton CPU in EXPECTED_HCTX_CPUS: %q\n' "$expected_hctx_cpu" >&2
            exit 2
        }
    done
elif [[ "$GUEST_MODE" == fio && \
        ( "${URING_PLAY_TOPOLOGY_STRICT:-0}" == 1 || "${URING_PLAY_TOPOLOGY_FATAL:-0}" == 1 ) ]]; then
    printf 'strict QEMU block benchmark requires EXPECTED_HCTX_CPUS so virtqueue-to-guest-CPU mapping is verified\n' >&2
    exit 2
fi
[[ "$BACKEND_PERF_SECONDS" =~ ^[0-9]+$ ]] || {
    printf 'invalid BACKEND_PERF_SECONDS=%s (expected a non-negative integer)\n' "$BACKEND_PERF_SECONDS" >&2
    exit 2
}
[[ "$BACKEND_PERF_FREQUENCY" =~ ^[1-9][0-9]*$ ]] || {
    printf 'invalid BACKEND_PERF_FREQUENCY=%s (expected a positive integer)\n' "$BACKEND_PERF_FREQUENCY" >&2
    exit 2
}
if (( BACKEND_PERF_SECONDS > 0 )); then
    need perf
fi
qemu_numa_nodes_spec="$QEMU_NUMA_HOST_NODES"
if [[ "$QEMU_DUAL_NUMA" == 1 ]]; then
    qemu_numa_nodes_spec=0,1
fi
qemu_numa_host_nodes=()
if [[ -n "$qemu_numa_nodes_spec" ]]; then
    [[ -n "$QEMU_MEM_PER_NODE" ]] || {
        printf 'queue-aligned guest NUMA requires QEMU_MEM_PER_NODE\n' >&2
        exit 2
    }
    IFS=, read -r -a qemu_numa_host_nodes <<<"$qemu_numa_nodes_spec"
    (( ${#qemu_numa_host_nodes[@]} >= 2 )) || {
        printf 'queue-aligned guest NUMA requires at least two host nodes\n' >&2
        exit 2
    }
    (( QEMU_SMP >= ${#qemu_numa_host_nodes[@]} && QEMU_SMP % ${#qemu_numa_host_nodes[@]} == 0 )) || {
        printf 'QEMU_SMP=%s must be evenly divisible by NUMA domain count=%s\n' \
            "$QEMU_SMP" "${#qemu_numa_host_nodes[@]}" >&2
        exit 2
    }
    [[ -z "$QEMU_MEM_NODE" ]] || {
        printf 'QEMU_MEM_NODE and queue-aligned guest NUMA are mutually exclusive\n' >&2
        exit 2
    }
    declare -A seen_qemu_numa_nodes=()
    for qemu_numa_node in "${qemu_numa_host_nodes[@]}"; do
        [[ "$qemu_numa_node" =~ ^[0-9]+$ ]] || {
            printf 'invalid host NUMA node in QEMU_NUMA_HOST_NODES: %q\n' "$qemu_numa_node" >&2
            exit 2
        }
        [[ -d "/sys/devices/system/node/node$qemu_numa_node" ]] || {
            printf 'host NUMA node does not exist: %s\n' "$qemu_numa_node" >&2
            exit 2
        }
        [[ -z "${seen_qemu_numa_nodes[$qemu_numa_node]:-}" ]] || {
            printf 'duplicate host NUMA node in queue-aligned mapping: %s\n' "$qemu_numa_node" >&2
            exit 2
        }
        seen_qemu_numa_nodes[$qemu_numa_node]=1
    done
fi
strict_topology=0
if [[ "${URING_PLAY_TOPOLOGY_STRICT:-0}" == 1 || "${URING_PLAY_TOPOLOGY_FATAL:-0}" == 1 ]]; then
    strict_topology=1
fi
queue_cpus=()
if [[ -n "$QUEUE_CPUS" ]]; then
    IFS=, read -r -a queue_cpus <<<"$QUEUE_CPUS"
fi
if [[ "$GUEST_MODE" == fio && "$strict_topology" == 1 ]]; then
    [[ "$QEMU_HUGETLB" == 1 ]] || {
        printf 'strict QEMU benchmark requires QEMU_HUGETLB=1\n' >&2
        exit 2
    }
    qemu_size_kib() {
        local size_spec=${1^^} magnitude suffix
        if [[ "$size_spec" =~ ^([0-9]+)([KMGT]?)(I?B)?$ ]]; then
            magnitude="${BASH_REMATCH[1]}"
            suffix="${BASH_REMATCH[2]}"
            case "$suffix" in
                '') printf '%s' "$((magnitude * 1024))" ;;
                K) printf '%s' "$magnitude" ;;
                M) printf '%s' "$((magnitude * 1024))" ;;
                G) printf '%s' "$((magnitude * 1024 * 1024))" ;;
                T) printf '%s' "$((magnitude * 1024 * 1024 * 1024))" ;;
            esac
            return 0
        fi
        return 1
    }
    qemu_mem_kib="$(qemu_size_kib "$QEMU_MEM")" || {
        printf 'strict QEMU benchmark cannot parse QEMU_MEM=%q for memlock preflight\n' "$QEMU_MEM" >&2
        exit 2
    }
    memlock_required_kib=$((qemu_mem_kib + qemu_mem_kib / 10 + 65536))
    memlock_actual_kib="$(ulimit -l)"
    if [[ "$memlock_actual_kib" != unlimited ]]; then
        [[ "$memlock_actual_kib" =~ ^[0-9]+$ && "$memlock_actual_kib" -ge "$memlock_required_kib" ]] || {
            printf 'strict QEMU benchmark has insufficient memlock: actual_kib=%s required_kib=%s qemu_mem=%s\n' \
                "$memlock_actual_kib" "$memlock_required_kib" "$QEMU_MEM" >&2
            exit 2
        }
    fi
    qemu_memlock_proof="actual_kib=$memlock_actual_kib,required_kib=$memlock_required_kib"
    (( ${#queue_cpus[@]} == QUEUES )) || {
        printf 'strict QEMU benchmark requires one QUEUE_CPUS entry per queue; entries=%s queues=%s\n' \
            "${#queue_cpus[@]}" "$QUEUES" >&2
        exit 2
    }
    (( ${#qemu_numa_host_nodes[@]} > 0 )) || {
        printf 'strict QEMU benchmark requires explicit queue-aligned guest NUMA memory\n' >&2
        exit 2
    }

    cpu_numa_node() {
        local cpu=$1 node_path
        for node_path in "/sys/devices/system/cpu/cpu$cpu"/node[0-9]*; do
            [[ -d "$node_path" ]] || continue
            printf '%s' "${node_path##*node}"
            return 0
        done
        return 1
    }

    vcpus_per_numa_preflight=$((QEMU_SMP / ${#qemu_numa_host_nodes[@]}))
    for ((hctx_index = 0; hctx_index < QUEUES; hctx_index++)); do
        guest_cpu="${expected_hctx_cpus[$hctx_index]}"
        (( guest_cpu < QEMU_SMP )) || {
            printf 'hctx=%s references guest_cpu=%s outside QEMU_SMP=%s\n' \
                "$hctx_index" "$guest_cpu" "$QEMU_SMP" >&2
            exit 2
        }
        backend_cpu="${queue_cpus[$hctx_index]}"
        vcpu_host_cpu="${qemu_vcpu_cpus[$guest_cpu]}"
        backend_numa_node="$(cpu_numa_node "$backend_cpu")" || {
            printf 'cannot resolve backend_cpu=%s NUMA node for hctx=%s\n' "$backend_cpu" "$hctx_index" >&2
            exit 2
        }
        vcpu_numa_node="$(cpu_numa_node "$vcpu_host_cpu")" || {
            printf 'cannot resolve vcpu_host_cpu=%s NUMA node for guest_cpu=%s\n' "$vcpu_host_cpu" "$guest_cpu" >&2
            exit 2
        }
        guest_numa_domain=$((guest_cpu / vcpus_per_numa_preflight))
        memory_numa_node="${qemu_numa_host_nodes[$guest_numa_domain]}"
        if [[ "$backend_numa_node" != "$vcpu_numa_node" || \
              "$backend_numa_node" != "$memory_numa_node" ]]; then
            printf 'strict lane locality mismatch: hctx=%s backend_cpu=%s backend_node=%s guest_cpu=%s vcpu_host_cpu=%s vcpu_node=%s memory_node=%s\n' \
                "$hctx_index" "$backend_cpu" "$backend_numa_node" "$guest_cpu" \
                "$vcpu_host_cpu" "$vcpu_numa_node" "$memory_numa_node" >&2
            exit 2
        fi
    done
    qemu_lane_locality=verified-hctx-backend-vcpu-memory
else
    qemu_lane_locality=not-strictly-verified
    qemu_memlock_proof=not-strictly-verified
fi
if [[ "$BUILD" == 1 ]]; then
    need cargo
fi
[[ -r "$KERNEL" ]] || { printf 'kernel is not readable: %s\n' "$KERNEL" >&2; exit 1; }
if [[ -z "$VIRTIO_BLK_MODULE" ]]; then
    grep -q '^CONFIG_VIRTIO_BLK=y$' "/boot/config-$KERNEL_RELEASE" || {
        printf 'virtio_blk is neither built in nor available as a module\n' >&2
        exit 1
    }
elif [[ ! -r "$VIRTIO_BLK_MODULE" ]]; then
    printf 'virtio_blk module is not readable: %s\n' "$VIRTIO_BLK_MODULE" >&2
    exit 1
fi
[[ -r "$GUEST_INIT" ]]

if [[ "$BUILD" == 1 ]]; then
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release --bin zcvhost-user-blk
    if [[ "$BACKEND" != "$DEFAULT_BACKEND" ]]; then
        printf 'BUILD=1 compiled %s but BACKEND resolves to the separate executable %s; use BUILD=0 for an intentionally prebuilt override\n' \
            "$DEFAULT_BACKEND" "$BACKEND" >&2
        exit 2
    fi
fi
[[ -x "$BACKEND" ]] || { printf 'backend is not executable: %s\n' "$BACKEND" >&2; exit 1; }
BACKEND_RESOLVED="$(readlink -f -- "$BACKEND")"
BACKEND_SHA256="$(sha256sum -- "$BACKEND_RESOLVED")"
BACKEND_SHA256="${BACKEND_SHA256%% *}"
BACKEND_SIZE_BYTES="$(stat -c %s -- "$BACKEND_RESOLVED")"
BACKEND_MTIME="$(stat -c %y -- "$BACKEND_RESOLVED")"

mkdir -p "$WORK_DIR"
if [[ -d "$ROOTFS" ]]; then
    rm -rf -- "$ROOTFS"
fi
mkdir -p "$ROOTFS/bin" "$ROOTFS/lib" "$ROOTFS/lib64" "$ROOTFS/modules" \
    "$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/tmp"
cp /usr/bin/busybox "$ROOTFS/bin/busybox"
for applet in awk cat cmp cut dd echo insmod mkdir mount poweroff sh sleep sync; do
    ln -s busybox "$ROOTFS/bin/$applet"
done
cp "$GUEST_INIT" "$ROOTFS/init"
chmod +x "$ROOTFS/init"
if [[ "$GUEST_MODE" == fio ]]; then
    mkdir -p "$ROOTFS/usr/bin"
    cp "$(command -v fio)" "$ROOTFS/usr/bin/fio"
fi
if [[ -n "$VIRTIO_BLK_MODULE" ]]; then
    case "$VIRTIO_BLK_MODULE" in
        *.xz) xz -dc -- "$VIRTIO_BLK_MODULE" > "$ROOTFS/modules/virtio_blk.ko" ;;
        *.zst) zstd -dc -- "$VIRTIO_BLK_MODULE" > "$ROOTFS/modules/virtio_blk.ko" ;;
        *) cp "$VIRTIO_BLK_MODULE" "$ROOTFS/modules/virtio_blk.ko" ;;
    esac
fi

while IFS= read -r library; do
    [[ -n "$library" ]] || continue
    mkdir -p "$ROOTFS$(dirname "$library")"
    cp "$library" "$ROOTFS$library"
done < <(
    {
        # Ubuntu's busybox-static makes ldd return non-zero.  Do not let that
        # short-circuit the fio dependency scan in this process substitution.
        ldd /usr/bin/busybox || true
        if [[ "$GUEST_MODE" == fio ]]; then
            ldd "$(command -v fio)"
        fi
    } | awk '
        /=> \// { print $3; next }
        /^[[:space:]]*\/lib/ { print $1; next }
    ' | sort -u
)

(
    cd "$ROOTFS"
    find . -print0 | cpio --null -o --format=newc > "$INITRAMFS"
)
if [[ -n "$ARENA_SOCKET" && -n "$DIRECT_OFI_ADDRESS" ]]; then
    printf 'ARENA_SOCKET and DIRECT_OFI_ADDRESS are mutually exclusive\n' >&2
    exit 2
fi
if [[ -z "$ARENA_SOCKET" && -z "$DIRECT_OFI_ADDRESS" ]]; then
    truncate -s 64M "$LEAF"
fi
rm -f -- "$SOCKET" "$BACKEND_LOG" "$QEMU_LOG" "$QEMU_PIN_LOG" "$QEMU_QMP_SOCKET" "$QEMU_PIDFILE"
printf 'zcvhost-backend-provenance: path=%s sha256=%s size_bytes=%s mtime=%q build=%s cargo_target_dir=%s\n' \
    "$BACKEND_RESOLVED" "$BACKEND_SHA256" "$BACKEND_SIZE_BYTES" "$BACKEND_MTIME" \
    "$BUILD" "$CARGO_TARGET_DIR" | tee -a "$QEMU_LOG"

backend_pid=''
profile_pid=''
pin_pid=''
cleanup() {
    if [[ -n "$pin_pid" ]] && kill -0 "$pin_pid" 2>/dev/null; then
        kill "$pin_pid"
        wait "$pin_pid" 2>/dev/null || true
    fi
    if [[ -n "$profile_pid" ]] && kill -0 "$profile_pid" 2>/dev/null; then
        kill "$profile_pid"
        wait "$profile_pid" 2>/dev/null || true
    fi
    if [[ -n "$backend_pid" ]] && kill -0 "$backend_pid" 2>/dev/null; then
        kill "$backend_pid"
        wait "$backend_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT

backend_args=(--socket "$SOCKET" --queues "$QUEUES" --queue-size "$QUEUE_SIZE" --poll-us "$POLL_US")
if [[ "$EVENT_IDX" == 0 ]]; then
    backend_args+=(--no-event-idx)
elif [[ "$EVENT_IDX" != 1 ]]; then
    printf 'invalid EVENT_IDX=%s (expected 0 or 1)\n' "$EVENT_IDX" >&2
    exit 2
fi
if [[ -n "$QUEUE_CPUS" ]]; then
    backend_args+=(--queue-cpus "$QUEUE_CPUS")
fi
if [[ -n "$DIRECT_OFI_ADDRESS" ]]; then
    backend_args+=(
        --direct-ofi "$DIRECT_OFI_ADDRESS"
        --direct-provider "$DIRECT_OFI_PROVIDER"
        --direct-endpoint "$DIRECT_OFI_ENDPOINT"
        --direct-base-service "$DIRECT_OFI_BASE_SERVICE"
        --direct-capacity-bytes "$DIRECT_OFI_CAPACITY_BYTES"
        --direct-slot-bytes "$DIRECT_OFI_SLOT_BYTES"
    )
    if [[ -n "$DIRECT_OFI_DOMAIN" ]]; then
        backend_args+=(--direct-domain "$DIRECT_OFI_DOMAIN")
    fi
    if [[ "$DIRECT_OFI_REQUIRE_HUGETLB" == 1 ]]; then
        backend_args+=(--direct-require-hugetlb)
    elif [[ "$DIRECT_OFI_REQUIRE_HUGETLB" != 0 ]]; then
        printf 'invalid DIRECT_OFI_REQUIRE_HUGETLB=%s (expected 0 or 1)\n' "$DIRECT_OFI_REQUIRE_HUGETLB" >&2
        exit 2
    fi
elif [[ -n "$ARENA_SOCKET" ]]; then
    backend_args+=(--arena-socket "$ARENA_SOCKET" --zcnblk-device "$ZCNBLK_DEVICE")
else
    backend_args+=(--leaf-file "$LEAF")
fi
"$BACKEND" "${backend_args[@]}" >"$BACKEND_LOG" 2>&1 &
backend_pid=$!

for _ in $(seq 1 100); do
    [[ -S "$SOCKET" ]] && break
    kill -0 "$backend_pid" 2>/dev/null || {
        printf 'backend exited before creating its socket\n' >&2
        sed -n '1,240p' "$BACKEND_LOG" >&2
        exit 1
    }
    sleep 0.05
done
[[ -S "$SOCKET" ]] || { printf 'backend socket was not created\n' >&2; exit 1; }

profile_backend_workers() {
    local attempt task comm worker_tids_csv
    local -a worker_tids=()

    # Queue workers appear only after QEMU negotiates the vhost device. Resolve
    # them through this backend's exact PID so unrelated QEMU sessions cannot
    # enter the recording.
    for attempt in $(seq 1 400); do
        [[ -d "/proc/$backend_pid/task" ]] || return 1
        worker_tids=()
        for task in "/proc/$backend_pid"/task/[0-9]*; do
            [[ -r "$task/comm" ]] || continue
            read -r comm <"$task/comm"
            [[ "$comm" == vring_worker ]] || continue
            worker_tids+=("${task##*/}")
        done
        (( ${#worker_tids[@]} >= QUEUES )) && break
        sleep 0.025
    done
    if (( ${#worker_tids[@]} < QUEUES )); then
        printf 'backend profiler found %s/%s vring workers under pid %s\n' \
            "${#worker_tids[@]}" "$QUEUES" "$backend_pid" >&2
        return 1
    fi
    worker_tids_csv="$(IFS=,; printf '%s' "${worker_tids[*]}")"
    mkdir -p "$(dirname "$BACKEND_PERF_OUTPUT")"
    printf 'zcvhost-backend-profile: exact_backend_pid=%s exact_vring_worker_tids=%s frequency_hz=%s seconds=%s output=%s\n' \
        "$backend_pid" "$worker_tids_csv" "$BACKEND_PERF_FREQUENCY" \
        "$BACKEND_PERF_SECONDS" "$BACKEND_PERF_OUTPUT" | tee -a "$QEMU_LOG"
    exec perf record --quiet -F "$BACKEND_PERF_FREQUENCY" -e cycles:u -g \
        --tid "$worker_tids_csv" -o "$BACKEND_PERF_OUTPUT" -- \
        sleep "$BACKEND_PERF_SECONDS"
}

if (( BACKEND_PERF_SECONDS > 0 )); then
    profile_backend_workers &
    profile_pid=$!
fi

pin_qemu_vcpus() {
    local attempt qemu_pid task vcpu_index vcpu_tid expected_cpu allowed_cpu qmp_reply
    local -a qmp_vcpu_rows=()
    local -a vcpu_tid_by_index=()

    for attempt in $(seq 1 400); do
        if [[ -s "$QEMU_PIDFILE" ]]; then
            read -r qemu_pid <"$QEMU_PIDFILE"
            [[ "$qemu_pid" =~ ^[0-9]+$ && -d "/proc/$qemu_pid/task" ]] && break
        fi
        sleep 0.01
    done
    if [[ -z "${qemu_pid:-}" || ! -d "/proc/$qemu_pid/task" ]]; then
        printf 'qemu-vcpu-pin: QEMU PID did not appear\n' >&2
        return 1
    fi

    for attempt in $(seq 1 400); do
        [[ -S "$QEMU_QMP_SOCKET" ]] && break
        sleep 0.01
    done
    if [[ ! -S "$QEMU_QMP_SOCKET" ]]; then
        printf 'qemu-vcpu-pin: QMP socket did not appear: %s\n' "$QEMU_QMP_SOCKET" >&2
        kill "$qemu_pid" 2>/dev/null || true
        return 1
    fi

    qmp_reply="$({
        printf '%s\n' '{"execute":"qmp_capabilities"}' '{"execute":"query-cpus-fast"}'
    } | nc -U -N -w 2 "$QEMU_QMP_SOCKET" 2>&1 || true)"
    printf 'qemu-vcpu-pin: query_cpus_reply=%q\n' "$qmp_reply"
    mapfile -t qmp_vcpu_rows < <(
        jq -r 'select(.return | type == "array") | .return[] | [."cpu-index", ."thread-id"] | @tsv' \
            <<<"$qmp_reply" 2>/dev/null
    )
    for qmp_vcpu_row in "${qmp_vcpu_rows[@]}"; do
        IFS=$'\t' read -r vcpu_index vcpu_tid <<<"$qmp_vcpu_row"
        [[ "$vcpu_index" =~ ^[0-9]+$ && "$vcpu_tid" =~ ^[0-9]+$ ]] || continue
        (( vcpu_index < QEMU_SMP )) || continue
        vcpu_tid_by_index[$vcpu_index]="$vcpu_tid"
    done
    if (( ${#vcpu_tid_by_index[@]} != QEMU_SMP )); then
        printf 'qemu-vcpu-pin: QMP reported %s/%s vCPU thread IDs under qemu_pid=%s\n' \
            "${#vcpu_tid_by_index[@]}" "$QEMU_SMP" "$qemu_pid" >&2
        kill "$qemu_pid" 2>/dev/null || true
        return 1
    fi

    for ((vcpu_index = 0; vcpu_index < QEMU_SMP; vcpu_index++)); do
        expected_cpu="${qemu_vcpu_cpus[$vcpu_index]}"
        task="${vcpu_tid_by_index[$vcpu_index]}"
        if ! taskset -pc "$expected_cpu" "$task"; then
            kill "$qemu_pid" 2>/dev/null || true
            return 1
        fi
        allowed_cpu="$(awk '/^Cpus_allowed_list:/ { print $2 }' "/proc/$task/status")"
        if [[ "$allowed_cpu" != "$expected_cpu" ]]; then
            printf 'qemu-vcpu-pin: vcpu=%s tid=%s expected_cpu=%s actual_allowed=%s\n' \
                "$vcpu_index" "$task" "$expected_cpu" "$allowed_cpu" >&2
            kill "$qemu_pid" 2>/dev/null || true
            return 1
        fi
        printf 'qemu-vcpu-pin: vcpu=%s tid=%s host_cpu=%s verified=true\n' \
            "$vcpu_index" "$task" "$expected_cpu"
    done

    qmp_reply="$({
        printf '%s\n' '{"execute":"qmp_capabilities"}' '{"execute":"cont"}' '{"execute":"query-status"}'
    } | nc -U -N -w 2 "$QEMU_QMP_SOCKET" 2>&1 || true)"
    printf 'qemu-vcpu-pin: qmp_reply=%q\n' "$qmp_reply"
    if ! grep -Eq '"status"[[:space:]]*:[[:space:]]*"running"' <<<"$qmp_reply"; then
        printf 'qemu-vcpu-pin: QMP did not confirm running state\n' >&2
        kill "$qemu_pid" 2>/dev/null || true
        return 1
    fi
}

qemu_memory_args=()
qemu_memory_topology="${QEMU_MEM_NODE:-default}"
if (( ${#qemu_numa_host_nodes[@]} > 0 )); then
    vcpus_per_numa=$((QEMU_SMP / ${#qemu_numa_host_nodes[@]}))
    qemu_memory_topology="queue-aligned-numa:"
    for numa_index in "${!qemu_numa_host_nodes[@]}"; do
        host_numa_node="${qemu_numa_host_nodes[$numa_index]}"
        first_vcpu=$((numa_index * vcpus_per_numa))
        last_vcpu=$((first_vcpu + vcpus_per_numa - 1))
        memory_object="memory-backend-memfd,id=guestmem$numa_index,size=$QEMU_MEM_PER_NODE,share=on,host-nodes=$host_numa_node,policy=bind,prealloc=on,prealloc-context=prealloc$numa_index"
        if [[ "$QEMU_HUGETLB" == 1 ]]; then
            memory_object+=",hugetlb=on,hugetlbsize=2M"
        fi
        qemu_memory_args+=(
            -object "thread-context,id=prealloc$numa_index,node-affinity=$host_numa_node"
            -object "$memory_object"
            -numa "node,nodeid=$numa_index,cpus=$first_vcpu-$last_vcpu,memdev=guestmem$numa_index"
        )
        [[ "$numa_index" == 0 ]] || qemu_memory_topology+=","
        qemu_memory_topology+="guest$numa_index(vcpus$first_vcpu-$last_vcpu)-host$host_numa_node"
    done
else
    memory_object="memory-backend-memfd,id=guestmem,size=$QEMU_MEM,share=on"
    if [[ "$QEMU_HUGETLB" == 1 ]]; then
        memory_object+=",hugetlb=on,hugetlbsize=2M,prealloc=on"
    fi
    if [[ -n "$QEMU_MEM_NODE" ]]; then
        memory_object+=",host-nodes=$QEMU_MEM_NODE,policy=$QEMU_MEM_POLICY"
    fi
    qemu_memory_args=(-object "$memory_object" -numa node,memdev=guestmem)
fi
if [[ "$GUEST_MODE" == fio ]]; then
    printf 'zcvhost-qemu-topology: guest_jobs=%s requested_per_worker_qd=%s effective_per_worker_qd=%s requested_aggregate_outstanding_depth=%s effective_aggregate_outstanding_depth=%s descriptors_per_data_request=%s virtqueue_size=%s guest_job_cpu_map=jobN:vcpuN guest_hctx_cpu_map=%s guest_hctx_verification=%s guest_batch_submit=%s guest_batch_complete_min=%s guest_batch_complete_max=%s guest_batch_source=%s guest_nomerges=%s virtqueues=%s backend_queue_cpu_map=%s lane_locality=%s backend_poll_us=%s event_idx=%s qemu_host_cpu_set=%s qemu_vcpu_cpu_map=%s qemu_vcpu_pinning=%s qemu_memory_node=%s qemu_memory_policy=%s qemu_accel=%s qemu_hugetlb=%s qemu_memlock=%s final_sync=%s completion_semantics=%s\n' \
        "$FIO_JOBS" "$FIO_QD" "$EFFECTIVE_FIO_QD" "$((FIO_JOBS * FIO_QD))" \
        "$EFFECTIVE_AGGREGATE_QD" "$DATA_DESCRIPTORS_PER_REQUEST" "$QUEUE_SIZE" \
        "${EXPECTED_HCTX_CPUS:-unmapped}" "$([[ -n "$EXPECTED_HCTX_CPUS" ]] && printf guest-preflight || printf observational-only)" \
        "$FIO_BATCH_SUBMIT" "$FIO_BATCH_COMPLETE_MIN" "$FIO_BATCH_COMPLETE_MAX" "$FIO_BATCH_SOURCE" "${FIO_NOMERGES:-kernel-default}" "$QUEUES" \
        "${QUEUE_CPUS:-unmapped}" "$qemu_lane_locality" "$POLL_US" "$EVENT_IDX" "${QEMU_CPUS:-unmapped}" "${QEMU_VCPU_CPUS:-unmapped}" \
        "$([[ -n "$QEMU_VCPU_CPUS" ]] && printf verified-before-vm-resume || printf process-cpuset-only)" \
        "$qemu_memory_topology" "$QEMU_MEM_POLICY" "$QEMU_ACCEL" "$QEMU_HUGETLB" "$qemu_memlock_proof" "$FIO_FINAL_SYNC" \
        "$([[ "$FIO_FINAL_SYNC" == 1 ]] && printf ordinary-device-ack+final-sync-drain || printf ordinary-device-ack-only)" | tee -a "$QEMU_LOG"
fi

qemu_prefix=()
if [[ -n "$QEMU_CPUS" ]]; then
    need taskset
    qemu_prefix=(taskset -c "$QEMU_CPUS")
fi
qemu_control_args=()
if [[ -n "$QEMU_VCPU_CPUS" ]]; then
    qemu_control_args=(-S -qmp "unix:$QEMU_QMP_SOCKET,server=on,wait=off" -pidfile "$QEMU_PIDFILE")
    pin_qemu_vcpus >"$QEMU_PIN_LOG" 2>&1 &
    pin_pid=$!
fi
# Publish exact artifact paths before QEMU starts. The enclosing block harness
# can then preserve the backend/QEMU/pinning evidence even when a timeout or
# guest failure is the result under investigation.
printf 'guest_log=%s\nbackend_log=%s\nqemu_vcpu_pin_log=%s\n' \
    "$QEMU_LOG" "$BACKEND_LOG" "$QEMU_PIN_LOG"
set +e
timeout "$TIMEOUT" "${qemu_prefix[@]}" "$QEMU_BIN" \
    -machine "$QEMU_MACHINE,accel=$QEMU_ACCEL" \
    -cpu "$QEMU_CPU" \
    -m "$QEMU_MEM" \
    -smp "$QEMU_SMP" \
    "${qemu_memory_args[@]}" \
    "${qemu_control_args[@]}" \
    -nodefaults \
    -nographic \
    -no-reboot \
    -serial mon:stdio \
    -kernel "$KERNEL" \
    -initrd "$INITRAMFS" \
    -append "console=$GUEST_CONSOLE panic=-1 oops=panic quiet zc_fio_jobs=$FIO_JOBS zc_fio_qd=$FIO_QD zc_fio_effective_qd=$EFFECTIVE_FIO_QD zc_fio_effective_aggregate_qd=$EFFECTIVE_AGGREGATE_QD zc_virtqueue_size=$QUEUE_SIZE zc_expected_hctx_cpus=$EXPECTED_HCTX_CPUS zc_fio_runtime=$FIO_RUNTIME zc_fio_rw=$FIO_RW zc_fio_bs=$FIO_BS zc_fio_size=$FIO_SIZE zc_fio_hipri=$FIO_HIPRI zc_fio_final_sync=$FIO_FINAL_SYNC zc_fio_batch_submit=$FIO_BATCH_SUBMIT zc_fio_batch_complete_min=$FIO_BATCH_COMPLETE_MIN zc_fio_batch_complete_max=$FIO_BATCH_COMPLETE_MAX zc_fio_nomerges=$FIO_NOMERGES" \
    -chardev "socket,id=zcblk,path=$SOCKET" \
    -device "vhost-user-blk-pci,chardev=zcblk,num-queues=$QUEUES,queue-size=$QUEUE_SIZE" \
    | tee -a "$QEMU_LOG"
qemu_status=${PIPESTATUS[0]}
pin_status=0
if [[ -n "$pin_pid" ]]; then
    wait "$pin_pid"
    pin_status=$?
    pin_pid=''
    cat "$QEMU_PIN_LOG"
fi
profile_status=0
if [[ -n "$profile_pid" ]]; then
    wait "$profile_pid"
    profile_status=$?
    profile_pid=''
fi
set -e

# Reap the backend directly.  `kill -0` also succeeds for an exited child that
# is waiting to be reaped, which made the earlier lifecycle check misclassify
# a clean disconnect as a live daemon.
wait "$backend_pid"
backend_pid=''

if [[ "$qemu_status" -ne 0 ]]; then
    printf 'QEMU exited with status %s\n' "$qemu_status" >&2
    exit "$qemu_status"
fi
if [[ "$pin_status" -ne 0 ]]; then
    printf 'QEMU vCPU pinning exited with status %s\n' "$pin_status" >&2
    exit "$pin_status"
fi
if [[ "$profile_status" -ne 0 ]]; then
    printf 'backend profiler exited with status %s\n' "$profile_status" >&2
    exit "$profile_status"
fi
grep -Fq '[zcvhost-vm] PASS:' "$QEMU_LOG"
if [[ "$GUEST_MODE" == fio ]]; then
    grep -Eq 'zcvhost-user-blk-summary:.*(reads|writes)=[1-9].*io_errors=0' "$BACKEND_LOG"
    grep -Eq 'IOPS=' "$QEMU_LOG"
else
    grep -Eq 'zcvhost-user-blk-summary:.*reads=[1-9].*writes=[1-9].*flushes=[1-9].*io_errors=0' "$BACKEND_LOG"
fi
if grep -Eq 'BUG:|Oops:|KASAN:|general protection fault|kernel panic' "$QEMU_LOG"; then
    printf 'guest kernel failure detected\n' >&2
    exit 1
fi

printf 'zcvhost-user-blk QEMU %s passed\n' "$GUEST_MODE"
printf 'guest_log=%s\nbackend_log=%s\nqemu_vcpu_pin_log=%s\n' \
    "$QEMU_LOG" "$BACKEND_LOG" "$QEMU_PIN_LOG"
