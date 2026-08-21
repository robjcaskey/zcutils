use super::{
    IORING_CQE_F_MORE, IORING_CQE_F_NOTIF, IORING_NOTIF_USAGE_ZC_COPIED, IORING_OP_SEND_ZC,
    RawRing, SendZcBatchAttempts, UringSendMode, ZCNBLK_FAN_WAL_COMPACT_WRITE_EXTENT_LEN,
    ZCNBLK_FAN_WAL_FLAG_DIRECT_MEMORY_WRITE_LAYOUT, ZCNBLK_FAN_WAL_FLAG_OFI_RMA_READ_RESULT,
    ZCNBLK_FAN_WAL_FLAG_OFI_RMA_READ_WINDOW, ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_PAYLOAD,
    ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_WINDOW, ZCNBLK_FAN_WAL_FLAG_RESULT_RANGE_BATCH,
    ZCNBLK_FAN_WAL_HEADER_LEN, ZCNBLK_FAN_WAL_OP_EOF, ZCNBLK_FAN_WAL_OP_HELLO,
    ZCNBLK_FAN_WAL_OP_HELLO_ACK, ZCNBLK_FAN_WAL_OP_READ_DESC, ZCNBLK_FAN_WAL_OP_REQUEST_BATCH,
    ZCNBLK_FAN_WAL_OP_RESULT, ZCNBLK_FAN_WAL_OP_RESULT_BATCH, ZCNBLK_FAN_WAL_OP_RESULT_RANGE_BATCH,
    ZCNBLK_FAN_WAL_OP_SYNC, ZCNBLK_FAN_WAL_OP_WRITE_BATCH, ZCNBLK_FAN_WAL_OP_WRITE_DESC,
    ZCNBLK_FAN_WAL_OP_WRITE_EXTENT_BATCH, ZCNBLK_FAN_WAL_STATUS_OK, ZCNBLK_OP_READ,
    ZCNBLK_OP_WRITE, ZcOfiMessageStream, ZcnblkFanWalAdaptiveRecvSpin,
    ZcnblkFanWalCompactWriteExtent, ZcnblkFanWalFrame, ZcnblkFanWalSharedLeaseSource,
    ZcnblkShmArenaDirtyHwmCache, advance_send_zc_iovecs, connect_tcp_bound_local_ip,
    default_hugepage_size, env_enabled_or, first_touch_pages, kernel_supports_request_opcode,
    memlock_rlimit_bytes, set_tcp_bench_buffers, socket_bench_buffer_bytes,
    validate_uring_send_mode_location, zc_topology_issue, zcnblk_fan_wal_decode_frame_slice,
    zcnblk_fan_wal_recv_exact_spin_then_block, zcnblk_fan_wal_write_frame,
    zcnblk_fan_wal_write_leaf_batch_payload, zcnblk_fan_wal_write_rma_payload_doorbell,
};
use crate::iops_policy::{LaneBudgetMailbox, LaneBudgetSnapshot, LaneLimiter};
use crate::wal_contract::{
    ZCNBLK_WAL_FEATURE_ALL, ZCNBLK_WAL_FEATURE_ATOMIC_WRITE, ZCNBLK_WAL_FEATURE_BATCH_SUBMISSION,
    ZCNBLK_WAL_FEATURE_FUA, ZCNBLK_WAL_FEATURE_IO_PRIORITY, ZCNBLK_WAL_FEATURE_POLLED_COMPLETION,
    ZCNBLK_WAL_FEATURE_REGISTERED_LEASE, ZCNBLK_WAL_FEATURE_WRITE_LIFETIME, ZcnblkWalIoContract,
};
use crate::zcnblk_app_arena::{
    ZCNBLK_APP_ARENA_F_EXTERNAL_HUGETLB, ZCNBLK_APP_ARENA_MAGIC, ZCNBLK_APP_ARENA_VERSION,
    ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED, ZcnblkAppArenaDescriptor, send_descriptor,
};
use std::cell::UnsafeCell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, VecDeque};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IoSlice, Read, Write};
use std::mem::{MaybeUninit, size_of};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{
    Receiver, Sender, SyncSender, TryRecvError, TrySendError, channel, sync_channel,
};
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, serde::Deserialize)]
struct WalLaneRateUpdate {
    generation: u64,
    sustained_iops: u64,
    peak_iops: u64,
    burst_seconds: u64,
    quantum_ops: u64,
    #[serde(default = "default_metric_publish_ns")]
    metric_publish_ns: u64,
    #[serde(default)]
    effective_after_ns: u64,
}

fn default_metric_publish_ns() -> u64 {
    100_000_000
}

struct WalLaneRateControl {
    mailboxes: Vec<Arc<LaneBudgetMailbox>>,
    epoch: Arc<Instant>,
    stop: Option<Arc<AtomicBool>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for WalLaneRateControl {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Release);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

const ZCNBLK_SHM_MAGIC: u64 = 0x3130_4d48_534e_435a;
const ZCNBLK_SHM_VERSION: u32 = 6;
const ZCNBLK_SHM_DESC_BYTES: u32 = 64;
const ZCNBLK_SHM_OP_WRITE: u16 = 1;
const ZCNBLK_SHM_OP_READ: u16 = 2;
const ZCNBLK_SHM_OP_SYNC: u16 = 7;
const ZCNBLK_SHM_F_APP_PAYLOAD_ALIAS: u16 = 1 << 2;
const ZCNBLK_SHM_CAP_SECTOR_PREDECESSOR: u64 = 1 << 0;
const ZCNBLK_SHM_CAP_TRANSFER_PAYLOAD_SLOTS: u64 = 1 << 1;
const ZCNBLK_SHM_CAP_READ_PAYLOAD_REF: u64 = 1 << 2;
const ZCNBLK_SHM_CAP_REQUEST_WAKE_ARMED: u64 = 1 << 3;
const ZCNBLK_SHM_CAP_COMPLETION_WAKE_ARMED: u64 = 1 << 4;
const ZCNBLK_SHM_CAP_ORDERING_EPOCH: u64 = 1 << 5;
const ZCNBLK_SHM_CAP_ORDERING_VECTOR: u64 = 1 << 6;
const ZCNBLK_SHM_CAP_IO_CONTRACT_SIDECAR: u64 = 1 << 7;
const ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_IMPORT: u64 = 1 << 8;
const ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_ACTIVE: u64 = 1 << 9;
const ZCNBLK_SHM_CAP_BIO_ARENA_ALIAS: u64 = 1 << 10;
const ZCNBLK_SHM_CAP_LANE_LOCAL_SEQUENCE: u64 = 1 << 12;
const ZCNBLK_SHM_IO_FEATURE_ALL: u64 = ZCNBLK_WAL_FEATURE_FUA as u64
    | ZCNBLK_WAL_FEATURE_POLLED_COMPLETION as u64
    | ZCNBLK_WAL_FEATURE_BATCH_SUBMISSION as u64
    | ZCNBLK_WAL_FEATURE_IO_PRIORITY as u64
    | ZCNBLK_WAL_FEATURE_REGISTERED_LEASE as u64
    | ZCNBLK_WAL_FEATURE_ATOMIC_WRITE as u64
    | ZCNBLK_WAL_FEATURE_WRITE_LIFETIME as u64;
const ZCNBLK_SHM_IO_F_FUA: u32 = 1 << 0;
const ZCNBLK_SHM_IO_F_POLLED_COMPLETION: u32 = 1 << 1;
const ZCNBLK_SHM_IO_F_REGISTERED_LEASE: u32 = 1 << 2;
const ZCNBLK_SHM_IO_F_ATOMIC_WRITE: u32 = 1 << 3;
const ZCNBLK_SHM_IO_F_ALL: u32 = (1 << 4) - 1;
const ZCNBLK_SHM_REQUEST_ID_BITS: u32 = 16;
#[cfg(test)]
const ZCNBLK_SHM_REQUEST_ID_MASK: u64 = (1 << ZCNBLK_SHM_REQUEST_ID_BITS) - 1;
const ZCNBLK_SHM_CQE_F_READ_PAYLOAD_REF: u32 = 1 << 0;
const ZCNBLK_SHM_CQE_REF_CHANNEL_SHIFT: u32 = 8;
const ZCNBLK_SHM_ATTACH_F_TRANSFER_PAYLOAD_SLOTS: u32 = 1 << 0;
const ZCNBLK_SHM_ATTACH_F_LANE_LOCAL_SEQUENCE: u32 = 1 << 1;
const ZCNBLK_SHM_ARENA_IMPORT_F_HUGETLB: u32 = 1 << 0;
const ZCNBLK_SHM_HEADER_CAPABILITIES: usize = 0;
const ZCNBLK_SHM_HEADER_PAYLOAD_OWNER_OFFSET: usize = 1;
const ZCNBLK_SHM_HEADER_IO_CONTRACT_OFFSET: usize = 2;
const ZCNBLK_SHM_HEADER_IO_FEATURES: usize = 3;
const ZCNBLK_SHM_CHANNEL_FLUSH_TAIL: usize = 0;
const ZCNBLK_SHM_CHANNEL_FLUSH_EPOCH: usize = 1;
const ZCNBLK_SHM_IOC_MAGIC: u32 = 0xbc;

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = 8;
const IOC_SIZESHIFT: u32 = 16;
const IOC_DIRSHIFT: u32 = 30;
const ZCNBLK_MFD_CLOEXEC: libc::c_uint = 0x0001;
const ZCNBLK_MFD_ALLOW_SEALING: libc::c_uint = 0x0002;
const ZCNBLK_MFD_HUGETLB: libc::c_uint = 0x0004;

fn wal_lane_rate_control_from_env(lane_count: u32) -> io::Result<Option<WalLaneRateControl>> {
    let Some(sustained) = env::var("URING_PLAY_ZCNBLK_SHM_HTB_SUSTAINED_IOPS")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
    else {
        return Ok(None);
    };
    if lane_count == 0 || sustained == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HTB sustained IOPS and lane count must be nonzero",
        ));
    }
    let peak = env::var("URING_PLAY_ZCNBLK_SHM_HTB_PEAK_IOPS")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(sustained);
    let quantum = env::var("URING_PLAY_ZCNBLK_SHM_HTB_QUANTUM_OPS")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(256)
        .max(1);
    let burst_seconds = env::var("URING_PLAY_ZCNBLK_SHM_HTB_BURST_SECONDS")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(1);
    if peak < sustained {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HTB peak IOPS must be at least sustained IOPS",
        ));
    }
    let lanes = u64::from(lane_count);
    let epoch = Arc::new(Instant::now());
    let mailboxes = (0..lane_count)
        .map(|lane| {
            let lane = u64::from(lane);
            let lane_sustained = sustained / lanes + u64::from(lane < sustained % lanes);
            let lane_peak = peak / lanes + u64::from(lane < peak % lanes);
            Arc::new(LaneBudgetMailbox::new(LaneBudgetSnapshot {
                generation: 1,
                sustained_iops: lane_sustained,
                peak_iops: lane_peak,
                burst_ops: quantum.saturating_add(
                    lane_peak
                        .saturating_sub(lane_sustained)
                        .saturating_mul(burst_seconds),
                ),
                quantum_ops: quantum,
                metric_publish_ns: 100_000_000,
                effective_ns: 0,
                fallback_sustained_iops: lane_sustained,
                fallback_peak_iops: lane_peak,
                valid_until_ns: 0,
            }))
        })
        .collect::<Vec<_>>();
    eprintln!(
        "zcnblk-shm-target-htb: mode=lane-local-batch sustained_iops={sustained} peak_iops={peak} lanes={lane_count} quantum_ops={quantum} burst_seconds={burst_seconds} mailbox_poll=grant-boundary controller_path=off-hot-path"
    );
    let (stop, worker) = if let Some(path) =
        env::var_os("URING_PLAY_ZCNBLK_SHM_HTB_CONTROL_FILE").filter(|path| !path.is_empty())
    {
        let path = PathBuf::from(path);
        let worker_mailboxes = mailboxes.clone();
        let worker_epoch = Arc::clone(&epoch);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("zc-htb-control".into())
            .spawn(move || {
                let mut applied_generation = 1u64;
                while !worker_stop.load(Ordering::Acquire) {
                    if let Ok(bytes) = fs::read(&path) {
                        match serde_json::from_slice::<WalLaneRateUpdate>(&bytes) {
                            Ok(update)
                                if update.generation > applied_generation
                                    && update.sustained_iops > 0
                                    && update.peak_iops >= update.sustained_iops
                                    && update.quantum_ops > 0 =>
                            {
                                let effective_ns = (worker_epoch
                                    .elapsed()
                                    .as_nanos()
                                    .min(u128::from(u64::MAX))
                                    as u64)
                                    .saturating_add(update.effective_after_ns);
                                for (lane, mailbox) in worker_mailboxes.iter().enumerate() {
                                    let lane_sustained = split_lane_rate(
                                        update.sustained_iops,
                                        worker_mailboxes.len(),
                                        lane,
                                    );
                                    let lane_peak = split_lane_rate(
                                        update.peak_iops,
                                        worker_mailboxes.len(),
                                        lane,
                                    );
                                    mailbox.publish(LaneBudgetSnapshot {
                                        generation: update.generation,
                                        sustained_iops: lane_sustained,
                                        peak_iops: lane_peak,
                                        burst_ops: update.quantum_ops.saturating_add(
                                            lane_peak
                                                .saturating_sub(lane_sustained)
                                                .saturating_mul(update.burst_seconds),
                                        ),
                                        quantum_ops: update.quantum_ops.max(1),
                                        metric_publish_ns: update.metric_publish_ns.max(1),
                                        effective_ns,
                                        fallback_sustained_iops: lane_sustained,
                                        fallback_peak_iops: lane_peak,
                                        valid_until_ns: 0,
                                    });
                                }
                                applied_generation = update.generation;
                                eprintln!(
                                    "zcnblk-shm-target-htb-update: generation={} sustained_iops={} peak_iops={} effective_ns={effective_ns}",
                                    update.generation, update.sustained_iops, update.peak_iops
                                );
                            }
                            Ok(update) if update.generation > applied_generation => eprintln!(
                                "zcnblk-shm-target-htb-control-warning: file={} generation={} invalid-rate-update",
                                path.display(), update.generation
                            ),
                            Ok(_) => {}
                            Err(error) => eprintln!(
                                "zcnblk-shm-target-htb-control-warning: file={} error={error}",
                                path.display()
                            ),
                        }
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            })?;
        (Some(stop), Some(worker))
    } else {
        (None, None)
    };
    Ok(Some(WalLaneRateControl {
        mailboxes,
        epoch,
        stop,
        worker,
    }))
}

fn split_lane_rate(total: u64, lanes: usize, lane: usize) -> u64 {
    total / lanes as u64 + u64::from((lane as u64) < total % lanes as u64)
}

const fn ioctl_code(dir: u32, nr: u32, size: usize) -> libc::c_ulong {
    ((dir << IOC_DIRSHIFT)
        | (ZCNBLK_SHM_IOC_MAGIC << IOC_TYPESHIFT)
        | (nr << IOC_NRSHIFT)
        | ((size as u32) << IOC_SIZESHIFT)) as libc::c_ulong
}

const ZCNBLK_SHM_IOC_ATTACH: libc::c_ulong = ioctl_code(IOC_WRITE, 1, size_of::<ZcnblkShmAttach>());
const ZCNBLK_SHM_IOC_KICK: libc::c_ulong = ioctl_code(IOC_WRITE, 2, size_of::<u32>());
const ZCNBLK_SHM_IOC_GET_INFO: libc::c_ulong =
    ioctl_code(IOC_READ, 3, size_of::<ZcnblkShmHeader>());
const ZCNBLK_SHM_IOC_IMPORT_ARENA: libc::c_ulong =
    ioctl_code(IOC_WRITE, 4, size_of::<ZcnblkShmArenaImport>());

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ZcnblkShmChannel {
    req_prod: u64,
    request_publishes: u64,
    request_kicks: u64,
    request_producer_reserved: [u64; 5],
    req_cons: u64,
    request_wake_armed: u64,
    request_consumer_reserved: [u64; 6],
    comp_prod: u64,
    payload_lease_hwm: u64,
    completion_producer_reserved: [u64; 6],
    comp_cons: u64,
    completion_kicks: u64,
    completion_wake_armed: u64,
    completion_consumer_reserved: [u64; 5],
    payload_free_slots: u64,
    payload_reserved: [u64; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ZcnblkShmRequest {
    sequence: u64,
    request_id: u64,
    offset: u64,
    len: u32,
    op: u16,
    flags: u16,
    lane: u32,
    stream: u32,
    queue_id: u32,
    payload_slot: u32,
    submit_sequence: u64,
    sector_predecessor: u64,
}

impl ZcnblkShmRequest {
    fn ordering_epoch(self) -> u64 {
        self.request_id >> ZCNBLK_SHM_REQUEST_ID_BITS
    }

    #[cfg(test)]
    fn client_request_id(self) -> u64 {
        self.request_id & ZCNBLK_SHM_REQUEST_ID_MASK
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ZcnblkShmCompletion {
    sequence: u64,
    request_id: u64,
    offset: u64,
    committed_hwm: u64,
    len: u32,
    lane: u32,
    stream: u32,
    payload_slot: u32,
    op: u16,
    status: i16,
    flags: u32,
    request_sequence: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ZcnblkShmIoContract {
    flags: u32,
    ioprio: u16,
    write_lifetime: u8,
    reserved: u8,
    lease_id: u64,
}

impl ZcnblkShmIoContract {
    fn into_wal(self) -> io::Result<ZcnblkWalIoContract> {
        let unknown = self.flags & !ZCNBLK_SHM_IO_F_ALL;
        if unknown != 0 || self.reserved != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "zcnblk SHM I/O contract contains unknown flags={unknown:#x} reserved={}",
                    self.reserved
                ),
            ));
        }
        let contract = ZcnblkWalIoContract {
            fua: self.flags & ZCNBLK_SHM_IO_F_FUA != 0,
            polled_completion: self.flags & ZCNBLK_SHM_IO_F_POLLED_COMPLETION != 0,
            registered_lease: self.flags & ZCNBLK_SHM_IO_F_REGISTERED_LEASE != 0,
            atomic_write: self.flags & ZCNBLK_SHM_IO_F_ATOMIC_WRITE != 0,
            ioprio: self.ioprio,
            write_lifetime: self.write_lifetime,
            lease_id: self.lease_id,
        };
        contract.encode()?;
        Ok(contract)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ZcnblkShmHeader {
    magic: u64,
    version: u32,
    header_bytes: u32,
    channels: u32,
    ring_entries: u32,
    payload_entries: u32,
    slot_bytes: u32,
    descriptor_bytes: u32,
    channel_offset: u64,
    request_offset: u64,
    completion_offset: u64,
    payload_offset: u64,
    region_bytes: u64,
    capacity_bytes: u64,
    daemon_generation: u64,
    daemon_online: u64,
    global_submit_sequence: u64,
    reserved: [u64; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ZcnblkShmAttach {
    magic: u64,
    version: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ZcnblkShmArenaImport {
    magic: u64,
    version: u32,
    flags: u32,
    fd: i32,
    reserved: u32,
    region_bytes: u64,
}

const _: () = assert!(size_of::<ZcnblkShmChannel>() == 320);
const _: () = assert!(size_of::<ZcnblkShmRequest>() == 64);
const _: () = assert!(size_of::<ZcnblkShmCompletion>() == 64);
const _: () = assert!(size_of::<ZcnblkShmIoContract>() == 16);
const _: () = assert!(size_of::<ZcnblkShmHeader>() == 144);
const _: () = assert!(size_of::<ZcnblkShmArenaImport>() == 32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SharedArenaBacking {
    KernelVmalloc,
    ExternalHugeTlb,
}

impl SharedArenaBacking {
    fn label(self) -> &'static str {
        match self {
            Self::KernelVmalloc => "kernel-vmalloc-user",
            Self::ExternalHugeTlb => "external-hugetlb-memfd",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SharedArenaRequest {
    Vmalloc,
    HugeTlb,
    Auto,
}

impl SharedArenaRequest {
    fn from_env() -> io::Result<Self> {
        match env::var("URING_PLAY_ZCNBLK_SHM_ARENA_BACKING")
            .unwrap_or_else(|_| "vmalloc".to_string())
            .as_str()
        {
            "vmalloc" | "kernel-vmalloc" | "small-pages" => Ok(Self::Vmalloc),
            "hugetlb" | "huge" | "external-hugetlb" => Ok(Self::HugeTlb),
            "auto" => Ok(Self::Auto),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown shared arena backing {other:?}; use vmalloc, hugetlb, or auto"),
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Vmalloc => "vmalloc",
            Self::HugeTlb => "hugetlb",
            Self::Auto => "auto",
        }
    }
}

struct Mapping {
    ptr: *mut u8,
    len: usize,
    backing: SharedArenaBacking,
    hugepage_bytes: usize,
    export_fd: Option<OwnedFd>,
}

impl Mapping {
    fn map_control(
        file: &File,
        len: usize,
        backing: SharedArenaBacking,
        hugepage_bytes: usize,
    ) -> io::Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: ptr.cast(),
            len,
            backing,
            hugepage_bytes,
            export_fd: None,
        })
    }

    fn slice(&self, start: usize, len: usize) -> io::Result<&[u8]> {
        let end = start.checked_add(len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "shared mapping range overflow")
        })?;
        if end > self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "shared mapping range exceeds mmap",
            ));
        }
        Ok(unsafe { std::slice::from_raw_parts(self.ptr.add(start).cast_const(), len) })
    }
}

unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

struct RamBacking {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for RamBacking {}
unsafe impl Sync for RamBacking {}

impl RamBacking {
    fn new(len: usize) -> io::Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            libc::madvise(ptr, len, libc::MADV_HUGEPAGE);
        }
        Ok(Self {
            ptr: ptr.cast(),
            len,
        })
    }
}

impl Drop for RamBacking {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendMode {
    Null,
    Memory,
    WalMemory,
    WalTcp,
}

impl BackendMode {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "null" => Ok(Self::Null),
            "memory" | "ram" => Ok(Self::Memory),
            "wal-memory" | "walmem" | "lease-memory" => Ok(Self::WalMemory),
            "wal-tcp" | "tcp-leaf" | "fan-tcp" => Ok(Self::WalTcp),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown backend {value:?}; use null, memory, wal-memory, or wal-tcp"),
            )),
        }
    }

    fn is_wal_writeback(self) -> bool {
        matches!(self, Self::WalMemory | Self::WalTcp)
    }

    fn can_ack_block_sync(self) -> bool {
        matches!(self, Self::WalTcp)
    }

    fn sync_contract(self) -> &'static str {
        if self.can_ack_block_sync() {
            "remote-leaf-hwm"
        } else {
            "unsupported-volatile"
        }
    }
}

impl ZcnblkFanWalSharedLeaseSource for Mapping {
    fn payload_slice(&self, start: usize, len: usize) -> io::Result<&[u8]> {
        self.slice(start, len)
    }
}

#[derive(Clone, Copy)]
struct PendingWalWrite {
    request: ZcnblkShmRequest,
    io_contract: ZcnblkWalIoContract,
    request_sequence: u64,
    submit_sequence: u64,
    offset: u64,
    len: usize,
    payload_offset: usize,
}

#[derive(Clone, Copy)]
struct PendingRemoteRead {
    request: ZcnblkShmRequest,
    io_contract: ZcnblkWalIoContract,
    request_sequence: u64,
    payload_offset: usize,
    dirty_ref: Option<WalDirtyReadRef>,
}

enum RemoteWalStream {
    Tcp(TcpStream),
    Ofi(ZcOfiMessageStream),
}

// SharedTarget is borrowed by its scoped lane workers only after every
// RemoteWalLeaf has been moved out into its owning lane.  Its remaining shared
// stream methods inspect metadata or a TCP fd and never call the OFI endpoint;
// all OFI I/O still requires exclusive `&mut RemoteWalStream` access.
unsafe impl Sync for RemoteWalStream {}

impl RemoteWalStream {
    fn transport_label(&self) -> &'static str {
        match self {
            Self::Tcp(_) => "tcp",
            Self::Ofi(_) => "ofi",
        }
    }
}

impl RemoteWalStream {
    fn tcp(&self) -> Option<&TcpStream> {
        match self {
            Self::Tcp(stream) => Some(stream),
            Self::Ofi(_) => None,
        }
    }

    fn raw_fd(&self) -> Option<i32> {
        self.tcp().map(AsRawFd::as_raw_fd)
    }

    fn recv_exact(&mut self, wait: &mut RemoteWalRecvWait, out: &mut [u8]) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => wait.recv_exact(stream, out),
            Self::Ofi(stream) => stream.read_exact(out),
        }
    }

    fn set_quickack(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => set_tcp_quickack(stream),
            Self::Ofi(_) => Ok(()),
        }
    }

    fn locality(&self) -> (i32, i32) {
        self.tcp().map_or((-1, -1), socket_rx_locality)
    }

    fn register_rma_read_buffer(&mut self, target: &mut [u8]) -> io::Result<()> {
        match self {
            Self::Ofi(stream) => stream.register_rma_read_buffer(target),
            Self::Tcp(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "OFI RMA read-buffer registration requested for TCP",
            )),
        }
    }

    unsafe fn register_rma_read_buffer_raw(
        &mut self,
        target: *mut u8,
        len: usize,
    ) -> io::Result<()> {
        match self {
            Self::Ofi(stream) => unsafe { stream.register_rma_read_buffer_raw(target, len) },
            Self::Tcp(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "OFI RMA read-buffer registration requested for TCP",
            )),
        }
    }

    fn configure_rma_read_queue(&mut self, depth: usize) -> io::Result<()> {
        match self {
            Self::Ofi(stream) => stream.configure_rma_read_queue(depth),
            Self::Tcp(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "OFI RMA read queue requested for TCP",
            )),
        }
    }

    unsafe fn register_rma_write_buffer_raw(
        &mut self,
        source: *const u8,
        len: usize,
    ) -> io::Result<()> {
        match self {
            Self::Ofi(stream) => unsafe { stream.register_rma_write_buffer_raw(source, len) },
            Self::Tcp(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "OFI RMA write-buffer registration requested for TCP",
            )),
        }
    }

    fn configure_rma_write_queue(&mut self, depth: usize) -> io::Result<()> {
        match self {
            Self::Ofi(stream) => stream.configure_rma_write_queue(depth),
            Self::Tcp(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "OFI RMA write queue requested for TCP",
            )),
        }
    }

    /// `source` remains owned by its shared payload lease until this slot is
    /// returned by `poll_rma_writes`.
    unsafe fn post_rma_write_raw(
        &mut self,
        source: *const u8,
        len: usize,
        remote_addr: u64,
        remote_key: u64,
        slot: usize,
        user_data: u64,
        more: bool,
    ) -> io::Result<bool> {
        match self {
            Self::Ofi(stream) => unsafe {
                stream.post_rma_write_more_raw(
                    source,
                    len,
                    remote_addr,
                    remote_key,
                    slot,
                    user_data,
                    more,
                )
            },
            Self::Tcp(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "OFI RMA write post requested for TCP",
            )),
        }
    }

    fn poll_rma_writes(
        &mut self,
        out_slots: &mut [usize],
        out_user_data: &mut [u64],
        wait: bool,
    ) -> io::Result<usize> {
        match self {
            Self::Ofi(stream) => stream.poll_rma_writes(out_slots, out_user_data, wait),
            Self::Tcp(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "OFI RMA write poll requested for TCP",
            )),
        }
    }

    /// `target` remains owned by `slot` until `poll_rma_reads` returns it.
    unsafe fn post_rma_read_raw(
        &mut self,
        target: *mut u8,
        len: usize,
        remote_addr: u64,
        remote_key: u64,
        slot: usize,
        user_data: u64,
        force_completion: bool,
    ) -> io::Result<bool> {
        match self {
            Self::Ofi(stream) => unsafe {
                stream.post_rma_read_raw(
                    target,
                    len,
                    remote_addr,
                    remote_key,
                    slot,
                    user_data,
                    force_completion,
                )
            },
            Self::Tcp(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "OFI RMA read post requested for TCP",
            )),
        }
    }

    fn poll_rma_reads(
        &mut self,
        out_slots: &mut [usize],
        out_user_data: &mut [u64],
        wait: bool,
    ) -> io::Result<usize> {
        match self {
            Self::Ofi(stream) => stream.poll_rma_reads(out_slots, out_user_data, wait),
            Self::Tcp(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "OFI RMA read poll requested for TCP",
            )),
        }
    }

    fn rma_read(&mut self, target: &mut [u8], remote_addr: u64, remote_key: u64) -> io::Result<()> {
        match self {
            Self::Ofi(stream) => stream.rma_read(target, remote_addr, remote_key),
            Self::Tcp(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "OFI RMA read requested for TCP",
            )),
        }
    }
}

impl Read for RemoteWalStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(out),
            Self::Ofi(stream) => stream.read(out),
        }
    }
}

impl Write for RemoteWalStream {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(input),
            Self::Ofi(stream) => stream.write(input),
        }
    }

    fn write_vectored(&mut self, inputs: &[IoSlice<'_>]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write_vectored(inputs),
            Self::Ofi(stream) => stream.write_vectored(inputs),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            Self::Ofi(stream) => stream.flush(),
        }
    }
}

struct RemoteWalLeaf {
    stream: RemoteWalStream,
    mapping: Option<Arc<Mapping>>,
    address: String,
    lane_id: u32,
    lane_count: u32,
    negotiated_features: u32,
    rma_read_window: Option<RemoteWalRmaReadWindow>,
    rma_read_queue: Option<RemoteWalRmaReadQueue>,
    rma_write_window: Option<RemoteWalRmaReadWindow>,
    rma_write_queue: Option<RemoteWalRmaWriteQueue>,
    rma_write_required: bool,
    request_batches_pending: usize,
    rma_write_doorbell_pending: bool,
    target_cpu: Option<usize>,
    write_batches: u64,
    write_records: u64,
    write_bytes: u64,
    write_payload_iovecs: u64,
    write_payload_tx_iovecs: u64,
    write_payload_runs: u64,
    max_write_payload_run_bytes: u64,
    compact_writes: bool,
    compact_write_batches: u64,
    request_descriptor_bytes: u64,
    wire_descriptor_bytes: u64,
    read_records: u64,
    read_bytes: u64,
    read_batches: u64,
    syncs: u64,
    sync_time: Duration,
    recv_wait: RemoteWalRecvWait,
    send_mode: RemoteWalSendMode,
    require_send_zc: bool,
    control_writev_batches: u64,
    send_zc_notifications: u64,
    send_zc_copied_notifications: u64,
    tcp_nodelay: bool,
    quickack: bool,
    request_send_calls: u64,
    request_send_time: Duration,
    result_recv_calls: u64,
    result_recv_time: Duration,
    result_header_time: Duration,
    result_descriptor_time: Duration,
    result_payload_time: Duration,
    rma_read_calls: u64,
    rma_read_time: Duration,
    rma_read_copy_time: Duration,
    rma_write_batches: u64,
    rma_write_calls: u64,
    rma_write_bytes: u64,
    rma_write_time: Duration,
}

#[derive(Clone, Copy)]
struct RemoteWalRmaReadWindow {
    addr: u64,
    key: u64,
    len: u64,
}

#[derive(Clone, Copy)]
struct RemoteWalRmaQueuedRead {
    batch_id: u64,
    token: u64,
    remote_offset: u64,
    payload_offset: usize,
    len: usize,
}

#[derive(Clone, Copy, Default)]
struct RemoteWalRmaActiveRead {
    batch_id: u64,
    token: u64,
}

struct RemoteWalRmaBatch {
    remaining: usize,
    records: usize,
    bytes: u64,
    complete: bool,
}

struct RemoteWalRmaBatchEntry {
    id: u64,
    batch: RemoteWalRmaBatch,
}

struct RemoteWalRmaReadQueue {
    slot_bytes: usize,
    depth: usize,
    free_slots: Vec<usize>,
    pending: VecDeque<RemoteWalRmaQueuedRead>,
    active: Vec<RemoteWalRmaActiveRead>,
    batches: VecDeque<RemoteWalRmaBatchEntry>,
    completion_slots: Vec<usize>,
    completion_tokens: Vec<u64>,
    latency_started: Option<Vec<Option<Instant>>>,
    next_batch_id: u64,
    next_token: u64,
    in_flight: usize,
    peak_in_flight: usize,
    cq_polls: u64,
    cq_batches: u64,
    cq_completions: u64,
    post_eagain: u64,
    defer_tail_completion: bool,
}

#[inline]
fn rma_read_post_policy(
    defer_tail_completion: bool,
    close_group: bool,
    pending: usize,
    free_slots: usize,
    in_flight: usize,
) -> (bool, bool) {
    if !defer_tail_completion || pending == 0 || free_slots == 0 {
        return (false, false);
    }
    let defer = !close_group && pending == 1 && free_slots > 1 && in_flight != 0;
    let force_completion = !defer
        && (free_slots == 1 || (close_group && pending == 1) || (in_flight == 0 && pending == 1));
    (defer, force_completion)
}

fn advance_lane_completion_counts(
    lane_completed: &mut [u64],
    per_channel: &[usize],
) -> io::Result<()> {
    if lane_completed.len() != per_channel.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "lane completion count shape mismatch",
        ));
    }
    for (completed, count) in lane_completed.iter_mut().zip(per_channel.iter().copied()) {
        *completed = completed
            .checked_add(u64::try_from(count).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "lane completion count overflow")
            })?)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "lane completion HWM overflow")
            })?;
    }
    Ok(())
}

fn lane_token_is_complete(lane_completed: &[u64], token: u64) -> bool {
    if token == 0 {
        return true;
    }
    let channels = lane_completed.len() as u64;
    if channels == 0 {
        return false;
    }
    let zero_based = token - 1;
    let channel = (zero_based % channels) as usize;
    let request_sequence = zero_based / channels;
    lane_completed[channel] > request_sequence
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RemoteWalRmaWriteRun {
    source_offset: usize,
    remote_offset: u64,
    len: usize,
}

struct RemoteWalRmaActiveWrite {
    token: u64,
    remote_offset: u64,
    remote_end: u64,
}

struct RemoteWalRmaWriteQueue {
    depth: usize,
    free_slots: Vec<usize>,
    active: Vec<Option<RemoteWalRmaActiveWrite>>,
    completion_slots: Vec<usize>,
    completion_tokens: Vec<u64>,
    next_token: u64,
    peak_in_flight: usize,
    cq_polls: u64,
    cq_batches: u64,
    cq_completions: u64,
    post_eagain: u64,
    completed_batches: u64,
    completed_operations: u64,
    multi_run_batches: u64,
    concurrently_posted_batches: u64,
    batches_exceeding_depth: u64,
    post_rounds: u64,
    max_batch_operations: usize,
    depth_barriers: u64,
    overlap_barriers: u64,
    write_more: bool,
    more_posts: u64,
    flush_posts: u64,
    more_followup_eagain: u64,
}

fn remote_wal_rma_write_runs_overlap(
    left: RemoteWalRmaWriteRun,
    right: RemoteWalRmaWriteRun,
) -> bool {
    let left_end = left.remote_offset + left.len as u64;
    let right_end = right.remote_offset + right.len as u64;
    left.remote_offset < right_end && right.remote_offset < left_end
}

fn remote_wal_rma_write_overlaps_active(
    active: &[Option<RemoteWalRmaActiveWrite>],
    run: RemoteWalRmaWriteRun,
) -> bool {
    let remote_end = run.remote_offset + run.len as u64;
    active
        .iter()
        .flatten()
        .any(|active| run.remote_offset < active.remote_end && active.remote_offset < remote_end)
}

fn validate_remote_wal_rma_write_runs(
    mapping_len: usize,
    window: RemoteWalRmaReadWindow,
    runs: &[RemoteWalRmaWriteRun],
) -> io::Result<()> {
    for run in runs {
        if run.len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "OFI RMA write run must not be empty",
            ));
        }
        let source_end = run.source_offset.checked_add(run.len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "OFI RMA source range overflow")
        })?;
        if source_end > mapping_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "OFI RMA source range exceeds the registered shared mapping",
            ));
        }
        let remote_len = u64::try_from(run.len).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "OFI RMA write length overflow")
        })?;
        let remote_end = run.remote_offset.checked_add(remote_len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "OFI RMA write range overflow")
        })?;
        if remote_end > window.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "OFI RMA write range offset={} len={} exceeds negotiated window bytes={}",
                    run.remote_offset, run.len, window.len
                ),
            ));
        }
        window.addr.checked_add(run.remote_offset).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "OFI RMA write address overflow")
        })?;
    }
    Ok(())
}

#[derive(Default)]
struct RemoteWalRmaProgress {
    completions: usize,
    completion_time: Duration,
    copy_time: Duration,
}

impl RemoteWalRmaReadQueue {
    fn new(slot_bytes: usize, depth: usize) -> io::Result<Self> {
        if slot_bytes == 0 || depth == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid OFI RMA direct-placement queue shape",
            ));
        }
        let latency_started = env_enabled_or("URING_PLAY_OFI_RMA_LATENCY_TELEMETRY", false)
            .then(|| (0..depth).map(|_| None).collect());
        Ok(Self {
            slot_bytes,
            depth,
            free_slots: (0..depth).rev().collect(),
            pending: VecDeque::new(),
            active: vec![RemoteWalRmaActiveRead::default(); depth],
            batches: VecDeque::with_capacity(depth),
            completion_slots: vec![0; depth],
            completion_tokens: vec![0; depth],
            latency_started,
            next_batch_id: 1,
            next_token: 1,
            in_flight: 0,
            peak_in_flight: 0,
            cq_polls: 0,
            cq_batches: 0,
            cq_completions: 0,
            post_eagain: 0,
            defer_tail_completion: env_enabled_or("URING_PLAY_OFI_SELECTIVE_COMPLETION", false)
                && env::var("URING_PLAY_OFI_RMA_READ_COMPLETION_STRIDE")
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(1)
                    > 1
                && env_enabled_or("URING_PLAY_OFI_RMA_DEFER_TAIL_COMPLETION", true),
        })
    }

    fn submit_batch(
        &mut self,
        window: RemoteWalRmaReadWindow,
        mapping_len: usize,
        requests: &[PendingRemoteRead],
    ) -> io::Result<u64> {
        if requests.is_empty()
            || requests
                .iter()
                .any(|request| request.request.op != ZCNBLK_SHM_OP_READ)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "OFI RMA batch must contain at least one read and no other operations",
            ));
        }
        let mut bytes = 0u64;
        for request in requests {
            let len = usize::try_from(request.request.len).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "OFI RMA read length overflow")
            })?;
            if len == 0 || len > self.slot_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "OFI RMA read bytes={len} exceed fixed slot bytes={}",
                        self.slot_bytes
                    ),
                ));
            }
            let payload_end = request.payload_offset.checked_add(len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "OFI RMA payload range overflow")
            })?;
            if payload_end > mapping_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "OFI RMA payload range offset={} len={len} exceeds shared mapping bytes={mapping_len}",
                        request.payload_offset,
                    ),
                ));
            }
            let remote_end = request
                .request
                .offset
                .checked_add(request.request.len.into())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "OFI RMA remote read range overflow",
                    )
                })?;
            if remote_end > window.len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "OFI RMA read range offset={} len={} exceeds negotiated window bytes={}",
                        request.request.offset, request.request.len, window.len
                    ),
                ));
            }
            window
                .addr
                .checked_add(request.request.offset)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "OFI RMA remote address overflow",
                    )
                })?;
            bytes = bytes.saturating_add(request.request.len.into());
        }
        let batch_id = self.next_batch_id;
        self.next_batch_id = self.next_batch_id.wrapping_add(1).max(1);
        if let Some(last) = self.batches.back() {
            let expected = last.id.checked_add(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    "OFI RMA batch identifier space exhausted with live batches",
                )
            })?;
            if batch_id != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "OFI RMA batch identifier sequence is not contiguous",
                ));
            }
        }
        self.batches.push_back(RemoteWalRmaBatchEntry {
            id: batch_id,
            batch: RemoteWalRmaBatch {
                remaining: requests.len(),
                records: requests.len(),
                bytes,
                complete: false,
            },
        });
        for &request in requests {
            let token = self.next_token;
            self.next_token = self.next_token.wrapping_add(1).max(1);
            self.pending.push_back(RemoteWalRmaQueuedRead {
                batch_id,
                token,
                remote_offset: request.request.offset,
                payload_offset: request.payload_offset,
                len: request.request.len as usize,
            });
        }
        Ok(batch_id)
    }

    fn post_available(
        &mut self,
        stream: &mut RemoteWalStream,
        mapping: &Mapping,
        window: RemoteWalRmaReadWindow,
        close_group: bool,
    ) -> io::Result<()> {
        if self.latency_started.is_some() {
            self.post_available_mode::<true>(stream, mapping, window, close_group)
        } else {
            self.post_available_mode::<false>(stream, mapping, window, close_group)
        }
    }

    fn post_available_mode<const TRACK_LATENCY: bool>(
        &mut self,
        stream: &mut RemoteWalStream,
        mapping: &Mapping,
        window: RemoteWalRmaReadWindow,
        close_group: bool,
    ) -> io::Result<()> {
        while !self.pending.is_empty() && !self.free_slots.is_empty() {
            /* Keep one real read available to carry the fence CQE when the
             * consumer reaches its blocking drain boundary.  Previously that
             * boundary required a synthetic one-byte RMA read.  Do not hold a
             * lone operation in an otherwise idle queue, and close a full
             * posting window immediately so it can make progress. */
            let pending_len = self.pending.len();
            let free_len = self.free_slots.len();
            // Interior posts cannot be the deferred real tail and cannot
            // close a full posting window. Keep the full liveness policy off
            // the dominant QD-2 prefix; only either tail pays those branches.
            let (defer, force_completion) = if pending_len > 1 && free_len > 1 {
                (false, false)
            } else {
                rma_read_post_policy(
                    self.defer_tail_completion,
                    close_group,
                    pending_len,
                    free_len,
                    self.in_flight,
                )
            };
            if defer {
                break;
            }
            let (slot, queued) = self
                .take_postable()
                .expect("non-empty RMA queue lost a postable read");
            if self.active[slot].token != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("OFI RMA free-list returned active slot={slot}"),
                ));
            }
            let len = queued.len;
            let remote_addr = window
                .addr
                .checked_add(queued.remote_offset)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "OFI RMA remote address overflow",
                    )
                })?;
            // The block-edge request owns this shared payload slot until its
            // completion is published. The whole mapping is registered once,
            // and the operation slot is retained until its CQE is retired.
            // Batch admission proved payload_offset + len is inside this
            // stable mapping. Mapping ownership outlives every queued read,
            // so posting needs only the address calculation on the hot path.
            let target = unsafe { mapping.ptr.add(queued.payload_offset) };
            // Per-operation clock reads are surprisingly expensive at multi-million
            // IOPS. Keep latency telemetry available for diagnostic runs, but do not
            // put two clock_gettime calls on every production RMA read by default.
            let posted_at = if TRACK_LATENCY {
                Some(Instant::now())
            } else {
                None
            };
            let posted = unsafe {
                stream.post_rma_read_raw(
                    target,
                    len,
                    remote_addr,
                    window.key,
                    slot,
                    queued.token,
                    force_completion,
                )?
            };
            if !posted {
                self.post_eagain = self.post_eagain.saturating_add(1);
                self.pending.push_front(queued);
                self.free_slots.push(slot);
                break;
            }
            self.active[slot] = RemoteWalRmaActiveRead {
                batch_id: queued.batch_id,
                token: queued.token,
            };
            if TRACK_LATENCY {
                self.latency_started
                    .as_mut()
                    .expect("latency-specialized RMA posting lost timestamp storage")[slot] =
                    posted_at;
            }
            self.in_flight += 1;
            self.peak_in_flight = self.peak_in_flight.max(self.in_flight);
        }
        Ok(())
    }

    fn take_postable(&mut self) -> Option<(usize, RemoteWalRmaQueuedRead)> {
        if self.pending.is_empty() {
            return None;
        }
        let slot = self.free_slots.pop()?;
        let queued = self
            .pending
            .pop_front()
            .expect("non-empty OFI RMA pending queue became empty");
        Some((slot, queued))
    }

    fn progress(
        &mut self,
        stream: &mut RemoteWalStream,
        mapping: &Mapping,
        window: RemoteWalRmaReadWindow,
        wait: bool,
    ) -> io::Result<RemoteWalRmaProgress> {
        self.post_available(stream, mapping, window, wait)?;
        if self.in_flight == 0 {
            return Ok(RemoteWalRmaProgress::default());
        }
        self.cq_polls = self.cq_polls.saturating_add(1);
        let completed = stream.poll_rma_reads(
            &mut self.completion_slots,
            &mut self.completion_tokens,
            wait,
        )?;
        if completed != 0 {
            self.cq_batches = self.cq_batches.saturating_add(1);
        }
        let mut progress = RemoteWalRmaProgress {
            completions: completed,
            ..RemoteWalRmaProgress::default()
        };
        if completed > self.in_flight {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "OFI RMA completion batch={completed} exceeds in-flight={}",
                    self.in_flight
                ),
            ));
        }
        let free_base = self.free_slots.len();
        let free_end = free_base.checked_add(completed).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "OFI RMA free-list length overflow",
            )
        })?;
        if free_end > self.free_slots.capacity() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "OFI RMA completion batch={completed} exceeds free-list capacity={} len={free_base}",
                    self.free_slots.capacity()
                ),
            ));
        }
        let free_ptr = self.free_slots.as_mut_ptr();
        let mut completion_run_batch_id = 0u64;
        let mut completion_run_records = 0usize;
        for index in 0..completed {
            let slot = self.completion_slots[index];
            let token = self.completion_tokens[index];
            let active = self.active.get_mut(slot).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("OFI RMA completion returned invalid slot={slot}"),
                )
            })?;
            if active.token == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("OFI RMA completion returned inactive slot={slot}"),
                ));
            }
            let active_batch_id = active.batch_id;
            let active_token = active.token;
            // `token == 0` is the sole ownership sentinel. The next post
            // overwrites both words, so retaining the stale batch ID avoids
            // an unnecessary second 64-bit clear on every completion.
            active.token = 0;
            if active_token != token {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "OFI RMA completion token mismatch slot={slot} expected={active_token} actual={token}"
                    ),
                ));
            }
            if completion_run_records != 0 && completion_run_batch_id != active_batch_id {
                self.complete_batch_run(completion_run_batch_id, completion_run_records)?;
                completion_run_records = 0;
            }
            completion_run_batch_id = active_batch_id;
            completion_run_records += 1;
            // The queue owns `capacity == depth`, no operation below mutates
            // this allocation, and `free_end` was checked above. Initialize
            // the returned slots in place, then publish their length once
            // after the entire provider-proven group has been validated.
            unsafe { free_ptr.add(free_base + index).write(slot) };
        }
        if completion_run_records != 0 {
            self.complete_batch_run(completion_run_batch_id, completion_run_records)?;
        }
        // Keep optional clock reads and timestamp-vector bounds checks out of
        // the production per-slot ownership loop. Diagnostic runs pay for a
        // second pass; telemetry-disabled runs take one branch per CQ group.
        if let Some(started) = self.latency_started.as_mut() {
            for index in 0..completed {
                let slot = self.completion_slots[index];
                if let Some(posted_at) = started[slot].take() {
                    progress.completion_time =
                        progress.completion_time.saturating_add(posted_at.elapsed());
                }
            }
        }
        // The C queue has already validated and returned one ownership record
        // per completion. Account the proven group once instead of loading and
        // storing this lane counter for every 4 KiB read.
        self.in_flight -= completed;
        unsafe { self.free_slots.set_len(free_end) };
        self.cq_completions = self.cq_completions.saturating_add(completed as u64);
        self.post_available(stream, mapping, window, false)?;
        Ok(progress)
    }

    fn batch_complete(&self, batch_id: u64) -> io::Result<bool> {
        self.batch_index(batch_id)
            .map(|index| self.batches[index].batch.complete)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown OFI RMA batch id={batch_id}"),
                )
            })
    }

    fn complete_batch_run(&mut self, batch_id: u64, records: usize) -> io::Result<()> {
        let batch_index = self.batch_index(batch_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "OFI RMA completion referenced an unknown batch",
            )
        })?;
        let batch = &mut self.batches[batch_index].batch;
        batch.remaining = batch
            .remaining
            .checked_sub(records)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "OFI RMA batch underflow"))?;
        batch.complete = batch.remaining == 0;
        Ok(())
    }

    fn finish_batch(&mut self, batch_id: u64) -> io::Result<RemoteWalRmaBatch> {
        let front = self.batches.front().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown OFI RMA batch id={batch_id}"),
            )
        })?;
        if front.id != batch_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "out-of-order OFI RMA batch retirement id={batch_id} oldest={}",
                    front.id
                ),
            ));
        }
        if !front.batch.complete {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("OFI RMA batch id={batch_id} is incomplete"),
            ));
        }
        self.batches
            .pop_front()
            .map(|entry| entry.batch)
            .ok_or_else(|| io::Error::other("OFI RMA batch queue became empty"))
    }

    #[inline]
    fn batch_index(&self, batch_id: u64) -> Option<usize> {
        let first = self.batches.front()?.id;
        let offset = usize::try_from(batch_id.checked_sub(first)?).ok()?;
        self.batches
            .get(offset)
            .is_some_and(|entry| entry.id == batch_id)
            .then_some(offset)
    }

    fn has_work(&self) -> bool {
        self.in_flight != 0 || !self.pending.is_empty()
    }

    fn incomplete_batch_count(&self) -> usize {
        self.batches
            .iter()
            .filter(|entry| !entry.batch.complete)
            .count()
    }
}

impl RemoteWalRmaWriteQueue {
    fn new(depth: usize) -> io::Result<Self> {
        if !(1..=65_536).contains(&depth) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("OFI RMA write queue depth must be in 1..=65536, got {depth}"),
            ));
        }
        Ok(Self {
            depth,
            free_slots: (0..depth).rev().collect(),
            active: (0..depth).map(|_| None).collect(),
            completion_slots: vec![0; depth],
            completion_tokens: vec![0; depth],
            next_token: 1,
            peak_in_flight: 0,
            cq_polls: 0,
            cq_batches: 0,
            cq_completions: 0,
            post_eagain: 0,
            completed_batches: 0,
            completed_operations: 0,
            multi_run_batches: 0,
            concurrently_posted_batches: 0,
            batches_exceeding_depth: 0,
            post_rounds: 0,
            max_batch_operations: 0,
            depth_barriers: 0,
            overlap_barriers: 0,
            write_more: env_enabled_or("URING_PLAY_OFI_RMA_WRITE_MORE", false),
            more_posts: 0,
            flush_posts: 0,
            more_followup_eagain: 0,
        })
    }

    fn write_batch(
        &mut self,
        stream: &mut RemoteWalStream,
        mapping: &Mapping,
        window: RemoteWalRmaReadWindow,
        runs: &[RemoteWalRmaWriteRun],
    ) -> io::Result<()> {
        if runs.is_empty() {
            return Ok(());
        }
        if self.free_slots.len() != self.depth || self.active.iter().any(Option::is_some) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "OFI RMA write queue was not idle at payload-batch admission",
            ));
        }
        // Validate the complete batch before the first post. Once libfabric
        // accepts an RMA write, the registered shared-mapping lease must stay
        // live until its delivery CQE; a later deterministic range error must
        // therefore never strand an already-posted prefix of the batch.
        validate_remote_wal_rma_write_runs(mapping.len, window, runs)?;
        let timeout = Duration::from_millis(
            env::var("URING_PLAY_OFI_TIMEOUT_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(30_000)
                .max(1),
        );
        let started = Instant::now();
        let mut next_run = 0usize;
        let mut in_flight = 0usize;
        let mut batch_peak_in_flight = 0usize;
        let mut batch_post_rounds = 0u64;
        while next_run < runs.len() || in_flight != 0 {
            let mut posted = 0usize;
            let mut previous_post_used_more = false;
            while next_run < runs.len() {
                let run = runs[next_run];
                let source = mapping.slice(run.source_offset, run.len)?;
                // A delivery CQE is an ordering barrier for overlapping ranges.
                // Preserve input order while still allowing disjoint payload runs
                // to occupy the configured RMA queue concurrently.
                if remote_wal_rma_write_overlaps_active(&self.active, run) {
                    self.overlap_barriers = self.overlap_barriers.saturating_add(1);
                    break;
                }
                let Some(slot) = self.free_slots.pop() else {
                    self.depth_barriers = self.depth_barriers.saturating_add(1);
                    break;
                };
                let remote_addr = window.addr + run.remote_offset;
                let remote_end = run.remote_offset + run.len as u64;
                let token = self.next_token;
                self.next_token = self.next_token.wrapping_add(1).max(1);
                let more = self.write_more
                    && !self.free_slots.is_empty()
                    && runs.get(next_run + 1).is_some_and(|next| {
                        !remote_wal_rma_write_overlaps_active(&self.active, *next)
                            && !remote_wal_rma_write_runs_overlap(run, *next)
                    });
                let accepted = unsafe {
                    stream.post_rma_write_raw(
                        source.as_ptr(),
                        source.len(),
                        remote_addr,
                        window.key,
                        slot,
                        token,
                        more,
                    )?
                };
                if !accepted {
                    self.post_eagain = self.post_eagain.saturating_add(1);
                    self.more_followup_eagain = self
                        .more_followup_eagain
                        .saturating_add(u64::from(previous_post_used_more));
                    self.free_slots.push(slot);
                    break;
                }
                if more {
                    self.more_posts = self.more_posts.saturating_add(1);
                } else if self.write_more {
                    self.flush_posts = self.flush_posts.saturating_add(1);
                }
                previous_post_used_more = more;
                self.active[slot] = Some(RemoteWalRmaActiveWrite {
                    token,
                    remote_offset: run.remote_offset,
                    remote_end,
                });
                in_flight += 1;
                self.peak_in_flight = self.peak_in_flight.max(in_flight);
                batch_peak_in_flight = batch_peak_in_flight.max(in_flight);
                next_run += 1;
                posted += 1;
            }
            if posted != 0 {
                batch_post_rounds = batch_post_rounds.saturating_add(1);
            }
            if in_flight == 0 {
                if started.elapsed() >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "OFI RMA write queue could not post before timeout",
                    ));
                }
                if posted == 0 {
                    thread::yield_now();
                }
                continue;
            }
            self.cq_polls = self.cq_polls.saturating_add(1);
            let completed = stream.poll_rma_writes(
                &mut self.completion_slots,
                &mut self.completion_tokens,
                true,
            )?;
            if completed != 0 {
                self.cq_batches = self.cq_batches.saturating_add(1);
            }
            for index in 0..completed {
                let slot = self.completion_slots[index];
                let token = self.completion_tokens[index];
                let active = self
                    .active
                    .get_mut(slot)
                    .and_then(Option::take)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("OFI RMA write completion returned inactive slot={slot}"),
                        )
                    })?;
                if active.token != token {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "OFI RMA write completion token mismatch slot={slot} expected={} actual={token}",
                            active.token
                        ),
                    ));
                }
                self.free_slots.push(slot);
                in_flight = in_flight.checked_sub(1).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "OFI RMA write in-flight underflow",
                    )
                })?;
            }
            self.cq_completions = self.cq_completions.saturating_add(completed as u64);
            if completed == 0 && started.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "OFI RMA write queue made no completion progress before timeout",
                ));
            }
        }
        self.completed_batches = self.completed_batches.saturating_add(1);
        self.completed_operations = self.completed_operations.saturating_add(runs.len() as u64);
        self.multi_run_batches = self
            .multi_run_batches
            .saturating_add(u64::from(runs.len() > 1));
        self.concurrently_posted_batches = self
            .concurrently_posted_batches
            .saturating_add(u64::from(batch_peak_in_flight > 1));
        self.batches_exceeding_depth = self
            .batches_exceeding_depth
            .saturating_add(u64::from(runs.len() > self.depth));
        self.post_rounds = self.post_rounds.saturating_add(batch_post_rounds);
        self.max_batch_operations = self.max_batch_operations.max(runs.len());
        Ok(())
    }
}

struct SharedPayloadPlan {
    source_iovecs: u64,
    runs: Vec<(usize, usize)>,
    rma_runs: Vec<RemoteWalRmaWriteRun>,
    max_run_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteWalSendMode {
    Blocking,
    SendZcVectorized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteWalRecvPolicy {
    Fixed,
    Adaptive,
}

impl RemoteWalRecvPolicy {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "fixed" | "bounded" | "spin-then-block" => Ok(Self::Fixed),
            "adaptive" => Ok(Self::Adaptive),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_POLICY must be fixed or adaptive, got {other:?}"
                ),
            )),
        }
    }

    fn from_env() -> io::Result<Self> {
        Self::parse(
            &env::var("URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_POLICY")
                .unwrap_or_else(|_| "adaptive".to_string()),
        )
    }

    fn label(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Adaptive => "adaptive",
        }
    }
}

enum RemoteWalRecvWait {
    Fixed(Option<usize>),
    Adaptive(ZcnblkFanWalAdaptiveRecvSpin),
}

impl RemoteWalRecvWait {
    fn from_env(spin_budget: Option<usize>) -> io::Result<Self> {
        match RemoteWalRecvPolicy::from_env()? {
            RemoteWalRecvPolicy::Fixed => Ok(Self::Fixed(spin_budget)),
            RemoteWalRecvPolicy::Adaptive => {
                let initial = spin_budget.unwrap_or(0);
                let min = parse_optional_usize_env(
                    "URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MIN",
                )?
                .unwrap_or(0);
                let max = parse_optional_usize_env(
                    "URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_SPIN_MAX",
                )?
                .unwrap_or_else(|| initial.max(min).max(65_536).max(1));
                if max < min {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "remote adaptive receive spin maximum is below its minimum",
                    ));
                }
                let wait_ns =
                    parse_optional_usize_env("URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_WAIT_NS")?
                        .unwrap_or(50_000) as u64;
                let hysteresis_ns = parse_optional_usize_env(
                    "URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_ADAPTIVE_HYSTERESIS_NS",
                )?
                .unwrap_or(10_000_000) as u64;
                Ok(Self::Adaptive(ZcnblkFanWalAdaptiveRecvSpin::with_limits(
                    initial,
                    min,
                    max,
                    wait_ns,
                    hysteresis_ns,
                )))
            }
        }
    }

    fn policy(&self) -> RemoteWalRecvPolicy {
        match self {
            Self::Fixed(_) => RemoteWalRecvPolicy::Fixed,
            Self::Adaptive(_) => RemoteWalRecvPolicy::Adaptive,
        }
    }

    fn recv_exact(&mut self, stream: &mut TcpStream, out: &mut [u8]) -> io::Result<()> {
        match self {
            Self::Fixed(spin_budget) => {
                zcnblk_fan_wal_recv_exact_spin_then_block(stream, out, *spin_budget)
            }
            Self::Adaptive(state) => unsafe {
                state.recv_exact_raw(stream, out.as_mut_ptr(), out.len())
            },
        }
    }

    fn adaptive_state(&self) -> Option<&ZcnblkFanWalAdaptiveRecvSpin> {
        match self {
            Self::Adaptive(state) => Some(state),
            Self::Fixed(_) => None,
        }
    }

    fn current_budget_label(&self) -> String {
        match self {
            Self::Fixed(Some(budget)) => budget.to_string(),
            Self::Fixed(None) => "unbounded".to_string(),
            Self::Adaptive(state) => state.current.to_string(),
        }
    }

    fn counters(&self) -> (u64, u64, u64, u64, u64) {
        self.adaptive_state().map_or((0, 0, 0, 0, 0), |state| {
            (
                state.spin_hits,
                state.blocking_fallbacks,
                state.would_block_polls,
                state.grow_events,
                state.shrink_events,
            )
        })
    }
}

fn parse_optional_usize_env(name: &str) -> io::Result<Option<usize>> {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))
        })
        .transpose()
}

fn set_tcp_quickack(stream: &TcpStream) -> io::Result<()> {
    let enabled: libc::c_int = 1;
    let ret = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_QUICKACK,
            (&enabled as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn socket_i32_option(stream: &TcpStream, option: libc::c_int) -> i32 {
    let mut value = 0i32;
    let mut len = size_of::<i32>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut i32).cast(),
            &mut len,
        )
    };
    if ret == 0 && len as usize == size_of::<i32>() {
        value
    } else if option == libc::SO_INCOMING_CPU {
        -1
    } else {
        0
    }
}

fn socket_rx_locality(stream: &TcpStream) -> (i32, i32) {
    (
        socket_i32_option(stream, libc::SO_INCOMING_CPU),
        socket_i32_option(stream, libc::SO_INCOMING_NAPI_ID),
    )
}

impl RemoteWalSendMode {
    fn from_env() -> io::Result<Self> {
        let value = env::var("URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE")
            .unwrap_or_else(|_| "blocking".to_string());
        match value.as_str() {
            "blocking" | "writev" | "copy" => Ok(Self::Blocking),
            "send-zc-vectorized" | "zc-vectorized" | "vectorized-zc" => Ok(Self::SendZcVectorized),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE must be blocking or send-zc-vectorized, got {other:?}"
                ),
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Blocking => "blocking-writev",
            Self::SendZcVectorized => "uring-send-zc-vectorized",
        }
    }
}

struct RemoteWalTxBatch {
    frame: Box<[u8; ZCNBLK_FAN_WAL_HEADER_LEN]>,
    descriptors: Box<[u8]>,
    mapping: Arc<Mapping>,
    iovecs: Vec<libc::iovec>,
    first_iovec: usize,
    remaining_bytes: usize,
    attempts: SendZcBatchAttempts,
    failure: Option<String>,
    notifications: usize,
    copied_notifications: usize,
}

impl RemoteWalTxBatch {
    fn new(
        frame: ZcnblkFanWalFrame,
        descriptors: Vec<u8>,
        mapping: Arc<Mapping>,
        payload_runs: &[(usize, usize)],
    ) -> io::Result<Self> {
        let frame = Box::new(frame.encode());
        let descriptors = descriptors.into_boxed_slice();
        let mut batch = Self {
            frame,
            descriptors,
            mapping,
            iovecs: Vec::with_capacity(payload_runs.len().saturating_add(2)),
            first_iovec: 0,
            remaining_bytes: 0,
            attempts: SendZcBatchAttempts::default(),
            failure: None,
            notifications: 0,
            copied_notifications: 0,
        };
        batch.push_iovec(batch.frame.as_ptr(), batch.frame.len())?;
        if !batch.descriptors.is_empty() {
            batch.push_iovec(batch.descriptors.as_ptr(), batch.descriptors.len())?;
        }
        for &(offset, len) in payload_runs {
            let payload = batch.mapping.slice(offset, len)?;
            batch.push_iovec(payload.as_ptr(), payload.len())?;
        }
        if batch.iovecs.len() > 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "remote WAL vectorized send requires {} iovecs, exceeding Linux UIO_MAXIOV=1024; reduce extent records or improve arena locality",
                    batch.iovecs.len()
                ),
            ));
        }
        Ok(batch)
    }

    fn push_iovec(&mut self, ptr: *const u8, len: usize) -> io::Result<()> {
        self.remaining_bytes = self.remaining_bytes.checked_add(len).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote WAL vectorized send byte count overflow",
            )
        })?;
        self.iovecs.push(libc::iovec {
            iov_base: ptr as *mut libc::c_void,
            iov_len: len,
        });
        Ok(())
    }

    fn active_iovecs(&self) -> &[libc::iovec] {
        &self.iovecs[self.first_iovec..]
    }

    fn mark_initial(&mut self, attempt: usize, result: i32, flags: u32) -> io::Result<bool> {
        self.attempts
            .mark_initial(attempt, flags & IORING_CQE_F_MORE != 0)?;
        if result > 0 {
            advance_send_zc_iovecs(
                &mut self.iovecs,
                &mut self.first_iovec,
                &mut self.remaining_bytes,
                result as usize,
            )?;
        } else if result == 0 {
            self.failure = Some("remote WAL vectorized send completed zero bytes".to_string());
        } else {
            self.failure = Some(io::Error::from_raw_os_error(-result).to_string());
        }
        Ok(result > 0 && self.remaining_bytes != 0)
    }

    fn mark_notification(&mut self, attempt: usize, result: i32) -> io::Result<()> {
        let copied = result & IORING_NOTIF_USAGE_ZC_COPIED != 0;
        self.attempts.mark_notification(attempt, copied)?;
        self.notifications += 1;
        if copied {
            self.copied_notifications += 1;
        }
        Ok(())
    }

    fn done(&self) -> bool {
        (self.remaining_bytes == 0 || self.failure.is_some()) && self.attempts.live_attempts() == 0
    }
}

#[derive(Clone, Copy)]
struct RemoteWalTxAttempt {
    batch_id: u64,
    local_attempt: usize,
}

struct RemoteWalTxCompletion {
    notifications: usize,
    copied_notifications: usize,
    failure: Option<String>,
}

struct RemoteWalUringTx {
    ring: RawRing,
    fd: i32,
    next_batch_id: u64,
    next_user_data: u64,
    batches: HashMap<u64, RemoteWalTxBatch>,
    attempts: HashMap<u64, RemoteWalTxAttempt>,
}

struct RemoteWalTxContext {
    uring: Option<RemoteWalUringTx>,
    pending_batches: VecDeque<RemoteWalPendingTx>,
}

enum RemoteWalPendingTx {
    BlockingControl,
    SendZc(u64),
}

impl RemoteWalTxContext {
    fn new(remote: &RemoteWalLeaf) -> io::Result<Self> {
        let uring = match remote.send_mode {
            RemoteWalSendMode::Blocking => None,
            RemoteWalSendMode::SendZcVectorized => Some(RemoteWalUringTx::new(
                remote.stream.raw_fd().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "remote WAL send-zc requires a TCP socket",
                    )
                })?,
                remote.lane_id,
            )?),
        };
        Ok(Self {
            uring,
            pending_batches: VecDeque::new(),
        })
    }

    fn ensure_idle(&self) -> io::Result<()> {
        if self.pending_batches.is_empty() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "remote WAL sender stopped with {} zero-copy batches still pending",
                    self.pending_batches.len()
                ),
            ))
        }
    }
}

impl RemoteWalUringTx {
    fn new(fd: i32, lane_id: u32) -> io::Result<Self> {
        if !kernel_supports_request_opcode(IORING_OP_SEND_ZC)? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote WAL send-zc-vectorized requires IORING_OP_SEND_ZC",
            ));
        }
        validate_uring_send_mode_location(UringSendMode::SendZcVectorized)?;
        let entries = env::var("URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_RING_ENTRIES")
            .ok()
            .map(|value| value.parse::<u32>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(256)
            .max(8);
        let ring = RawRing::new(entries, entries.saturating_mul(2))?;
        ring.register_napi_from_env(&format!("zcnblk-shm-remote-send-{lane_id}"))?;
        Ok(Self {
            ring,
            fd,
            next_batch_id: 1,
            next_user_data: 1,
            batches: HashMap::new(),
            attempts: HashMap::new(),
        })
    }

    fn queue(&mut self, batch: RemoteWalTxBatch) -> io::Result<u64> {
        let batch_id = self.next_batch_id;
        self.next_batch_id = self.next_batch_id.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "remote WAL batch id overflow")
        })?;
        self.batches.insert(batch_id, batch);
        if let Err(err) = self.queue_attempt(batch_id) {
            self.batches.remove(&batch_id);
            return Err(err);
        }
        self.ring.submit_pending()?;
        Ok(batch_id)
    }

    fn queue_attempt(&mut self, batch_id: u64) -> io::Result<()> {
        let user_data = self.next_user_data;
        self.next_user_data = self.next_user_data.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "remote WAL send user_data overflow",
            )
        })?;
        let batch = self.batches.get_mut(&batch_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "remote WAL send batch disappeared",
            )
        })?;
        let local_attempt = batch.attempts.begin();
        self.ring.queue_send_zc_vectorized(
            self.fd,
            batch.active_iovecs(),
            libc::MSG_NOSIGNAL as u32,
            true,
            user_data,
        )?;
        self.attempts.insert(
            user_data,
            RemoteWalTxAttempt {
                batch_id,
                local_attempt,
            },
        );
        Ok(())
    }

    fn process_cqe(&mut self, cqe: super::IoUringCqe32) -> io::Result<()> {
        let attempt = *self.attempts.get(&cqe.user_data).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown remote WAL send CQE user_data={}", cqe.user_data),
            )
        })?;
        let (retry, retired) = {
            let batch = self.batches.get_mut(&attempt.batch_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "remote WAL CQE batch disappeared",
                )
            })?;
            let retry = if cqe.flags & IORING_CQE_F_NOTIF != 0 {
                batch.mark_notification(attempt.local_attempt, cqe.res)?;
                false
            } else {
                batch.mark_initial(attempt.local_attempt, cqe.res, cqe.flags)?
            };
            let retired = batch.attempts.attempt_retired(attempt.local_attempt)?;
            (retry, retired)
        };
        if retired {
            self.attempts.remove(&cqe.user_data);
        }
        if retry {
            self.queue_attempt(attempt.batch_id)?;
            self.ring.submit_pending()?;
        }
        Ok(())
    }

    fn wait(&mut self, batch_id: u64) -> io::Result<RemoteWalTxCompletion> {
        loop {
            let done = self
                .batches
                .get(&batch_id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "remote WAL wait batch missing")
                })?
                .done();
            if done {
                let batch = self.batches.remove(&batch_id).expect("batch was checked");
                return Ok(RemoteWalTxCompletion {
                    notifications: batch.notifications,
                    copied_notifications: batch.copied_notifications,
                    failure: batch.failure,
                });
            }
            let cqe = self.ring.wait_cqe()?;
            self.process_cqe(cqe)?;
        }
    }

    fn wait_transmitted(&mut self, batch_id: u64) -> io::Result<()> {
        loop {
            let batch = self.batches.get(&batch_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "remote WAL transmit batch missing",
                )
            })?;
            if batch.failure.is_some() {
                let completion = self.wait(batch_id)?;
                return Err(io::Error::other(format!(
                    "remote WAL vectorized send failed before transmit completion: {}",
                    completion
                        .failure
                        .unwrap_or_else(|| "unknown send failure".to_string())
                )));
            }
            if batch.remaining_bytes == 0 {
                return Ok(());
            }
            let cqe = self.ring.wait_cqe()?;
            self.process_cqe(cqe)?;
        }
    }
}

fn shared_payload_plan(
    payloads: impl IntoIterator<Item = (usize, u64, usize)>,
) -> io::Result<SharedPayloadPlan> {
    let mut source_iovecs = 0u64;
    let mut runs = Vec::<(usize, usize)>::new();
    let mut rma_runs = Vec::<RemoteWalRmaWriteRun>::new();
    let mut max_run_bytes = 0u64;
    for (offset, remote_offset, len) in payloads {
        let end = offset.checked_add(len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "shared payload range overflow")
        })?;
        remote_offset.checked_add(len as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "remote payload range overflow")
        })?;
        source_iovecs = source_iovecs.saturating_add(1);
        if let Some((run_offset, run_len)) = runs.last_mut()
            && run_offset.checked_add(*run_len) == Some(offset)
        {
            *run_len = run_len.checked_add(len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "shared payload run overflow")
            })?;
            max_run_bytes = max_run_bytes.max(*run_len as u64);
        } else {
            runs.push((offset, len));
            max_run_bytes = max_run_bytes.max(len as u64);
        }
        debug_assert_eq!(end, offset + len);
        if let Some(run) = rma_runs.last_mut()
            && run.source_offset.checked_add(run.len) == Some(offset)
            && run.remote_offset.checked_add(run.len as u64) == Some(remote_offset)
        {
            run.len = run.len.checked_add(len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "RMA payload run overflow")
            })?;
        } else {
            rma_runs.push(RemoteWalRmaWriteRun {
                source_offset: offset,
                remote_offset,
                len,
            });
        }
    }
    Ok(SharedPayloadPlan {
        source_iovecs,
        runs,
        rma_runs,
        max_run_bytes,
    })
}

fn compact_write_batch_descriptors(
    lane_id: u32,
    requests: &[PendingRemoteRead],
) -> io::Result<(Vec<u8>, usize, usize)> {
    if requests.is_empty()
        || requests
            .iter()
            .any(|request| request.request.op != ZCNBLK_SHM_OP_WRITE)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "compact remote WAL batch requires writes only",
        ));
    }
    let write_payload_len = requests.iter().try_fold(0usize, |total, request| {
        total
            .checked_add(request.request.len as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "compact payload overflow"))
    })?;
    let original_descriptor_len = requests
        .len()
        .checked_mul(ZCNBLK_FAN_WAL_HEADER_LEN)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "original descriptor size overflow",
            )
        })?;
    let descriptor_len = requests
        .len()
        .checked_mul(ZCNBLK_FAN_WAL_COMPACT_WRITE_EXTENT_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "compact descriptor overflow"))?;
    let mut descriptors = Vec::with_capacity(descriptor_len);
    for request in requests {
        let extent = ZcnblkFanWalCompactWriteExtent {
            leaf_offset: request.request.offset,
            logical_offset: request.request.offset,
            payload_len: request.request.len,
            record_count: 1,
            mode_selector: request.request.submit_sequence ^ u64::from(lane_id),
            io_contract_word: request.io_contract.encode()?,
            lease_id: request.io_contract.lease_id,
        };
        descriptors.extend_from_slice(&extent.encode());
    }
    Ok((descriptors, write_payload_len, original_descriptor_len))
}

fn lane_env_entry(name: &str, lane_id: u32, lane_count: u32) -> io::Result<Option<String>> {
    let Ok(value) = env::var(name) else {
        return Ok(None);
    };
    let entries = value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    match entries.len() {
        0 => Ok(None),
        1 => Ok(Some(entries[0].to_string())),
        count if count == lane_count as usize => Ok(Some(entries[lane_id as usize].to_string())),
        count => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{name} must contain one entry or one per lane: got {count} for {lane_count} lanes"
            ),
        )),
    }
}

impl RemoteWalLeaf {
    fn connect(lane_id: u32, lane_count: u32, rma_read_buffer_bytes: usize) -> io::Result<Self> {
        let base_address = lane_env_entry("URING_PLAY_ZCNBLK_SHM_LEAF_ADDRS", lane_id, lane_count)?
            .or_else(|| env::var("URING_PLAY_ZCNBLK_SHM_LEAF_ADDR").ok())
            .unwrap_or_else(|| "127.0.0.1:29000".to_string());
        let mut socket_address = resolve_remote_wal_leaf_address(&base_address)?;
        let lane_offset = u16::try_from(lane_id).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "remote WAL lane exceeds u16")
        })?;
        socket_address.set_port(socket_address.port().checked_add(lane_offset).ok_or_else(
            || io::Error::new(io::ErrorKind::InvalidInput, "remote WAL lane port overflow"),
        )?);
        let source_ip = lane_env_entry(
            "URING_PLAY_ZCNBLK_SHM_LEAF_SOURCE_ADDRS",
            lane_id,
            lane_count,
        )?
        .or_else(|| env::var("URING_PLAY_ZCNBLK_SHM_LEAF_SOURCE_ADDR").ok())
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value.trim().parse::<IpAddr>().map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid URING_PLAY_ZCNBLK_SHM_LEAF_SOURCE_ADDR={value:?}: {err}"),
                )
            })
        })
        .transpose()?;
        let transport = env::var("URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT")
            .unwrap_or_else(|_| "tcp".to_string());
        let rma_reads_enabled = env_enabled_or("URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS", false);
        let rma_writes_enabled = env_enabled_or("URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES", false);
        let (mut stream, address, tcp_nodelay, quickack) = match transport.as_str() {
            "tcp" => {
                let tcp = connect_remote_wal_tcp(socket_address, source_ip)?;
                set_tcp_bench_buffers(&tcp);
                let local_address = tcp.local_addr()?;
                let address = format!("{local_address}->{socket_address}");
                tcp.set_nodelay(true)?;
                let tcp_nodelay = tcp.nodelay()?;
                let quickack = env_enabled_or("URING_PLAY_ZCNBLK_SHM_REMOTE_QUICKACK", false);
                eprintln!(
                    "zcnblk-shm-target-remote-connect: transport=tcp lane={lane_id} address={address} tcp_nodelay={tcp_nodelay} quickack={quickack} socket_buffer_bytes={}",
                    socket_bench_buffer_bytes(),
                );
                (RemoteWalStream::Tcp(tcp), address, tcp_nodelay, quickack)
            }
            "ofi" | "rdm" | "efa" => {
                if source_ip.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "OFI WAL transport selects locality with URING_PLAY_OFI_DOMAIN, not URING_PLAY_ZCNBLK_SHM_LEAF_SOURCE_ADDR",
                    ));
                }
                let provider = env::var("URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER")
                    .unwrap_or_else(|_| "efa".to_string());
                let endpoint = env::var("URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_ENDPOINT")
                    .unwrap_or_else(|_| "rdm".to_string());
                let domain =
                    lane_env_entry("URING_PLAY_ZCNBLK_SHM_OFI_DOMAINS", lane_id, lane_count)?
                        .or_else(|| env::var("URING_PLAY_OFI_DOMAIN").ok())
                        .filter(|value| !value.trim().is_empty());
                let node = socket_address.ip().to_string();
                let ofi = ZcOfiMessageStream::connect_on_domain(
                    &provider,
                    &endpoint,
                    &node,
                    socket_address.port(),
                    false,
                    rma_reads_enabled || rma_writes_enabled,
                    domain.as_deref(),
                )?;
                let message_bytes = ofi.message_bytes();
                let address = format!("ofi://{node}:{}", socket_address.port());
                eprintln!(
                    "zcnblk-shm-target-remote-connect: transport=ofi provider={provider} endpoint={endpoint} lane={lane_id} domain={} address={address} message_bytes={message_bytes} cq_sleep_ns={} placement_owner=external-userspace-stage block_client_placement=no",
                    domain.as_deref().unwrap_or("implicit"),
                    env::var("URING_PLAY_OFI_CQ_SLEEP_NS").unwrap_or_else(|_| "50000".to_string()),
                );
                (RemoteWalStream::Ofi(ofi), address, false, false)
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT must be tcp or ofi, got {other:?}"
                    ),
                ));
            }
        };
        let rma_reads_requested = matches!(stream, RemoteWalStream::Ofi(_)) && rma_reads_enabled;
        let rma_writes_requested = matches!(stream, RemoteWalStream::Ofi(_)) && rma_writes_enabled;
        let hello = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_HELLO,
            flags: ZCNBLK_FAN_WAL_FLAG_RESULT_RANGE_BATCH
                | if rma_reads_requested {
                    ZCNBLK_FAN_WAL_FLAG_OFI_RMA_READ_WINDOW
                } else {
                    0
                }
                | if rma_writes_requested {
                    ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_WINDOW
                } else {
                    0
                },
            lane_id,
            lane_count,
            branch_id: 0,
            branch_count: 1,
            ..ZcnblkFanWalFrame::default()
        }
        .with_hello_features(ZCNBLK_WAL_FEATURE_ALL)?;
        zcnblk_fan_wal_write_frame(&mut stream, hello, &[])?;
        let mut hello_ack_bytes = [0u8; ZCNBLK_FAN_WAL_HEADER_LEN];
        stream.read_exact(&mut hello_ack_bytes)?;
        let hello_ack = ZcnblkFanWalFrame::decode(&hello_ack_bytes)?;
        if hello_ack.op != ZCNBLK_FAN_WAL_OP_HELLO_ACK
            || hello_ack.lane_id != lane_id
            || hello_ack.lane_count != lane_count
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote WAL leaf returned an invalid I/O contract hello acknowledgement",
            ));
        }
        let negotiated_features = hello_ack.hello_features()?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "remote WAL leaf omitted negotiated I/O capabilities",
            )
        })?;
        let rma_read_window = if hello_ack.flags & ZCNBLK_FAN_WAL_FLAG_OFI_RMA_READ_WINDOW != 0 {
            if !rma_reads_requested || hello_ack.sync_epoch == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "remote WAL leaf returned an invalid unsolicited OFI RMA read window",
                ));
            }
            let window = RemoteWalRmaReadWindow {
                addr: hello_ack.logical_offset,
                key: hello_ack.leaf_offset,
                len: hello_ack.sync_epoch,
            };
            eprintln!(
                "zcnblk-shm-target-ofi-rma-read-window: lane={lane_id} bytes={} addr={:#x} key={:#x} completion=initiator-local-cq-data-visible remote_ack=no sync_fua=separate-message-path",
                window.len, window.addr, window.key,
            );
            Some(window)
        } else {
            if rma_reads_requested {
                eprintln!(
                    "zcnblk-shm-target-ofi-rma-read-window: lane={lane_id} negotiated=no fallback=message-request-response"
                );
            }
            None
        };
        let rma_write_required =
            env_enabled_or("URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES_REQUIRED", false);
        let rma_write_window = if hello_ack.flags & ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_WINDOW != 0 {
            if !rma_writes_requested || hello_ack.sync_epoch == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "remote WAL leaf returned an invalid unsolicited OFI RMA write window",
                ));
            }
            let window = RemoteWalRmaReadWindow {
                addr: hello_ack.logical_offset,
                key: hello_ack.leaf_offset,
                len: hello_ack.sync_epoch,
            };
            eprintln!(
                "zcnblk-shm-target-ofi-rma-write-window: lane={lane_id} bytes={} addr={:#x} key={:#x} completion=initiator-delivery-cq-before-doorbell remote_ack=doorbell-result-hwm sync_fua=leaf-after-doorbell",
                window.len, window.addr, window.key,
            );
            Some(window)
        } else {
            if rma_writes_requested {
                eprintln!(
                    "zcnblk-shm-target-ofi-rma-write-window: lane={lane_id} negotiated=no fallback=message-payload"
                );
                if rma_write_required {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "OFI RMA write payloads are required but the remote leaf did not negotiate a write window",
                    ));
                }
            }
            None
        };
        eprintln!(
            "zcnblk-shm-target-remote-contract: lane={lane_id} requested={ZCNBLK_WAL_FEATURE_ALL:#x} negotiated={negotiated_features:#x}"
        );
        let recv_spin_spec =
            env::var("URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_SPINS").unwrap_or_else(|_| "0".to_string());
        let recv_spin_budget = if matches!(
            recv_spin_spec.to_ascii_lowercase().as_str(),
            "unbounded" | "infinite" | "greedy"
        ) {
            None
        } else {
            Some(recv_spin_spec.parse::<usize>().map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "invalid URING_PLAY_ZCNBLK_SHM_REMOTE_RECV_SPINS={recv_spin_spec:?}: {err}"
                    ),
                )
            })?)
        };
        let recv_wait = RemoteWalRecvWait::from_env(recv_spin_budget)?;
        let send_mode = RemoteWalSendMode::from_env()?;
        if matches!(stream, RemoteWalStream::Ofi(_)) && send_mode != RemoteWalSendMode::Blocking {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "direct OFI WAL transport requires blocking send mode; io_uring send-zc is TCP-only",
            ));
        }
        let rma_read_qd = env::var("URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD")
            .unwrap_or_else(|_| "1".to_string())
            .parse::<usize>()
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD: {error}"),
                )
            })?;
        if !(1..=1024).contains(&rma_read_qd) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "URING_PLAY_ZCNBLK_SHM_OFI_RMA_READ_QD must be in 1..=1024, got {rma_read_qd}"
                ),
            ));
        }
        let rma_read_queue = if rma_read_window.is_some() {
            Some(RemoteWalRmaReadQueue::new(
                rma_read_buffer_bytes,
                rma_read_qd,
            )?)
        } else {
            None
        };
        if let Some(queue) = rma_read_queue.as_ref() {
            stream.configure_rma_read_queue(queue.depth)?;
            eprintln!(
                "zcnblk-shm-target-ofi-rma-local-buffer: lane={lane_id} qd={} slot_bytes={} ring_bytes=0 registration_scope=deferred-whole-shared-mapping destination=direct-shared-slot cq_processing=batched",
                queue.depth, queue.slot_bytes,
            );
        }
        let rma_write_qd = env::var("URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_QD")
            .or_else(|_| env::var("URING_PLAY_OFI_RMA_WRITE_QD"))
            .unwrap_or_else(|_| "1".to_string())
            .parse::<usize>()
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid OFI RMA write queue depth: {error}"),
                )
            })?;
        let mut rma_write_queue = if rma_write_window.is_some() {
            Some(RemoteWalRmaWriteQueue::new(rma_write_qd)?)
        } else {
            None
        };
        if let Some(queue) = rma_write_queue.as_mut() {
            stream.configure_rma_write_queue(queue.depth)?;
            eprintln!(
                "zcnblk-shm-target-ofi-rma-write-queue: lane={lane_id} per_lane_qd={} qd_scope=payload-operations block_qd_coupled=no source_registration=deferred-whole-shared-mapping completion=delivery-complete-before-doorbell pipeline=one-unacknowledged-doorbell-per-lane",
                queue.depth,
            );
        }
        Ok(Self {
            stream,
            mapping: None,
            address,
            lane_id,
            lane_count,
            negotiated_features,
            rma_read_window,
            rma_read_queue,
            rma_write_window,
            rma_write_queue,
            rma_write_required,
            request_batches_pending: 0,
            rma_write_doorbell_pending: false,
            target_cpu: None,
            write_batches: 0,
            write_records: 0,
            write_bytes: 0,
            write_payload_iovecs: 0,
            write_payload_tx_iovecs: 0,
            write_payload_runs: 0,
            max_write_payload_run_bytes: 0,
            compact_writes: env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_COMPACT_WRITES", false),
            compact_write_batches: 0,
            request_descriptor_bytes: 0,
            wire_descriptor_bytes: 0,
            read_records: 0,
            read_bytes: 0,
            read_batches: 0,
            syncs: 0,
            sync_time: Duration::ZERO,
            recv_wait,
            send_mode,
            require_send_zc: env_enabled_or("URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_ZC_REQUIRED", true),
            control_writev_batches: 0,
            send_zc_notifications: 0,
            send_zc_copied_notifications: 0,
            tcp_nodelay,
            quickack,
            request_send_calls: 0,
            request_send_time: Duration::ZERO,
            result_recv_calls: 0,
            result_recv_time: Duration::ZERO,
            result_header_time: Duration::ZERO,
            result_descriptor_time: Duration::ZERO,
            result_payload_time: Duration::ZERO,
            rma_read_calls: 0,
            rma_read_time: Duration::ZERO,
            rma_read_copy_time: Duration::ZERO,
            rma_write_batches: 0,
            rma_write_calls: 0,
            rma_write_bytes: 0,
            rma_write_time: Duration::ZERO,
        })
    }

    fn attach_mapping(&mut self, mapping: Arc<Mapping>) -> io::Result<()> {
        if let Some(attached) = self.mapping.as_ref() {
            if Arc::ptr_eq(attached, &mapping) {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "remote WAL leaf already has a different shared mapping attached",
            ));
        }
        if self.rma_read_window.is_some() {
            // Read responses land directly in request-owned shared payload
            // slots. Register once before any post so strict topology never
            // permits a hot-path MR registration.
            unsafe {
                self.stream
                    .register_rma_read_buffer_raw(mapping.ptr, mapping.len)?;
            }
            eprintln!(
                "zcnblk-shm-target-ofi-rma-read-destination: lane={} registered_bytes={} registration_scope=whole-shared-mapping hot_registration=no placement=direct-request-owned-shared-slot copy_after_cq=no",
                self.lane_id, mapping.len,
            );
        }
        if self.rma_write_window.is_some() {
            // Register the mapping once, before any post. Payload leases keep
            // source ranges stable through local delivery completion and the
            // later remote result HWM keeps block-slot reuse ordered.
            unsafe {
                self.stream
                    .register_rma_write_buffer_raw(mapping.ptr.cast_const(), mapping.len)?;
            }
            eprintln!(
                "zcnblk-shm-target-ofi-rma-write-source: lane={} registered_bytes={} registration_scope=whole-shared-mapping hot_registration=no",
                self.lane_id, mapping.len,
            );
        }
        self.mapping = Some(mapping);
        Ok(())
    }

    fn try_submit_rma_read_batch(
        &mut self,
        requests: &[PendingRemoteRead],
    ) -> io::Result<Option<u64>> {
        let Some(window) = self.rma_read_window else {
            return Ok(None);
        };
        if requests.is_empty()
            || requests
                .iter()
                .any(|request| request.request.op != ZCNBLK_SHM_OP_READ)
        {
            return Ok(None);
        }
        let mapping = self.mapping.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "OFI RMA read sender has no registered shared mapping",
            )
        })?;
        let queue = self.rma_read_queue.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "OFI RMA read window has no registered lane-local buffer ring",
            )
        })?;
        let batch_id = queue.submit_batch(window, mapping.len, requests)?;
        queue.post_available(&mut self.stream, mapping.as_ref(), window, false)?;
        Ok(Some(batch_id))
    }

    fn progress_rma_reads(&mut self, mapping: &Mapping, wait: bool) -> io::Result<usize> {
        let Some(window) = self.rma_read_window else {
            return Ok(0);
        };
        let queue = self.rma_read_queue.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "OFI RMA read window has no registered lane-local buffer ring",
            )
        })?;
        let progress = queue.progress(&mut self.stream, mapping, window, wait)?;
        self.rma_read_calls = self
            .rma_read_calls
            .saturating_add(progress.completions as u64);
        self.rma_read_time = self.rma_read_time.saturating_add(progress.completion_time);
        self.rma_read_copy_time = self.rma_read_copy_time.saturating_add(progress.copy_time);
        Ok(progress.completions)
    }

    fn progress_rma_reads_attached(&mut self, wait: bool) -> io::Result<usize> {
        /* The mapping is permanently attached before any data-plane post.
         * Borrow its Arc payload alongside the disjoint stream/queue fields;
         * cloning it here put one contended refcount increment and decrement
         * around every lane CQ progress call. */
        let progress = {
            let window = self.rma_read_window.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "OFI RMA progress requested without a negotiated read window",
                )
            })?;
            let mapping = self.mapping.as_deref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "OFI RMA read queue has no attached shared mapping",
                )
            })?;
            let queue = self.rma_read_queue.as_mut().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "OFI RMA read window has no registered lane-local buffer ring",
                )
            })?;
            queue.progress(&mut self.stream, mapping, window, wait)?
        };
        self.rma_read_calls = self
            .rma_read_calls
            .saturating_add(progress.completions as u64);
        self.rma_read_time = self.rma_read_time.saturating_add(progress.completion_time);
        self.rma_read_copy_time = self.rma_read_copy_time.saturating_add(progress.copy_time);
        Ok(progress.completions)
    }

    fn rma_read_batch_complete(&self, batch_id: u64) -> io::Result<bool> {
        self.rma_read_queue
            .as_ref()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "OFI RMA read queue is absent")
            })?
            .batch_complete(batch_id)
    }

    fn finish_rma_read_batch(&mut self, batch_id: u64) -> io::Result<()> {
        let batch = self
            .rma_read_queue
            .as_mut()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "OFI RMA read queue is absent")
            })?
            .finish_batch(batch_id)?;
        self.read_batches = self.read_batches.saturating_add(1);
        self.read_records = self.read_records.saturating_add(batch.records as u64);
        self.read_bytes = self.read_bytes.saturating_add(batch.bytes);
        Ok(())
    }

    fn drain_rma_reads(&mut self, mapping: &Mapping) -> io::Result<()> {
        let timeout =
            Duration::from_millis(u64::try_from(crate::zcofi_timeout_ms()).unwrap_or(30_000));
        let mut stalled_at = None::<Instant>;
        loop {
            let has_work = self
                .rma_read_queue
                .as_ref()
                .is_some_and(RemoteWalRmaReadQueue::has_work);
            if !has_work {
                let incomplete_batches = self
                    .rma_read_queue
                    .as_ref()
                    .map_or(0, RemoteWalRmaReadQueue::incomplete_batch_count);
                if incomplete_batches != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "OFI RMA queue has no pending or active reads but retains {incomplete_batches} incomplete batch(es)"
                        ),
                    ));
                }
                return Ok(());
            }
            let completed = self.progress_rma_reads(mapping, true)?;
            if completed == 0 {
                let stalled_at = stalled_at.get_or_insert_with(Instant::now);
                if stalled_at.elapsed() >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "OFI RMA read queue made no progress before timeout",
                    ));
                }
                thread::yield_now();
            } else {
                stalled_at = None;
            }
        }
    }

    fn report_rma_read_queue(&self) {
        let Some(queue) = self.rma_read_queue.as_ref() else {
            return;
        };
        eprintln!(
            "zcnblk-shm-target-ofi-rma-queue: lane={} per_lane_qd={} slot_bytes={} registered_ring_bytes=0 peak_in_flight={} final_in_flight={} final_queued={} final_batches={} cq_poll_calls={} cq_batches={} cq_completions={} avg_cq_completions_per_batch={:.2} post_eagain={} deferred_real_tail_marker={} synthetic_partial_flush_policy=fallback-only completion=initiator-local-cq-data-visible buffer_policy=request-owned-shared-slot retirement=fifo-batch-after-out-of-order-cq copy_after_cq=no",
            self.lane_id,
            queue.depth,
            queue.slot_bytes,
            queue.peak_in_flight,
            queue.in_flight,
            queue.pending.len(),
            queue.batches.len(),
            queue.cq_polls,
            queue.cq_batches,
            queue.cq_completions,
            queue.cq_completions as f64 / queue.cq_batches.max(1) as f64,
            queue.post_eagain,
            queue.defer_tail_completion,
        );
    }

    fn report_rma_write_queue(&self) {
        let Some(queue) = self.rma_write_queue.as_ref() else {
            return;
        };
        eprintln!(
            "zcnblk-shm-target-ofi-rma-write-summary: lane={} per_lane_qd={} qd_scope=payload-operations block_qd_coupled=no batches={} operations={} bytes={} seconds={:.6} avg_batch_us={:.3} batch_elapsed_div_operations_us={:.3} avg_operations_per_batch={:.2} multi_run_batches={} concurrently_posted_batches={} batches_exceeding_qd={} avg_post_rounds_per_batch={:.2} max_batch_operations={} depth_barriers={} overlap_barriers={} peak_in_flight={} final_active={} cq_poll_calls={} cq_batches={} cq_completions={} avg_cq_completions_per_batch={:.2} post_eagain={} fi_more={} fi_more_candidates={} fi_more_flush_candidates={} fi_more_followup_eagain={} source=registered-shared-slot-lease destination=remote-leaf-memory-window local_completion=delivery-complete remote_completion=doorbell-result-hwm sync_fua=leaf-after-doorbell",
            self.lane_id,
            queue.depth,
            self.rma_write_batches,
            self.rma_write_calls,
            self.rma_write_bytes,
            self.rma_write_time.as_secs_f64(),
            self.rma_write_time.as_secs_f64() * 1_000_000.0 / self.rma_write_batches.max(1) as f64,
            self.rma_write_time.as_secs_f64() * 1_000_000.0 / self.rma_write_calls.max(1) as f64,
            queue.completed_operations as f64 / queue.completed_batches.max(1) as f64,
            queue.multi_run_batches,
            queue.concurrently_posted_batches,
            queue.batches_exceeding_depth,
            queue.post_rounds as f64 / queue.completed_batches.max(1) as f64,
            queue.max_batch_operations,
            queue.depth_barriers,
            queue.overlap_barriers,
            queue.peak_in_flight,
            queue.active.iter().filter(|entry| entry.is_some()).count(),
            queue.cq_polls,
            queue.cq_batches,
            queue.cq_completions,
            queue.cq_completions as f64 / queue.cq_batches.max(1) as f64,
            queue.post_eagain,
            queue.write_more,
            queue.more_posts,
            queue.flush_posts,
            queue.more_followup_eagain,
        );
    }

    fn send_batch_payload(
        &mut self,
        tx: &mut RemoteWalTxContext,
        mapping: &Mapping,
        mut frame: ZcnblkFanWalFrame,
        descriptors: Vec<u8>,
        payload_plan: &SharedPayloadPlan,
        rma_eligible: bool,
    ) -> io::Result<bool> {
        if rma_eligible && self.rma_write_window.is_some() && !payload_plan.rma_runs.is_empty() {
            if self.request_batches_pending != 0 || self.rma_write_doorbell_pending {
                if self.rma_write_required {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "OFI RMA write payload requires one unacknowledged doorbell per lane; set the userspace transport pipeline depth to 1",
                    ));
                }
            } else {
                let attached = self.mapping.as_ref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "OFI RMA write sender has no registered shared mapping lease",
                    )
                })?;
                if !std::ptr::eq(Arc::as_ptr(attached), mapping) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "OFI RMA write sender mapping mismatch",
                    ));
                }
                let window = self.rma_write_window.expect("RMA write window was checked");
                let started = Instant::now();
                self.rma_write_queue
                    .as_mut()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "OFI RMA write window has no operation queue",
                        )
                    })?
                    .write_batch(&mut self.stream, mapping, window, &payload_plan.rma_runs)?;
                self.rma_write_time = self.rma_write_time.saturating_add(started.elapsed());
                self.rma_write_batches = self.rma_write_batches.saturating_add(1);
                self.rma_write_calls = self
                    .rma_write_calls
                    .saturating_add(payload_plan.rma_runs.len() as u64);
                self.rma_write_bytes = self.rma_write_bytes.saturating_add(
                    payload_plan
                        .rma_runs
                        .iter()
                        .map(|run| run.len as u64)
                        .sum::<u64>(),
                );
                frame.flags |= ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_PAYLOAD;
                zcnblk_fan_wal_write_rma_payload_doorbell(&mut self.stream, frame, &descriptors)?;
                return Ok(true);
            }
        }
        match self.send_mode {
            RemoteWalSendMode::Blocking => {
                let mut payloads = Vec::with_capacity(payload_plan.runs.len());
                for &(offset, len) in &payload_plan.runs {
                    payloads.push(IoSlice::new(mapping.slice(offset, len)?));
                }
                zcnblk_fan_wal_write_leaf_batch_payload(
                    &mut self.stream,
                    frame,
                    &descriptors,
                    &payloads,
                )?;
            }
            RemoteWalSendMode::SendZcVectorized => {
                if payload_plan.runs.is_empty() {
                    zcnblk_fan_wal_write_leaf_batch_payload(
                        &mut self.stream,
                        frame,
                        &descriptors,
                        &[],
                    )?;
                    self.control_writev_batches = self.control_writev_batches.saturating_add(1);
                    tx.pending_batches
                        .push_back(RemoteWalPendingTx::BlockingControl);
                    return Ok(false);
                }
                let attached = self.mapping.as_ref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "remote WAL vectorized sender has no shared mapping lease",
                    )
                })?;
                if !std::ptr::eq(Arc::as_ptr(attached), mapping) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "remote WAL vectorized sender mapping mismatch",
                    ));
                }
                let batch = RemoteWalTxBatch::new(
                    frame,
                    descriptors,
                    Arc::clone(attached),
                    &payload_plan.runs,
                )?;
                let uring = tx
                    .uring
                    .as_mut()
                    .ok_or_else(|| io::Error::other("remote WAL send-zc ring missing"))?;
                let batch_id = uring.queue(batch)?;
                uring.wait_transmitted(batch_id)?;
                tx.pending_batches
                    .push_back(RemoteWalPendingTx::SendZc(batch_id));
            }
        }
        Ok(false)
    }

    fn finish_next_send_batch(&mut self, tx: &mut RemoteWalTxContext) -> io::Result<()> {
        if self.send_mode == RemoteWalSendMode::Blocking {
            return Ok(());
        }
        let pending = tx.pending_batches.pop_front().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "remote WAL result has no matching vectorized send batch",
            )
        })?;
        let RemoteWalPendingTx::SendZc(batch_id) = pending else {
            return Ok(());
        };
        let completion = tx
            .uring
            .as_mut()
            .ok_or_else(|| io::Error::other("remote WAL send-zc ring missing"))?
            .wait(batch_id)?;
        self.send_zc_notifications = self
            .send_zc_notifications
            .saturating_add(completion.notifications as u64);
        self.send_zc_copied_notifications = self
            .send_zc_copied_notifications
            .saturating_add(completion.copied_notifications as u64);
        if let Some(failure) = completion.failure {
            return Err(io::Error::other(format!(
                "remote WAL vectorized send failed: {failure}"
            )));
        }
        if self.require_send_zc {
            if completion.notifications == 0 {
                return Err(io::Error::other(
                    "remote WAL required zero copy but the send produced no notification CQE",
                ));
            }
            if completion.copied_notifications != 0 {
                return Err(io::Error::other(format!(
                    "remote WAL required zero copy but {} notification CQEs reported copied fallback",
                    completion.copied_notifications
                )));
            }
        }
        Ok(())
    }

    fn read_result_frame(&mut self) -> io::Result<ZcnblkFanWalFrame> {
        let mut header = [0u8; ZCNBLK_FAN_WAL_HEADER_LEN];
        self.stream.recv_exact(&mut self.recv_wait, &mut header)?;
        ZcnblkFanWalFrame::decode(&header)
    }

    fn recv_result_exact(&mut self, out: &mut [u8]) -> io::Result<()> {
        self.stream.recv_exact(&mut self.recv_wait, out)
    }

    fn finish_request_batch_tracking(&mut self) -> io::Result<()> {
        self.request_batches_pending =
            self.request_batches_pending.checked_sub(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "remote WAL result arrived without a pending request batch",
                )
            })?;
        if self.rma_write_doorbell_pending {
            self.rma_write_doorbell_pending = false;
        }
        Ok(())
    }

    fn request_frame(
        &self,
        write: &PendingWalWrite,
        op: u16,
        batched: bool,
    ) -> io::Result<ZcnblkFanWalFrame> {
        let zcnblk_op = match op {
            ZCNBLK_FAN_WAL_OP_WRITE_DESC => ZCNBLK_OP_WRITE,
            ZCNBLK_FAN_WAL_OP_READ_DESC => ZCNBLK_OP_READ,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "remote WAL leaf request has unsupported descriptor op",
                ));
            }
        };
        let frame = ZcnblkFanWalFrame {
            op,
            // The block ingress lane remains in PendingWalWrite for local
            // completion and lease release. Wire topology names the stable
            // userspace transport owner selected for this socket.
            lane_id: self.lane_id,
            lane_count: self.lane_count,
            branch_id: 0,
            branch_count: 1,
            segment_index: 0,
            segment_count: 1,
            payload_len: u32::try_from(write.len).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "remote WAL payload exceeds u32",
                )
            })?,
            sequence: write.submit_sequence,
            request_id: write.request.request_id,
            logical_offset: write.offset,
            leaf_offset: write.offset,
            logical_len: write.request.len,
            zcnblk_op,
            zcnblk_flags: write.request.flags,
            topology_preferred_worker: self.lane_id,
            topology_queue_id: self.lane_id,
            topology_flags: u32::from(write.request.flags),
            ..ZcnblkFanWalFrame::default()
        }
        .with_io_contract(write.io_contract)?;
        frame.validate_io_contract(self.negotiated_features, batched)?;
        Ok(frame)
    }

    fn write_batch(
        &mut self,
        tx: &mut RemoteWalTxContext,
        mapping: &Mapping,
        writes: &[PendingWalWrite],
    ) -> io::Result<()> {
        if writes.is_empty() {
            return Ok(());
        }
        let descriptor_len = writes
            .len()
            .checked_mul(ZCNBLK_FAN_WAL_HEADER_LEN)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "WAL descriptor overflow"))?;
        let payload_len = writes.iter().try_fold(0usize, |total, write| {
            total.checked_add(write.len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "WAL batch payload overflow")
            })
        })?;
        let payload_plan = shared_payload_plan(
            writes
                .iter()
                .map(|write| (write.payload_offset, write.offset, write.len)),
        )?;
        let mut descriptors = Vec::with_capacity(descriptor_len);
        for write in writes {
            descriptors.extend_from_slice(
                &self
                    .request_frame(write, ZCNBLK_FAN_WAL_OP_WRITE_DESC, true)?
                    .encode(),
            );
        }
        let batch = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_WRITE_BATCH,
            lane_id: self.lane_id,
            lane_count: self.lane_count,
            branch_id: 0,
            branch_count: 1,
            segment_count: u32::try_from(writes.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "WAL batch count exceeds u32")
            })?,
            payload_len: u32::try_from(descriptor_len.checked_add(payload_len).ok_or_else(
                || io::Error::new(io::ErrorKind::InvalidData, "WAL batch bytes overflow"),
            )?)
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "WAL batch bytes exceed u32")
            })?,
            ..ZcnblkFanWalFrame::default()
        };
        let _used_rma =
            self.send_batch_payload(tx, mapping, batch, descriptors, &payload_plan, true)?;
        self.write_payload_iovecs = self
            .write_payload_iovecs
            .saturating_add(payload_plan.source_iovecs);
        self.write_payload_tx_iovecs = self
            .write_payload_tx_iovecs
            .saturating_add(payload_plan.runs.len() as u64);
        self.write_payload_runs = self
            .write_payload_runs
            .saturating_add(payload_plan.runs.len() as u64);
        self.max_write_payload_run_bytes = self
            .max_write_payload_run_bytes
            .max(payload_plan.max_run_bytes);
        let result = self.read_result_frame()?;
        self.finish_next_send_batch(tx)?;
        let expected_lease_hwm = writes
            .iter()
            .filter(|write| write.io_contract.registered_lease)
            .map(|write| write.io_contract.lease_id)
            .max()
            .unwrap_or(0);
        if result.op != ZCNBLK_FAN_WAL_OP_RESULT_RANGE_BATCH
            || result.status != ZCNBLK_FAN_WAL_STATUS_OK
            || result.branch_id != 0
            || result.segment_count as usize != writes.len()
            || result.payload_len != 0
            || result.sync_epoch != expected_lease_hwm
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "remote WAL write result mismatch op={} status={} branch={} records={} payload_len={}",
                    result.op,
                    result.status,
                    result.branch_id,
                    result.segment_count,
                    result.payload_len
                ),
            ));
        }
        self.write_batches += 1;
        self.write_records += writes.len() as u64;
        self.write_bytes += payload_len as u64;
        Ok(())
    }

    fn read_into(&mut self, request: ZcnblkShmRequest, out: &mut [u8]) -> io::Result<()> {
        let pending = PendingWalWrite {
            request,
            io_contract: ZcnblkWalIoContract::default(),
            request_sequence: 0,
            submit_sequence: request.submit_sequence,
            offset: request.offset,
            len: out.len(),
            payload_offset: 0,
        };
        let frame = self.request_frame(&pending, ZCNBLK_FAN_WAL_OP_READ_DESC, false)?;
        zcnblk_fan_wal_write_frame(&mut self.stream, frame, &[])?;
        let result = self.read_result_frame()?;
        if result.op != ZCNBLK_FAN_WAL_OP_RESULT
            || result.status != ZCNBLK_FAN_WAL_STATUS_OK
            || result.sequence != request.submit_sequence
            || result.payload_len as usize != out.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "remote WAL read result mismatch op={} status={} sequence={} payload_len={}",
                    result.op, result.status, result.sequence, result.payload_len
                ),
            ));
        }
        self.recv_result_exact(out)?;
        self.read_records += 1;
        self.read_bytes += out.len() as u64;
        Ok(())
    }

    fn read_batch_into(
        &mut self,
        tx: &mut RemoteWalTxContext,
        mapping: &Mapping,
        reads: &[PendingRemoteRead],
    ) -> io::Result<()> {
        if reads
            .iter()
            .any(|read| read.request.op != ZCNBLK_SHM_OP_READ)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote WAL read batch contains a non-read request",
            ));
        }
        self.request_batch_into(tx, mapping, reads)
    }

    fn request_batch_into(
        &mut self,
        tx: &mut RemoteWalTxContext,
        mapping: &Mapping,
        requests: &[PendingRemoteRead],
    ) -> io::Result<()> {
        self.send_request_batch(tx, mapping, requests)?;
        self.recv_request_batch_into(tx, mapping, requests)
    }

    fn request_batch_lengths(requests: &[PendingRemoteRead]) -> io::Result<(usize, usize, usize)> {
        if requests.is_empty() {
            return Ok((0, 0, 0));
        }
        let descriptor_len = requests
            .len()
            .checked_mul(ZCNBLK_FAN_WAL_HEADER_LEN)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request batch overflow"))?;
        let read_payload_len = requests.iter().try_fold(0usize, |total, request| {
            if request.request.op == ZCNBLK_SHM_OP_READ {
                total
                    .checked_add(request.request.len as usize)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "read payload overflow")
                    })
            } else {
                Ok(total)
            }
        })?;
        let write_payload_len = requests.iter().try_fold(0usize, |total, request| {
            if request.request.op == ZCNBLK_SHM_OP_WRITE {
                total
                    .checked_add(request.request.len as usize)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "write payload overflow")
                    })
            } else {
                Ok(total)
            }
        })?;
        Ok((descriptor_len, read_payload_len, write_payload_len))
    }

    fn send_compact_write_batch(
        &mut self,
        tx: &mut RemoteWalTxContext,
        mapping: &Mapping,
        requests: &[PendingRemoteRead],
    ) -> io::Result<bool> {
        let payload_plan = shared_payload_plan(requests.iter().map(|request| {
            (
                request.payload_offset,
                request.request.offset,
                request.request.len as usize,
            )
        }))?;
        let (descriptors, write_payload_len, original_descriptor_len) =
            compact_write_batch_descriptors(self.lane_id, requests)?;
        let descriptor_len = descriptors.len();
        let batch = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_WRITE_EXTENT_BATCH,
            flags: ZCNBLK_FAN_WAL_FLAG_DIRECT_MEMORY_WRITE_LAYOUT,
            lane_id: self.lane_id,
            lane_count: self.lane_count,
            branch_id: 0,
            branch_count: 1,
            segment_index: u32::try_from(requests.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "compact WAL extent count exceeds u32",
                )
            })?,
            segment_count: u32::try_from(requests.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "compact WAL record count exceeds u32",
                )
            })?,
            request_id: requests[0].request.request_id,
            payload_len: u32::try_from(descriptor_len.checked_add(write_payload_len).ok_or_else(
                || {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "compact request payload overflow",
                    )
                },
            )?)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "compact request payload exceeds u32",
                )
            })?,
            logical_len: u32::try_from(write_payload_len).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "compact logical payload exceeds u32",
                )
            })?,
            logical_offset: requests[0].request.offset,
            leaf_offset: requests[0].request.offset,
            zcnblk_op: ZCNBLK_OP_WRITE,
            ..ZcnblkFanWalFrame::default()
        };
        let used_rma =
            self.send_batch_payload(tx, mapping, batch, descriptors, &payload_plan, true)?;
        self.write_payload_iovecs = self
            .write_payload_iovecs
            .saturating_add(payload_plan.source_iovecs);
        self.write_payload_tx_iovecs = self
            .write_payload_tx_iovecs
            .saturating_add(payload_plan.runs.len() as u64);
        self.write_payload_runs = self
            .write_payload_runs
            .saturating_add(payload_plan.runs.len() as u64);
        self.max_write_payload_run_bytes = self
            .max_write_payload_run_bytes
            .max(payload_plan.max_run_bytes);
        self.compact_write_batches = self.compact_write_batches.saturating_add(1);
        self.request_descriptor_bytes = self
            .request_descriptor_bytes
            .saturating_add(original_descriptor_len as u64);
        self.wire_descriptor_bytes = self
            .wire_descriptor_bytes
            .saturating_add(descriptor_len as u64);
        Ok(used_rma)
    }

    fn send_request_batch(
        &mut self,
        tx: &mut RemoteWalTxContext,
        mapping: &Mapping,
        requests: &[PendingRemoteRead],
    ) -> io::Result<()> {
        if requests.is_empty() {
            return Ok(());
        }
        let started = Instant::now();
        let result = self.send_request_batch_inner(tx, mapping, requests);
        self.request_send_calls = self.request_send_calls.saturating_add(1);
        self.request_send_time = self.request_send_time.saturating_add(started.elapsed());
        let used_rma = result?;
        self.request_batches_pending = self
            .request_batches_pending
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request batch overflow"))?;
        if used_rma {
            if self.rma_write_doorbell_pending {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "multiple OFI RMA write doorbells became pending on one lane",
                ));
            }
            self.rma_write_doorbell_pending = true;
        }
        Ok(())
    }

    fn send_request_batch_inner(
        &mut self,
        tx: &mut RemoteWalTxContext,
        mapping: &Mapping,
        requests: &[PendingRemoteRead],
    ) -> io::Result<bool> {
        if requests.is_empty() {
            return Ok(false);
        }
        if self.compact_writes
            && requests
                .iter()
                .all(|request| request.request.op == ZCNBLK_SHM_OP_WRITE)
        {
            return self.send_compact_write_batch(tx, mapping, requests);
        }
        let (descriptor_len, _read_payload_len, write_payload_len) =
            Self::request_batch_lengths(requests)?;
        let payload_plan = shared_payload_plan(
            requests
                .iter()
                .filter(|request| request.request.op == ZCNBLK_SHM_OP_WRITE)
                .map(|request| {
                    (
                        request.payload_offset,
                        request.request.offset,
                        request.request.len as usize,
                    )
                }),
        )?;
        let mut descriptors = Vec::with_capacity(descriptor_len);
        for request in requests {
            let op = match request.request.op {
                ZCNBLK_SHM_OP_READ => ZCNBLK_FAN_WAL_OP_READ_DESC,
                ZCNBLK_SHM_OP_WRITE => ZCNBLK_FAN_WAL_OP_WRITE_DESC,
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("remote WAL request batch has unsupported op {other}"),
                    ));
                }
            };
            let pending = PendingWalWrite {
                request: request.request,
                io_contract: request.io_contract,
                request_sequence: request.request_sequence,
                submit_sequence: request.request.submit_sequence,
                offset: request.request.offset,
                len: request.request.len as usize,
                payload_offset: request.payload_offset,
            };
            descriptors.extend_from_slice(&self.request_frame(&pending, op, true)?.encode());
        }
        let rma_read_results = self.rma_read_window.is_some()
            && requests
                .iter()
                .any(|request| request.request.op == ZCNBLK_SHM_OP_READ);
        let batch = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_REQUEST_BATCH,
            flags: if rma_read_results
                || requests
                    .iter()
                    .any(|request| request.request.op == ZCNBLK_SHM_OP_WRITE)
            {
                ZCNBLK_FAN_WAL_FLAG_DIRECT_MEMORY_WRITE_LAYOUT
            } else {
                0
            } | if rma_read_results {
                ZCNBLK_FAN_WAL_FLAG_OFI_RMA_READ_RESULT
            } else {
                0
            },
            lane_id: self.lane_id,
            lane_count: self.lane_count,
            branch_id: 0,
            branch_count: 1,
            segment_count: u32::try_from(requests.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "request batch count exceeds u32",
                )
            })?,
            request_id: requests[0].request.request_id,
            payload_len: u32::try_from(descriptor_len.checked_add(write_payload_len).ok_or_else(
                || io::Error::new(io::ErrorKind::InvalidData, "request payload overflow"),
            )?)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "request batch payload exceeds u32",
                )
            })?,
            ..ZcnblkFanWalFrame::default()
        };
        // The leaf's direct-memory request-batch decoder already accepts
        // mixed read/write descriptors with preplaced write payload. Reads
        // still return through the result message for this framed mixed-batch
        // form; ordered all-read spans take the one-sided read queue instead.
        let rma_eligible = requests
            .iter()
            .any(|request| request.request.op == ZCNBLK_SHM_OP_WRITE);
        let used_rma =
            self.send_batch_payload(tx, mapping, batch, descriptors, &payload_plan, rma_eligible)?;
        self.write_payload_iovecs = self
            .write_payload_iovecs
            .saturating_add(payload_plan.source_iovecs);
        self.write_payload_tx_iovecs = self
            .write_payload_tx_iovecs
            .saturating_add(payload_plan.runs.len() as u64);
        self.write_payload_runs = self
            .write_payload_runs
            .saturating_add(payload_plan.runs.len() as u64);
        self.max_write_payload_run_bytes = self
            .max_write_payload_run_bytes
            .max(payload_plan.max_run_bytes);
        self.request_descriptor_bytes = self
            .request_descriptor_bytes
            .saturating_add(descriptor_len as u64);
        self.wire_descriptor_bytes = self
            .wire_descriptor_bytes
            .saturating_add(descriptor_len as u64);
        Ok(used_rma)
    }

    fn recv_request_batch_into(
        &mut self,
        tx: &mut RemoteWalTxContext,
        mapping: &Mapping,
        requests: &[PendingRemoteRead],
    ) -> io::Result<()> {
        if requests.is_empty() {
            return Ok(());
        }
        let started = Instant::now();
        let result = self.recv_request_batch_into_inner(tx, mapping, requests);
        self.result_recv_calls = self.result_recv_calls.saturating_add(1);
        self.result_recv_time = self.result_recv_time.saturating_add(started.elapsed());
        result
    }

    fn recv_request_batch_into_inner(
        &mut self,
        tx: &mut RemoteWalTxContext,
        mapping: &Mapping,
        requests: &[PendingRemoteRead],
    ) -> io::Result<()> {
        if requests.is_empty() {
            return Ok(());
        }
        let (descriptor_len, read_payload_len, write_payload_len) =
            Self::request_batch_lengths(requests)?;
        let rma_read_results = self.rma_read_window.is_some()
            && requests
                .iter()
                .any(|request| request.request.op == ZCNBLK_SHM_OP_READ);
        let expected_lease_hwm = requests
            .iter()
            .filter(|request| request.io_contract.registered_lease)
            .map(|request| request.io_contract.lease_id)
            .max()
            .unwrap_or(0);
        let header_started = Instant::now();
        let result_batch = self.read_result_frame()?;
        self.result_header_time = self
            .result_header_time
            .saturating_add(header_started.elapsed());
        if self.quickack {
            self.stream.set_quickack()?;
        }
        self.finish_next_send_batch(tx)?;
        if read_payload_len == 0 {
            if result_batch.op != ZCNBLK_FAN_WAL_OP_RESULT_RANGE_BATCH
                || result_batch.status != ZCNBLK_FAN_WAL_STATUS_OK
                || result_batch.branch_id != 0
                || result_batch.segment_count as usize != requests.len()
                || result_batch.request_id != requests[0].request.request_id
                || result_batch.payload_len != 0
                || result_batch.sync_epoch != expected_lease_hwm
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "remote WAL write range mismatch op={} status={} branch={} records={} request_id={} payload_len={}",
                        result_batch.op,
                        result_batch.status,
                        result_batch.branch_id,
                        result_batch.segment_count,
                        result_batch.request_id,
                        result_batch.payload_len,
                    ),
                ));
            }
            self.write_batches += 1;
            self.write_records += requests.len() as u64;
            self.write_bytes += write_payload_len as u64;
            self.finish_request_batch_tracking()?;
            return Ok(());
        }
        let expected_result_len = if rma_read_results {
            descriptor_len
        } else {
            descriptor_len
                .checked_add(read_payload_len)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "read result batch overflow")
                })?
        };
        if result_batch.op != ZCNBLK_FAN_WAL_OP_RESULT_BATCH
            || result_batch.status != ZCNBLK_FAN_WAL_STATUS_OK
            || result_batch.branch_id != 0
            || result_batch.segment_count as usize != requests.len()
            || result_batch.request_id != requests[0].request.request_id
            || result_batch.payload_len as usize != expected_result_len
            || result_batch.sync_epoch != expected_lease_hwm
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "remote WAL read batch mismatch op={} status={} branch={} records={} request_id={} payload_len={} expected_records={} expected_request_id={} expected_payload_len={}",
                    result_batch.op,
                    result_batch.status,
                    result_batch.branch_id,
                    result_batch.segment_count,
                    result_batch.request_id,
                    result_batch.payload_len,
                    requests.len(),
                    requests[0].request.request_id,
                    expected_result_len,
                ),
            ));
        }
        let mut result_descriptors = vec![0u8; descriptor_len];
        let descriptor_started = Instant::now();
        self.recv_result_exact(&mut result_descriptors)?;
        self.result_descriptor_time = self
            .result_descriptor_time
            .saturating_add(descriptor_started.elapsed());
        for (idx, request) in requests.iter().enumerate() {
            let start = idx * ZCNBLK_FAN_WAL_HEADER_LEN;
            let result = zcnblk_fan_wal_decode_frame_slice(
                &result_descriptors[start..start + ZCNBLK_FAN_WAL_HEADER_LEN],
            )?;
            if result.op != ZCNBLK_FAN_WAL_OP_RESULT
                || result.status != ZCNBLK_FAN_WAL_STATUS_OK
                || result.sequence != request.request.submit_sequence
                || result.request_id != request.request.request_id
                || result.logical_offset != request.request.offset
                || result.leaf_offset != request.request.offset
                || result.sync_epoch != request.io_contract.lease_id
                || result.payload_len
                    != if request.request.op == ZCNBLK_SHM_OP_READ && !rma_read_results {
                        request.request.len
                    } else {
                        0
                    }
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "remote WAL read result mismatch idx={idx} op={} status={} sequence={} request_id={} offset={} leaf_offset={} payload_len={}",
                        result.op,
                        result.status,
                        result.sequence,
                        result.request_id,
                        result.logical_offset,
                        result.leaf_offset,
                        result.payload_len,
                    ),
                ));
            }
        }
        let payload_started = Instant::now();
        if rma_read_results {
            let reads = requests
                .iter()
                .copied()
                .filter(|request| request.request.op == ZCNBLK_SHM_OP_READ)
                .collect::<Vec<_>>();
            let batch_id = self.try_submit_rma_read_batch(&reads)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "RMA read-result batch lost its negotiated read window",
                )
            })?;
            self.drain_rma_reads(mapping)?;
            if !self.rma_read_batch_complete(batch_id)? {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("OFI RMA read-result batch id={batch_id} did not complete"),
                ));
            }
            self.finish_rma_read_batch(batch_id)?;
        } else {
            for request in requests {
                if request.request.op == ZCNBLK_SHM_OP_READ {
                    let out = unsafe {
                        std::slice::from_raw_parts_mut(
                            mapping.ptr.add(request.payload_offset),
                            request.request.len as usize,
                        )
                    };
                    self.recv_result_exact(out)?;
                }
            }
        }
        self.result_payload_time = self
            .result_payload_time
            .saturating_add(payload_started.elapsed());
        if self.quickack {
            self.stream.set_quickack()?;
        }
        let read_records = requests
            .iter()
            .filter(|request| request.request.op == ZCNBLK_SHM_OP_READ)
            .count() as u64;
        let write_records = requests.len() as u64 - read_records;
        if read_records != 0 && !rma_read_results {
            self.read_batches += 1;
            self.read_records += read_records;
            self.read_bytes += read_payload_len as u64;
        }
        if write_records != 0 {
            self.write_batches += 1;
            self.write_records += write_records;
            self.write_bytes += write_payload_len as u64;
        }
        self.finish_request_batch_tracking()?;
        Ok(())
    }

    fn sync(&mut self, submit_sequence: u64) -> io::Result<()> {
        if self.request_batches_pending != 0 || self.rma_write_doorbell_pending {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "remote WAL sync cannot pass {} unacknowledged request batch(es)",
                    self.request_batches_pending
                ),
            ));
        }
        let started = Instant::now();
        let frame = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_SYNC,
            lane_id: self.lane_id,
            lane_count: self.lane_count,
            branch_id: 0,
            branch_count: 1,
            sequence: submit_sequence,
            sync_epoch: submit_sequence,
            ..ZcnblkFanWalFrame::default()
        };
        zcnblk_fan_wal_write_frame(&mut self.stream, frame, &[])?;
        let result = self.read_result_frame()?;
        if result.op != ZCNBLK_FAN_WAL_OP_RESULT
            || result.status != ZCNBLK_FAN_WAL_STATUS_OK
            || result.sequence != submit_sequence
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote WAL sync result mismatch",
            ));
        }
        self.syncs += 1;
        self.sync_time = self.sync_time.saturating_add(started.elapsed());
        Ok(())
    }

    fn eof(&mut self) -> io::Result<()> {
        zcnblk_fan_wal_write_frame(
            &mut self.stream,
            ZcnblkFanWalFrame {
                op: ZCNBLK_FAN_WAL_OP_EOF,
                lane_id: self.lane_id,
                lane_count: self.lane_count,
                branch_id: 0,
                branch_count: 1,
                ..ZcnblkFanWalFrame::default()
            },
            &[],
        )
    }
}

fn resolve_remote_wal_leaf_address(address: &str) -> io::Result<SocketAddr> {
    address
        .to_socket_addrs()
        .map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid remote WAL leaf address {address:?}: {err}"),
            )
        })?
        .next()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("remote WAL leaf address {address:?} resolved to no addresses"),
            )
        })
}

fn connect_remote_wal_tcp(
    socket_address: SocketAddr,
    source_ip: Option<IpAddr>,
) -> io::Result<TcpStream> {
    let retry_ms = env::var("URING_PLAY_ZCNBLK_SHM_REMOTE_CONNECT_RETRY_MS")
        .ok()
        .map(|value| {
            value.parse::<u64>().map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "invalid URING_PLAY_ZCNBLK_SHM_REMOTE_CONNECT_RETRY_MS={value:?}: {err}"
                    ),
                )
            })
        })
        .transpose()?
        .unwrap_or(0);
    let started = Instant::now();
    let retry_for = Duration::from_millis(retry_ms);
    let mut attempts = 0_u64;
    loop {
        attempts += 1;
        let result = match source_ip {
            Some(source_ip) => connect_tcp_bound_local_ip(socket_address, source_ip),
            None => TcpStream::connect(socket_address),
        };
        match result {
            Ok(stream) => {
                if attempts > 1 {
                    eprintln!(
                        "zcnblk-shm-target-remote-connect-retry: address={socket_address} attempts={attempts} elapsed_ms={} status=connected",
                        started.elapsed().as_millis()
                    );
                }
                return Ok(stream);
            }
            Err(error) if started.elapsed() < retry_for => {
                if attempts == 1 || attempts.is_multiple_of(50) {
                    eprintln!(
                        "zcnblk-shm-target-remote-connect-retry: address={socket_address} attempts={attempts} elapsed_ms={} status=waiting error={error}",
                        started.elapsed().as_millis()
                    );
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error),
        }
    }
}

enum RemoteWalCommand {
    WriteWindow(Vec<Vec<PendingWalWrite>>),
    Read(Vec<PendingRemoteRead>),
    Sync(u64),
    Eof,
}

struct RemoteWalWorker {
    lane_id: u32,
    lane_count: u32,
    address: String,
    target_cpu: Option<usize>,
    wait_spins: usize,
    pipeline_batches: usize,
    command_tx: SyncSender<RemoteWalCommand>,
    result_rx: Mutex<Receiver<io::Result<()>>>,
    handle: Option<thread::JoinHandle<io::Result<RemoteWalLeaf>>>,
}

impl RemoteWalWorker {
    fn start(mut remote: RemoteWalLeaf, mapping: Arc<Mapping>) -> io::Result<Self> {
        remote.attach_mapping(Arc::clone(&mapping))?;
        let lane_id = remote.lane_id;
        let lane_count = remote.lane_count;
        let address = remote.address.clone();
        let target_cpu = remote.target_cpu;
        let wait_spins = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_SPINS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(
                if env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_OWNER_DISPATCH", false) {
                    4_096
                } else {
                    0
                },
            );
        let pipeline_batches = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_BATCHES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(16)
            .max(1);
        let (command_tx, command_rx) = sync_channel::<RemoteWalCommand>(1);
        let (result_tx, result_rx) = sync_channel::<io::Result<()>>(1);
        let (startup_tx, startup_rx) = sync_channel::<io::Result<()>>(1);
        let handle = thread::Builder::new()
            .name(format!("zcwal-lane-{lane_id}"))
            .spawn(move || {
                if let Some(cpu) = target_cpu {
                    if let Err(err) = pin_current_thread(cpu) {
                        let reported = io::Error::new(err.kind(), err.to_string());
                        let _ = startup_tx.send(Err(reported));
                        return Err(err);
                    }
                }
                let mut tx = match RemoteWalTxContext::new(&remote) {
                    Ok(tx) => tx,
                    Err(err) => {
                        let reported = io::Error::new(err.kind(), err.to_string());
                        let _ = startup_tx.send(Err(reported));
                        return Err(err);
                    }
                };
                startup_tx.send(Ok(())).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "remote WAL startup receiver closed",
                    )
                })?;
                while let Some(command) = recv_remote_worker_item(&command_rx, wait_spins) {
                    let eof = matches!(command, RemoteWalCommand::Eof);
                    let result = match command {
                        RemoteWalCommand::WriteWindow(batches) => {
                            let mut result = Ok(());
                            for window in batches.chunks(pipeline_batches) {
                                let request_batches = window
                                    .iter()
                                    .map(|writes| {
                                        writes
                                            .iter()
                                            .map(|write| PendingRemoteRead {
                                                request: write.request,
                                                io_contract: write.io_contract,
                                                request_sequence: write.request_sequence,
                                                payload_offset: write.payload_offset,
                                                dirty_ref: None,
                                            })
                                            .collect::<Vec<_>>()
                                    })
                                    .collect::<Vec<_>>();
                                for requests in &request_batches {
                                    if let Err(err) = remote.send_request_batch(
                                        &mut tx,
                                        mapping.as_ref(),
                                        requests,
                                    ) {
                                        result = Err(err);
                                        break;
                                    }
                                }
                                if result.is_err() {
                                    break;
                                }
                                for requests in &request_batches {
                                    if let Err(err) = remote.recv_request_batch_into(
                                        &mut tx,
                                        mapping.as_ref(),
                                        requests,
                                    ) {
                                        result = Err(err);
                                        break;
                                    }
                                }
                                if result.is_err() {
                                    break;
                                }
                            }
                            result
                        }
                        RemoteWalCommand::Read(reads) => {
                            remote.read_batch_into(&mut tx, mapping.as_ref(), &reads)
                        }
                        RemoteWalCommand::Sync(sequence) => remote.sync(sequence),
                        RemoteWalCommand::Eof => tx.ensure_idle().and_then(|()| remote.eof()),
                    };
                    match result {
                        Ok(()) => result_tx.send(Ok(())).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "remote WAL result receiver closed",
                            )
                        })?,
                        Err(err) => {
                            let reported = io::Error::new(err.kind(), err.to_string());
                            let _ = result_tx.send(Err(reported));
                            return Err(err);
                        }
                    }
                    if eof {
                        return Ok(remote);
                    }
                }
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "remote WAL command sender closed",
                ))
            })?;
        match startup_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let _ = handle.join();
                return Err(err);
            }
            Err(_) => {
                let join_error = match handle.join() {
                    Ok(Err(err)) => err,
                    Ok(Ok(_)) => io::Error::other(
                        "remote WAL worker exited successfully before startup acknowledgement",
                    ),
                    Err(_) => io::Error::other("remote WAL worker panicked during startup"),
                };
                return Err(join_error);
            }
        }
        Ok(Self {
            lane_id,
            lane_count,
            address,
            target_cpu,
            wait_spins,
            pipeline_batches,
            command_tx,
            result_rx: Mutex::new(result_rx),
            handle: Some(handle),
        })
    }

    fn send(&self, command: RemoteWalCommand) -> io::Result<()> {
        self.command_tx.send(command).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("remote WAL lane {} command worker exited", self.lane_id),
            )
        })
    }

    fn wait(&self) -> io::Result<()> {
        let result_rx = self
            .result_rx
            .lock()
            .map_err(|_| io::Error::other("remote WAL result lock poisoned"))?;
        recv_remote_worker_item(&result_rx, self.wait_spins).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("remote WAL lane {} result worker exited", self.lane_id),
            )
        })?
    }
}

fn recv_remote_worker_item<T>(receiver: &Receiver<T>, spins: usize) -> Option<T> {
    for _ in 0..spins {
        match receiver.try_recv() {
            Ok(value) => return Some(value),
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) => std::hint::spin_loop(),
        }
    }
    receiver.recv().ok()
}

#[repr(align(64))]
struct WalSpscCursor(AtomicUsize);

struct WalSpscSlot<T> {
    sequence: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

struct WalSpscRing<T> {
    slots: Box<[WalSpscSlot<T>]>,
    mask: usize,
    capacity: usize,
    head: WalSpscCursor,
    tail: WalSpscCursor,
}

unsafe impl<T: Send> Send for WalSpscRing<T> {}
unsafe impl<T: Send> Sync for WalSpscRing<T> {}

impl<T> WalSpscRing<T> {
    fn new(capacity: usize) -> io::Result<Self> {
        let capacity = capacity
            .max(2)
            .checked_next_power_of_two()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "SPSC ring overflow"))?;
        Ok(Self {
            slots: (0..capacity)
                .map(|sequence| WalSpscSlot {
                    sequence: AtomicUsize::new(sequence),
                    value: UnsafeCell::new(MaybeUninit::uninit()),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            mask: capacity - 1,
            capacity,
            head: WalSpscCursor(AtomicUsize::new(0)),
            tail: WalSpscCursor(AtomicUsize::new(0)),
        })
    }

    fn try_push(&self, value: T) -> Result<(), T> {
        let position = self.head.0.load(Ordering::Relaxed);
        let slot = &self.slots[position & self.mask];
        if slot.sequence.load(Ordering::Acquire) != position {
            return Err(value);
        }
        unsafe {
            (*slot.value.get()).write(value);
        }
        slot.sequence.store(position + 1, Ordering::Release);
        self.head.0.store(position + 1, Ordering::Relaxed);
        Ok(())
    }

    fn try_pop(&self) -> Option<T> {
        let position = self.tail.0.load(Ordering::Relaxed);
        let slot = &self.slots[position & self.mask];
        if slot.sequence.load(Ordering::Acquire) != position + 1 {
            return None;
        }
        let value = unsafe { (*slot.value.get()).assume_init_read() };
        slot.sequence
            .store(position + self.capacity, Ordering::Release);
        self.tail.0.store(position + 1, Ordering::Relaxed);
        Some(value)
    }

    fn push_wait(&self, mut value: T, peer: &Thread) {
        let mut spins = 0u32;
        loop {
            match self.try_push(value) {
                Ok(()) => {
                    peer.unpark();
                    return;
                }
                Err(returned) => value = returned,
            }
            std::hint::spin_loop();
            spins = spins.wrapping_add(1);
            if spins & 4_095 == 0 {
                thread::yield_now();
            }
        }
    }

    fn pop_wait(&self, greedy: bool) -> T {
        let mut spins = 0u32;
        loop {
            if let Some(value) = self.try_pop() {
                return value;
            }
            std::hint::spin_loop();
            spins = spins.wrapping_add(1);
            if !greedy && spins & 4_095 == 0 {
                thread::park_timeout(Duration::from_micros(50));
            }
        }
    }
}

impl<T> Drop for WalSpscRing<T> {
    fn drop(&mut self) {
        while let Some(value) = self.try_pop() {
            drop(value);
        }
    }
}

enum WalLaneTransportCommand {
    Batch(Vec<PendingRemoteRead>),
    Sync(u64),
    Eof,
}

enum WalLaneTransportResult {
    Batch(Vec<PendingRemoteRead>),
    Sync(u64),
    Failed(io::Error),
}

#[derive(Clone, Copy)]
enum WalLanePendingBatchKind {
    Framed,
    Rma(u64),
}

struct WalLanePendingBatch {
    requests: Vec<PendingRemoteRead>,
    kind: WalLanePendingBatchKind,
}

fn wal_lane_has_framed_batch(pending: &VecDeque<WalLanePendingBatch>) -> bool {
    pending
        .iter()
        .any(|batch| matches!(batch.kind, WalLanePendingBatchKind::Framed))
}

struct WalLaneTransportWorker {
    lane_id: u32,
    commands: Arc<WalSpscRing<WalLaneTransportCommand>>,
    results: Arc<WalSpscRing<WalLaneTransportResult>>,
    worker_thread: Thread,
    greedy: bool,
    handle: Option<thread::JoinHandle<io::Result<RemoteWalLeaf>>>,
}

impl WalLaneTransportWorker {
    fn complete_pending(
        remote: &mut RemoteWalLeaf,
        tx: &mut RemoteWalTxContext,
        mapping: &Mapping,
        pending: WalLanePendingBatch,
    ) -> io::Result<Vec<PendingRemoteRead>> {
        match pending.kind {
            WalLanePendingBatchKind::Framed => {
                remote.recv_request_batch_into(tx, mapping, &pending.requests)?;
            }
            WalLanePendingBatchKind::Rma(batch_id) => {
                remote.drain_rma_reads(mapping)?;
                if !remote.rma_read_batch_complete(batch_id)? {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!("OFI RMA batch id={batch_id} did not complete while draining"),
                    ));
                }
                remote.finish_rma_read_batch(batch_id)?;
            }
        }
        Ok(pending.requests)
    }

    fn start(
        mut remote: RemoteWalLeaf,
        mapping: Arc<Mapping>,
        cpu: usize,
        queue_depth: usize,
    ) -> io::Result<Self> {
        remote.attach_mapping(Arc::clone(&mapping))?;
        remote.target_cpu = Some(cpu);
        let lane_id = remote.lane_id;
        let serialize_framed_batches = remote.stream.transport_label() == "ofi";
        let commands = Arc::new(WalSpscRing::new(queue_depth + 1)?);
        let results = Arc::new(WalSpscRing::new(queue_depth + 1)?);
        let worker_commands = Arc::clone(&commands);
        let worker_results = Arc::clone(&results);
        let foreground_thread = thread::current();
        let greedy = env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_TRANSPORT_GREEDY", false);
        let worker_greedy = greedy;
        let (startup_tx, startup_rx) = sync_channel::<io::Result<()>>(1);
        let handle = thread::Builder::new()
            .name(format!("zcwal-tx-{lane_id}"))
            .spawn(move || {
                if let Err(error) = pin_current_thread(cpu) {
                    let reported = io::Error::new(error.kind(), error.to_string());
                    let _ = startup_tx.send(Err(reported));
                    return Err(error);
                }
                let mut tx = match RemoteWalTxContext::new(&remote) {
                    Ok(tx) => tx,
                    Err(error) => {
                        let reported = io::Error::new(error.kind(), error.to_string());
                        let _ = startup_tx.send(Err(reported));
                        return Err(error);
                    }
                };
                startup_tx.send(Ok(())).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "WAL lane transport startup receiver closed",
                    )
                })?;
                let mut pending_batches =
                    VecDeque::<WalLanePendingBatch>::with_capacity(queue_depth.max(1));
                let publish_error = |error: &io::Error| {
                    worker_results.push_wait(
                        WalLaneTransportResult::Failed(io::Error::new(
                            error.kind(),
                            error.to_string(),
                        )),
                        &foreground_thread,
                    );
                };
                loop {
                    // EFA RDM messages above the eager threshold may not
                    // complete until the peer posts its receive. Once a
                    // framed request is outstanding, receive its result before
                    // posting another request; otherwise both peers can block
                    // in a rendezvous send. One-sided RMA read batches remain
                    // free to use the configured multi-batch window.
                    let framed_in_flight =
                        serialize_framed_batches && wal_lane_has_framed_batch(&pending_batches);
                    let command = if !framed_in_flight && pending_batches.len() < queue_depth.max(1)
                    {
                        if pending_batches.is_empty() {
                            Some(worker_commands.pop_wait(worker_greedy))
                        } else {
                            worker_commands.try_pop()
                        }
                    } else {
                        None
                    };
                    match command {
                        Some(WalLaneTransportCommand::Batch(batch)) => {
                            let rma_batch_id = if framed_in_flight {
                                None
                            } else {
                                match remote.try_submit_rma_read_batch(&batch) {
                                    Ok(batch_id) => batch_id,
                                    Err(error) => {
                                        publish_error(&error);
                                        return Err(error);
                                    }
                                }
                            };
                            let kind = if let Some(batch_id) = rma_batch_id {
                                WalLanePendingBatchKind::Rma(batch_id)
                            } else {
                                if let Err(error) =
                                    remote.drain_rma_reads(mapping.as_ref()).and_then(|()| {
                                        remote.send_request_batch(&mut tx, mapping.as_ref(), &batch)
                                    })
                                {
                                    publish_error(&error);
                                    return Err(error);
                                }
                                WalLanePendingBatchKind::Framed
                            };
                            pending_batches.push_back(WalLanePendingBatch {
                                requests: batch,
                                kind,
                            });
                            continue;
                        }
                        Some(WalLaneTransportCommand::Sync(sequence)) => {
                            while let Some(pending) = pending_batches.pop_front() {
                                let batch = match Self::complete_pending(
                                    &mut remote,
                                    &mut tx,
                                    mapping.as_ref(),
                                    pending,
                                ) {
                                    Ok(batch) => batch,
                                    Err(error) => {
                                        publish_error(&error);
                                        return Err(error);
                                    }
                                };
                                worker_results.push_wait(
                                    WalLaneTransportResult::Batch(batch),
                                    &foreground_thread,
                                );
                            }
                            if let Err(error) = remote.sync(sequence) {
                                publish_error(&error);
                                return Err(error);
                            }
                            worker_results.push_wait(
                                WalLaneTransportResult::Sync(sequence),
                                &foreground_thread,
                            );
                        }
                        Some(WalLaneTransportCommand::Eof) => {
                            while let Some(pending) = pending_batches.pop_front() {
                                let batch = match Self::complete_pending(
                                    &mut remote,
                                    &mut tx,
                                    mapping.as_ref(),
                                    pending,
                                ) {
                                    Ok(batch) => batch,
                                    Err(error) => {
                                        publish_error(&error);
                                        return Err(error);
                                    }
                                };
                                worker_results.push_wait(
                                    WalLaneTransportResult::Batch(batch),
                                    &foreground_thread,
                                );
                            }
                            if let Err(error) = tx.ensure_idle().and_then(|()| remote.eof()) {
                                publish_error(&error);
                                return Err(error);
                            }
                            return Ok(remote);
                        }
                        None => {
                            let pending = pending_batches
                                .pop_front()
                                .expect("full or command-idle transport has a pending batch");
                            let batch = match Self::complete_pending(
                                &mut remote,
                                &mut tx,
                                mapping.as_ref(),
                                pending,
                            ) {
                                Ok(batch) => batch,
                                Err(error) => {
                                    publish_error(&error);
                                    return Err(error);
                                }
                            };
                            worker_results.push_wait(
                                WalLaneTransportResult::Batch(batch),
                                &foreground_thread,
                            );
                        }
                    }
                }
            })?;
        let worker_thread = handle.thread().clone();
        match startup_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = handle.join();
                return Err(error);
            }
            Err(_) => {
                let error = match handle.join() {
                    Ok(Err(error)) => error,
                    Ok(Ok(_)) => {
                        io::Error::other("WAL lane transport exited before startup acknowledgement")
                    }
                    Err(_) => io::Error::other("WAL lane transport panicked during startup"),
                };
                return Err(error);
            }
        }
        Ok(Self {
            lane_id,
            commands,
            results,
            worker_thread,
            greedy,
            handle: Some(handle),
        })
    }

    fn send(&self, command: WalLaneTransportCommand) -> io::Result<()> {
        self.commands.push_wait(command, &self.worker_thread);
        Ok(())
    }

    fn recv(&self) -> io::Result<WalLaneTransportResult> {
        Ok(self.results.pop_wait(self.greedy))
    }

    fn try_recv(&self) -> io::Result<Option<WalLaneTransportResult>> {
        Ok(self.results.try_pop())
    }

    fn stop(mut self) -> io::Result<RemoteWalLeaf> {
        self.send(WalLaneTransportCommand::Eof)?;
        let handle = self
            .handle
            .take()
            .ok_or_else(|| io::Error::other("WAL lane transport join handle is missing"))?;
        handle
            .join()
            .map_err(|_| io::Error::other("WAL lane transport worker panicked"))?
    }
}

enum WalOwnerIngressCommand {
    Batch {
        ingress: u32,
        requests: Vec<PendingRemoteRead>,
        immediate: bool,
    },
    Sync {
        ingress: u32,
        sequence: u64,
    },
    Eof,
}

enum WalOwnerIngressResult {
    Batch(Vec<PendingRemoteRead>),
    Sync(u64),
    Failed(io::Error),
}

fn send_sync_channel_spinning<T>(sender: &SyncSender<T>, mut value: T) -> io::Result<()> {
    loop {
        match sender.try_send(value) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => value = returned,
            Err(TrySendError::Disconnected(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "WAL owner queue disconnected",
                ));
            }
        }
        std::hint::spin_loop();
    }
}

fn send_owner_result(
    sender: &Sender<WalOwnerIngressResult>,
    result: WalOwnerIngressResult,
) -> io::Result<()> {
    sender.send(result).map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "WAL owner result queue disconnected",
        )
    })
}

fn recv_channel_spinning<T>(receiver: &Receiver<T>, spins: usize) -> io::Result<T> {
    for _ in 0..spins {
        match receiver.try_recv() {
            Ok(value) => return Ok(value),
            Err(TryRecvError::Empty) => std::hint::spin_loop(),
            Err(TryRecvError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "WAL owner queue disconnected",
                ));
            }
        }
    }
    receiver
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "WAL owner queue disconnected"))
}

struct AdaptiveChannelReceiver {
    enabled: bool,
    min_spins: usize,
    max_spins: usize,
    current_spins: usize,
    quick_wait_ns: u64,
    spin_hits: u64,
    blocking_waits: u64,
    quick_blocking_waits: u64,
}

impl AdaptiveChannelReceiver {
    fn new(enabled: bool, min_spins: usize, max_spins: usize, quick_wait_ns: u64) -> Self {
        let min_spins = min_spins.min(max_spins);
        Self {
            enabled,
            min_spins,
            max_spins,
            current_spins: if enabled { min_spins } else { max_spins },
            quick_wait_ns,
            spin_hits: 0,
            blocking_waits: 0,
            quick_blocking_waits: 0,
        }
    }

    fn grow(&mut self) {
        if self.enabled && self.max_spins != 0 {
            self.current_spins = self
                .current_spins
                .saturating_mul(2)
                .clamp(self.min_spins, self.max_spins);
        }
    }

    fn shrink(&mut self) {
        if self.enabled {
            self.current_spins = (self.current_spins / 2).max(self.min_spins);
        }
    }

    fn recv<T>(&mut self, receiver: &Receiver<T>) -> io::Result<T> {
        for _ in 0..self.current_spins {
            match receiver.try_recv() {
                Ok(value) => {
                    self.spin_hits = self.spin_hits.saturating_add(1);
                    self.grow();
                    return Ok(value);
                }
                Err(TryRecvError::Empty) => std::hint::spin_loop(),
                Err(TryRecvError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "WAL owner queue disconnected",
                    ));
                }
            }
        }

        self.blocking_waits = self.blocking_waits.saturating_add(1);
        let started = Instant::now();
        let value = receiver.recv().map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "WAL owner queue disconnected")
        })?;
        if started.elapsed().as_nanos() <= self.quick_wait_ns as u128 {
            self.quick_blocking_waits = self.quick_blocking_waits.saturating_add(1);
            self.grow();
        } else {
            self.shrink();
        }
        Ok(value)
    }
}

struct WalOwnerIngressWorker {
    command_tx: SyncSender<WalOwnerIngressCommand>,
    handle: Option<thread::JoinHandle<io::Result<RemoteWalLeaf>>>,
}

impl WalOwnerIngressWorker {
    fn mixed_dispatch_is_immediate(
        explicit_immediate: bool,
        requests: &[PendingRemoteRead],
        read_hot_until: &mut Option<Instant>,
        hysteresis: Duration,
    ) -> bool {
        let has_read = requests
            .iter()
            .any(|pending| pending.request.op != ZCNBLK_SHM_OP_WRITE);
        let now = Instant::now();
        if has_read && explicit_immediate {
            *read_hot_until = now.checked_add(hysteresis);
            return true;
        }
        if has_read {
            return false;
        }
        read_hot_until.is_some_and(|deadline| now < deadline)
    }

    fn note_command_dequeued(
        command: &WalOwnerIngressCommand,
        queued_records: &AtomicUsize,
    ) -> io::Result<usize> {
        let WalOwnerIngressCommand::Batch { requests, .. } = command else {
            return Ok(queued_records.load(Ordering::Acquire));
        };
        Self::note_records_dequeued(requests.len(), queued_records)
    }

    fn note_records_dequeued(count: usize, queued_records: &AtomicUsize) -> io::Result<usize> {
        let previous = queued_records.fetch_sub(count, Ordering::AcqRel);
        if previous < count {
            queued_records.fetch_add(count, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WAL owner queued-record counter underflow",
            ));
        }
        Ok(previous - count)
    }

    fn start(
        mut remote: RemoteWalLeaf,
        mapping: Arc<Mapping>,
        cpu: usize,
        result_txs: Arc<[Sender<WalOwnerIngressResult>]>,
        queued_records_by_owner: Arc<[AtomicUsize]>,
        queue_depth: usize,
    ) -> io::Result<Self> {
        remote.attach_mapping(Arc::clone(&mapping))?;
        remote.target_cpu = Some(cpu);
        let owner = remote.lane_id;
        let owner_index = owner as usize;
        if owner_index >= queued_records_by_owner.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WAL owner queue counter is missing",
            ));
        }
        let (command_tx, command_rx) = sync_channel(queue_depth.max(2));
        let (startup_tx, startup_rx) = sync_channel::<io::Result<()>>(1);
        let handle = thread::Builder::new()
            .name(format!("zcwal-owner-{owner}"))
            .spawn(move || {
                let queued_records = &queued_records_by_owner[owner_index];
                if let Err(error) = pin_current_thread(cpu) {
                    let reported = io::Error::new(error.kind(), error.to_string());
                    let _ = startup_tx.send(Err(reported));
                    return Err(error);
                }
                let mut tx = match RemoteWalTxContext::new(&remote) {
                    Ok(tx) => tx,
                    Err(error) => {
                        let reported = io::Error::new(error.kind(), error.to_string());
                        let _ = startup_tx.send(Err(reported));
                        return Err(error);
                    }
                };
                startup_tx.send(Ok(())).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "WAL owner startup receiver closed",
                    )
                })?;
                let batch_records = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_BATCH_RECORDS")
                    .ok()
                    .map(|value| value.parse::<usize>())
                    .transpose()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                    .unwrap_or(2_048)
                    .max(1);
                let fill_records = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_WRITE_FILL_MIN")
                    .ok()
                    .map(|value| value.parse::<usize>())
                    .transpose()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                    .unwrap_or(256)
                    .clamp(1, batch_records);
                let fill_us = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_WRITE_FILL_US")
                    .ok()
                    .map(|value| value.parse::<u64>())
                    .transpose()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                    .unwrap_or(0);
                let debounce_us = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_DEBOUNCE_US")
                    .ok()
                    .map(|value| value.parse::<u64>())
                    .transpose()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                    .unwrap_or(2);
                let backlog_high = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_BACKLOG_HIGH_RECORDS")
                    .ok()
                    .map(|value| value.parse::<usize>())
                    .transpose()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                    .unwrap_or(fill_records)
                    .max(1);
                let backlog_low = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_BACKLOG_LOW_RECORDS")
                    .ok()
                    .map(|value| value.parse::<usize>())
                    .transpose()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                    .unwrap_or(16);
                if backlog_low >= backlog_high {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "WAL owner backlog low watermark must be below the high watermark",
                    ));
                }
                let pipeline_batches = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_BATCHES")
                    .ok()
                    .map(|value| value.parse::<usize>())
                    .transpose()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                    .unwrap_or(16)
                    .max(1);
                let pipeline_refill_spins =
                    env::var("URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_REFILL_SPINS")
                        .ok()
                        .map(|value| value.parse::<usize>())
                        .transpose()
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                        .unwrap_or(256);
                let wait_spins = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_SPINS")
                    .ok()
                    .map(|value| value.parse::<usize>())
                    .transpose()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                    .unwrap_or(65_536);
                let adaptive_wait = env_enabled_or(
                    "URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_ADAPTIVE_SPIN",
                    true,
                );
                let wait_spin_min = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_SPIN_MIN")
                    .ok()
                    .map(|value| value.parse::<usize>())
                    .transpose()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                    .unwrap_or(4_096);
                let quick_wait_ns =
                    env::var("URING_PLAY_ZCNBLK_SHM_OWNER_WORKER_ADAPTIVE_WAIT_NS")
                        .ok()
                        .map(|value| value.parse::<u64>())
                        .transpose()
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                        .unwrap_or(50_000);
                let mixed_hysteresis = Duration::from_micros(
                    env::var("URING_PLAY_ZCNBLK_SHM_OWNER_MIXED_HYSTERESIS_US")
                        .ok()
                        .map(|value| value.parse::<u64>())
                        .transpose()
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                        .unwrap_or(10_000),
                );
                let mut command_wait = AdaptiveChannelReceiver::new(
                    adaptive_wait,
                    wait_spin_min,
                    wait_spins,
                    quick_wait_ns,
                );
                let report_failure = |error: &io::Error| {
                    for result_tx in result_txs.iter() {
                        let _ = result_tx.send(WalOwnerIngressResult::Failed(io::Error::new(
                            error.kind(),
                            error.to_string(),
                        )));
                    }
                };
                let mut carry = None::<WalOwnerIngressCommand>;
                let mut read_hot_until = None::<Instant>;
                let mut bulk_mode = false;
                let mut wire_batches = 0u64;
                let mut wire_records = 0u64;
                let mut immediate_batches = 0u64;
                let mut debounced_batches = 0u64;
                let mut bulk_batches = 0u64;
                let mut max_queued_records = 0usize;
                let mut max_wire_batch_records = 0usize;
                loop {
                    let first = match carry.take() {
                        Some(command) => command,
                        None => match command_wait.recv(&command_rx) {
                            Ok(command) => command,
                            Err(error) => {
                                report_failure(&error);
                                return Err(error);
                            }
                        },
                    };
                    let queued_after_first = match Self::note_command_dequeued(&first, queued_records)
                    {
                        Ok(queued) => queued,
                        Err(error) => {
                            report_failure(&error);
                            return Err(error);
                        }
                    };
                    max_queued_records = max_queued_records.max(queued_after_first);
                    match first {
                        WalOwnerIngressCommand::Batch {
                            ingress,
                            requests,
                            immediate: explicit_immediate,
                        } => {
                            if queued_after_first.saturating_add(requests.len()) >= backlog_high {
                                bulk_mode = true;
                            } else if queued_after_first <= backlog_low {
                                bulk_mode = false;
                            }
                            let mut immediate = !bulk_mode
                                && queued_after_first <= backlog_low
                                && Self::mixed_dispatch_is_immediate(
                                explicit_immediate,
                                &requests,
                                &mut read_hot_until,
                                mixed_hysteresis,
                            );
                            let mut commands =
                                VecDeque::from([(ingress, requests, explicit_immediate)]);
                            let mut records =
                                commands.front().map_or(0, |(_, batch, _)| batch.len());
                            let fill_budget_us = if bulk_mode { fill_us } else { debounce_us };
                            let deadline = Instant::now()
                                .checked_add(Duration::from_micros(fill_budget_us))
                                .unwrap_or_else(Instant::now);
                            while records < fill_records && records < batch_records {
                                match command_rx.try_recv() {
                                    Ok(WalOwnerIngressCommand::Batch {
                                        ingress,
                                        requests,
                                        immediate: next_immediate,
                                    }) => {
                                        let queued = Self::note_records_dequeued(
                                            requests.len(),
                                            queued_records,
                                        )?;
                                        max_queued_records = max_queued_records.max(queued);
                                        if queued.saturating_add(records) >= backlog_high {
                                            bulk_mode = true;
                                        }
                                        if records != 0
                                            && records.saturating_add(requests.len())
                                                > batch_records
                                        {
                                            queued_records
                                                .fetch_add(requests.len(), Ordering::Release);
                                            carry = Some(WalOwnerIngressCommand::Batch {
                                                ingress,
                                                requests,
                                                immediate: next_immediate,
                                            });
                                            break;
                                        }
                                        immediate |= !bulk_mode
                                            && queued <= backlog_low
                                            && Self::mixed_dispatch_is_immediate(
                                                next_immediate,
                                                &requests,
                                                &mut read_hot_until,
                                                mixed_hysteresis,
                                            );
                                        records = records.saturating_add(requests.len());
                                        commands.push_back((ingress, requests, next_immediate));
                                    }
                                    Ok(control) => {
                                        carry = Some(control);
                                        break;
                                    }
                                    Err(TryRecvError::Disconnected) => {
                                        let error = io::Error::new(
                                            io::ErrorKind::BrokenPipe,
                                            "WAL owner command queue disconnected",
                                        );
                                        report_failure(&error);
                                        return Err(error);
                                    }
                                    Err(TryRecvError::Empty) => {
                                        if immediate || fill_us == 0 || Instant::now() >= deadline {
                                            break;
                                        }
                                        std::hint::spin_loop();
                                    }
                                }
                            }

                            while !commands.is_empty() {
                                let mut windows = Vec::with_capacity(pipeline_batches);
                                for _ in 0..pipeline_batches {
                                    if commands.is_empty()
                                        && carry.is_none()
                                        && !windows.is_empty()
                                    {
                                        for spin in 0..=pipeline_refill_spins {
                                            match command_rx.try_recv() {
                                                Ok(WalOwnerIngressCommand::Batch {
                                                    ingress,
                                                    requests,
                                                    immediate,
                                                }) => {
                                                    let queued = Self::note_records_dequeued(
                                                        requests.len(),
                                                        queued_records,
                                                    )?;
                                                    max_queued_records =
                                                        max_queued_records.max(queued);
                                                    if queued >= backlog_high {
                                                        bulk_mode = true;
                                                    }
                                                    commands.push_back((
                                                        ingress, requests, immediate,
                                                    ));
                                                    break;
                                                }
                                                Ok(control) => {
                                                    carry = Some(control);
                                                    break;
                                                }
                                                Err(TryRecvError::Disconnected) => {
                                                    let error = io::Error::new(
                                                        io::ErrorKind::BrokenPipe,
                                                        "WAL owner command queue disconnected",
                                                    );
                                                    report_failure(&error);
                                                    return Err(error);
                                                }
                                                Err(TryRecvError::Empty) => {
                                                    if spin != pipeline_refill_spins {
                                                        std::hint::spin_loop();
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    if commands.is_empty() {
                                        break;
                                    }
                                    let mut staged_records = commands
                                        .iter()
                                        .map(|(_, requests, _)| requests.len())
                                        .sum::<usize>();
                                    let staged_immediate = windows.is_empty()
                                        && !bulk_mode
                                        && commands.iter().any(|(_, _, immediate)| *immediate)
                                        && queued_records.load(Ordering::Acquire) <= backlog_low;
                                    let staged_budget_us = if bulk_mode {
                                        fill_us
                                    } else {
                                        debounce_us
                                    };
                                    let staged_deadline = Instant::now()
                                        .checked_add(Duration::from_micros(staged_budget_us))
                                        .unwrap_or_else(Instant::now);
                                    while staged_records < fill_records
                                        && staged_records < batch_records
                                        && carry.is_none()
                                    {
                                        match command_rx.try_recv() {
                                            Ok(WalOwnerIngressCommand::Batch {
                                                ingress,
                                                requests,
                                                immediate,
                                            }) => {
                                                let request_count = requests.len();
                                                let queued = Self::note_records_dequeued(
                                                    request_count,
                                                    queued_records,
                                                )?;
                                                max_queued_records =
                                                    max_queued_records.max(queued);
                                                if queued.saturating_add(staged_records)
                                                    >= backlog_high
                                                {
                                                    bulk_mode = true;
                                                }
                                                if staged_records
                                                    .saturating_add(request_count)
                                                    > batch_records
                                                {
                                                    queued_records.fetch_add(
                                                        request_count,
                                                        Ordering::Release,
                                                    );
                                                    carry = Some(WalOwnerIngressCommand::Batch {
                                                        ingress,
                                                        requests,
                                                        immediate,
                                                    });
                                                    break;
                                                }
                                                staged_records =
                                                    staged_records.saturating_add(request_count);
                                                commands.push_back((
                                                    ingress, requests, immediate,
                                                ));
                                            }
                                            Ok(control) => {
                                                carry = Some(control);
                                                break;
                                            }
                                            Err(TryRecvError::Disconnected) => {
                                                let error = io::Error::new(
                                                    io::ErrorKind::BrokenPipe,
                                                    "WAL owner command queue disconnected",
                                                );
                                                report_failure(&error);
                                                return Err(error);
                                            }
                                            Err(TryRecvError::Empty) => {
                                                if staged_immediate
                                                    || staged_budget_us == 0
                                                    || Instant::now() >= staged_deadline
                                                {
                                                    break;
                                                }
                                                std::hint::spin_loop();
                                            }
                                        }
                                    }
                                    let mut batch = Vec::new();
                                    let mut segments = Vec::new();
                                    while let Some((segment_ingress, segment, _)) = commands.front()
                                    {
                                        if !batch.is_empty()
                                            && batch.len().saturating_add(segment.len())
                                                > batch_records
                                        {
                                            break;
                                        }
                                        let segment_ingress = *segment_ingress;
                                        let mut segment = commands
                                            .pop_front()
                                            .expect("owner command front disappeared")
                                            .1;
                                        segments.push((segment_ingress, segment.len()));
                                        batch.append(&mut segment);
                                    }
                                    if batch.is_empty() {
                                        let (segment_ingress, mut segment, _) = commands
                                            .pop_front()
                                            .expect("nonempty owner queue lost its command");
                                        segments.push((segment_ingress, segment.len()));
                                        batch.append(&mut segment);
                                    }
                                    while batch.len() < batch_records && carry.is_none() {
                                        match command_rx.try_recv() {
                                            Ok(WalOwnerIngressCommand::Batch {
                                                ingress,
                                                mut requests,
                                                immediate,
                                            }) => {
                                                let queued = Self::note_records_dequeued(
                                                    requests.len(),
                                                    queued_records,
                                                )?;
                                                max_queued_records =
                                                    max_queued_records.max(queued);
                                                if queued >= backlog_high {
                                                    bulk_mode = true;
                                                }
                                                if batch
                                                    .len()
                                                    .saturating_add(requests.len())
                                                    > batch_records
                                                {
                                                    queued_records.fetch_add(
                                                        requests.len(),
                                                        Ordering::Release,
                                                    );
                                                    carry = Some(WalOwnerIngressCommand::Batch {
                                                        ingress,
                                                        requests,
                                                        immediate,
                                                    });
                                                    break;
                                                }
                                                segments.push((ingress, requests.len()));
                                                batch.append(&mut requests);
                                            }
                                            Ok(control) => {
                                                carry = Some(control);
                                                break;
                                            }
                                            Err(TryRecvError::Disconnected) => {
                                                let error = io::Error::new(
                                                    io::ErrorKind::BrokenPipe,
                                                    "WAL owner command queue disconnected",
                                                );
                                                report_failure(&error);
                                                return Err(error);
                                            }
                                            Err(TryRecvError::Empty) => break,
                                        }
                                    }
                                    wire_batches = wire_batches.saturating_add(1);
                                    wire_records =
                                        wire_records.saturating_add(batch.len() as u64);
                                    max_wire_batch_records = max_wire_batch_records.max(batch.len());
                                    if windows.is_empty() && immediate && !bulk_mode {
                                        immediate_batches = immediate_batches.saturating_add(1);
                                    } else if bulk_mode {
                                        bulk_batches = bulk_batches.saturating_add(1);
                                    } else {
                                        debounced_batches = debounced_batches.saturating_add(1);
                                    }
                                    if let Err(error) =
                                        remote.send_request_batch(&mut tx, mapping.as_ref(), &batch)
                                    {
                                        report_failure(&error);
                                        return Err(error);
                                    }
                                    windows.push((batch, segments));
                                }
                                for (batch, segments) in windows {
                                    if let Err(error) = remote.recv_request_batch_into(
                                        &mut tx,
                                        mapping.as_ref(),
                                        &batch,
                                    ) {
                                        report_failure(&error);
                                        return Err(error);
                                    }
                                    let mut offset = 0usize;
                                    for (segment_ingress, len) in segments {
                                        let end = offset.checked_add(len).ok_or_else(|| {
                                            io::Error::new(
                                                io::ErrorKind::InvalidData,
                                                "owner result segment overflow",
                                            )
                                        })?;
                                        let result = batch[offset..end].to_vec();
                                        send_owner_result(
                                            &result_txs[segment_ingress as usize],
                                            WalOwnerIngressResult::Batch(result),
                                        )?;
                                        offset = end;
                                    }
                                    if offset != batch.len() {
                                        return Err(io::Error::new(
                                            io::ErrorKind::InvalidData,
                                            "owner result segments do not cover the remote batch",
                                        ));
                                    }
                                }
                            }
                        }
                        WalOwnerIngressCommand::Sync { ingress, sequence } => {
                            if let Err(error) = remote.sync(sequence) {
                                report_failure(&error);
                                return Err(error);
                            }
                            send_owner_result(
                                &result_txs[ingress as usize],
                                WalOwnerIngressResult::Sync(sequence),
                            )?;
                        }
                        WalOwnerIngressCommand::Eof => {
                            tx.ensure_idle()?;
                            remote.eof()?;
                            eprintln!(
                                "zcnblk-shm-owner-wait-summary: owner={} adaptive={} min_spins={} max_spins={} final_spins={} quick_wait_ns={} pipeline_refill_spins={} debounce_us={} backlog_low={} backlog_high={} wire_batches={} wire_records={} avg_wire_batch_records={:.2} max_wire_batch_records={} immediate_batches={} debounced_batches={} bulk_batches={} max_queued_records={} final_queued_records={} spin_hits={} blocking_waits={} quick_blocking_waits={}",
                                owner,
                                command_wait.enabled,
                                command_wait.min_spins,
                                command_wait.max_spins,
                                command_wait.current_spins,
                                command_wait.quick_wait_ns,
                                pipeline_refill_spins,
                                debounce_us,
                                backlog_low,
                                backlog_high,
                                wire_batches,
                                wire_records,
                                wire_records as f64 / wire_batches.max(1) as f64,
                                max_wire_batch_records,
                                immediate_batches,
                                debounced_batches,
                                bulk_batches,
                                max_queued_records,
                                queued_records.load(Ordering::Acquire),
                                command_wait.spin_hits,
                                command_wait.blocking_waits,
                                command_wait.quick_blocking_waits,
                            );
                            return Ok(remote);
                        }
                    }
                }
            })?;
        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                command_tx,
                handle: Some(handle),
            }),
            Ok(Err(error)) => {
                let _ = handle.join();
                Err(error)
            }
            Err(_) => Err(io::Error::other(
                "WAL owner exited before startup acknowledgement",
            )),
        }
    }

    fn stop(mut self) -> io::Result<RemoteWalLeaf> {
        send_sync_channel_spinning(&self.command_tx, WalOwnerIngressCommand::Eof)?;
        self.handle
            .take()
            .ok_or_else(|| io::Error::other("WAL owner join handle is missing"))?
            .join()
            .map_err(|_| io::Error::other("WAL owner worker panicked"))?
    }
}

struct WalOwnerIngressEndpoint {
    ingress: u32,
    owner_commands: Arc<[SyncSender<WalOwnerIngressCommand>]>,
    queued_records_by_owner: Arc<[AtomicUsize]>,
    result_rx: Receiver<WalOwnerIngressResult>,
    owner_extent_records: u64,
}

enum WalLaneTransport {
    Inline {
        remote: RemoteWalLeaf,
        tx: RemoteWalTxContext,
        batches: VecDeque<WalLanePendingBatch>,
    },
    Split {
        worker: WalLaneTransportWorker,
        in_flight: usize,
    },
    OwnerIngress {
        endpoint: WalOwnerIngressEndpoint,
        in_flight: usize,
        foreground_in_flight: usize,
        foreground_immediate_limit: usize,
        pending_by_owner: Vec<Vec<PendingRemoteRead>>,
        urgent_owner_words: Vec<u64>,
        fragment_records: usize,
        fragment_fill_us: u64,
        fragment_started: Option<Instant>,
    },
}

impl WalLaneTransport {
    fn batch_has_foreground(batch: &[PendingRemoteRead]) -> bool {
        batch
            .iter()
            .any(|pending| pending.request.op != ZCNBLK_SHM_OP_WRITE)
    }

    fn default_owner_fragment_records(owner_count: usize) -> usize {
        owner_count.clamp(1, 16)
    }

    fn update_owner_fragment_deadline(started: &mut Option<Instant>, has_pending: bool) {
        if has_pending {
            started.get_or_insert_with(Instant::now);
        } else {
            *started = None;
        }
    }

    fn start(
        mut remote: RemoteWalLeaf,
        mapping: Arc<Mapping>,
        split_cpu: Option<usize>,
        queue_depth: usize,
    ) -> io::Result<Self> {
        if let Some(cpu) = split_cpu {
            return Ok(Self::Split {
                worker: WalLaneTransportWorker::start(remote, mapping, cpu, queue_depth)?,
                in_flight: 0,
            });
        }
        remote.attach_mapping(mapping)?;
        let tx = RemoteWalTxContext::new(&remote)?;
        Ok(Self::Inline {
            remote,
            tx,
            batches: VecDeque::with_capacity(queue_depth),
        })
    }

    fn start_owner_ingress(endpoint: WalOwnerIngressEndpoint) -> Self {
        let owner_count = endpoint.owner_commands.len();
        let fragment_records = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_FRAGMENT_RECORDS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_else(|| Self::default_owner_fragment_records(owner_count))
            .max(1);
        let fragment_fill_us = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_FRAGMENT_FILL_US")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(500);
        let foreground_immediate_limit =
            env::var("URING_PLAY_ZCNBLK_SHM_OWNER_FOREGROUND_IMMEDIATE_LIMIT")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(1);
        Self::OwnerIngress {
            endpoint,
            in_flight: 0,
            foreground_in_flight: 0,
            foreground_immediate_limit,
            pending_by_owner: vec![Vec::with_capacity(fragment_records); owner_count],
            urgent_owner_words: vec![0; owner_count.div_ceil(64)],
            fragment_records,
            fragment_fill_us,
            fragment_started: None,
        }
    }

    fn flush_owner_pending(
        endpoint: &WalOwnerIngressEndpoint,
        pending_by_owner: &mut [Vec<PendingRemoteRead>],
        in_flight: &mut usize,
        foreground_in_flight: &mut usize,
        only_full: bool,
        fragment_records: usize,
    ) -> io::Result<()> {
        for (owner, pending) in pending_by_owner.iter_mut().enumerate() {
            Self::flush_owner_queue(
                endpoint,
                owner,
                pending,
                in_flight,
                foreground_in_flight,
                only_full,
                fragment_records,
                false,
            )?;
        }
        Ok(())
    }

    fn flush_owner_queue(
        endpoint: &WalOwnerIngressEndpoint,
        owner: usize,
        pending: &mut Vec<PendingRemoteRead>,
        in_flight: &mut usize,
        foreground_in_flight: &mut usize,
        only_full: bool,
        fragment_records: usize,
        immediate: bool,
    ) -> io::Result<()> {
        while pending.len() >= fragment_records || (!only_full && !pending.is_empty()) {
            let take = pending.len().min(fragment_records);
            let requests = pending.drain(..take).collect::<Vec<_>>();
            let has_foreground = Self::batch_has_foreground(&requests);
            endpoint.queued_records_by_owner[owner].fetch_add(take, Ordering::Release);
            if let Err(error) = send_sync_channel_spinning(
                &endpoint.owner_commands[owner],
                WalOwnerIngressCommand::Batch {
                    ingress: endpoint.ingress,
                    requests,
                    immediate,
                },
            ) {
                endpoint.queued_records_by_owner[owner].fetch_sub(take, Ordering::AcqRel);
                return Err(error);
            }
            *in_flight += 1;
            *foreground_in_flight += usize::from(has_foreground);
        }
        Ok(())
    }

    fn in_flight_len(&self) -> usize {
        match self {
            Self::Inline { batches, .. } => batches.len(),
            Self::Split { in_flight, .. } => *in_flight,
            Self::OwnerIngress { in_flight, .. } => *in_flight,
        }
    }

    fn submit_available(&self, lane_window: usize) -> bool {
        if self.in_flight_len() >= lane_window {
            return false;
        }
        match self {
            Self::Inline {
                remote, batches, ..
            } => remote.stream.transport_label() != "ofi" || !wal_lane_has_framed_batch(batches),
            Self::Split { .. } | Self::OwnerIngress { .. } => true,
        }
    }

    fn foreground_in_flight_len(&self) -> usize {
        match self {
            Self::OwnerIngress {
                foreground_in_flight,
                ..
            } => *foreground_in_flight,
            Self::Inline { .. } | Self::Split { .. } => self.in_flight_len(),
        }
    }

    fn foreground_immediate_available(&self) -> bool {
        match self {
            Self::OwnerIngress {
                foreground_in_flight,
                foreground_immediate_limit,
                ..
            } => *foreground_in_flight < *foreground_immediate_limit,
            Self::Inline { .. } | Self::Split { .. } => true,
        }
    }

    fn has_pending(&self) -> bool {
        match self {
            Self::OwnerIngress {
                pending_by_owner, ..
            } => pending_by_owner.iter().any(|pending| !pending.is_empty()),
            Self::Inline { .. } | Self::Split { .. } => false,
        }
    }

    fn flush_owner_pending_if_due(&mut self, force: bool) -> io::Result<bool> {
        let Self::OwnerIngress {
            endpoint,
            in_flight,
            foreground_in_flight,
            pending_by_owner,
            fragment_records,
            fragment_fill_us,
            fragment_started,
            ..
        } = self
        else {
            return Ok(false);
        };
        let has_pending = pending_by_owner.iter().any(|pending| !pending.is_empty());
        if !has_pending {
            *fragment_started = None;
            return Ok(false);
        }
        let due = force
            || *fragment_fill_us == 0
            || fragment_started.get_or_insert_with(Instant::now).elapsed()
                >= Duration::from_micros(*fragment_fill_us);
        if !due {
            return Ok(false);
        }
        Self::flush_owner_pending(
            endpoint,
            pending_by_owner,
            in_flight,
            foreground_in_flight,
            false,
            *fragment_records,
        )?;
        *fragment_started = None;
        Ok(true)
    }

    fn submit(&mut self, mapping: &Mapping, batch: Vec<PendingRemoteRead>) -> io::Result<()> {
        match self {
            Self::Inline {
                remote,
                tx,
                batches,
            } => {
                let framed_in_flight = batches
                    .iter()
                    .any(|pending| matches!(pending.kind, WalLanePendingBatchKind::Framed));
                let rma_batch_id = if framed_in_flight {
                    None
                } else {
                    remote.try_submit_rma_read_batch(&batch)?
                };
                let kind = if let Some(batch_id) = rma_batch_id {
                    WalLanePendingBatchKind::Rma(batch_id)
                } else {
                    remote.drain_rma_reads(mapping)?;
                    remote.send_request_batch(tx, mapping, &batch)?;
                    WalLanePendingBatchKind::Framed
                };
                batches.push_back(WalLanePendingBatch {
                    requests: batch,
                    kind,
                });
            }
            Self::Split { worker, in_flight } => {
                worker.send(WalLaneTransportCommand::Batch(batch))?;
                *in_flight += 1;
            }
            Self::OwnerIngress {
                endpoint,
                in_flight,
                foreground_in_flight,
                foreground_immediate_limit,
                pending_by_owner,
                urgent_owner_words,
                fragment_records,
                fragment_started,
                ..
            } => {
                urgent_owner_words.fill(0);
                let mut urgent_budget =
                    foreground_immediate_limit.saturating_sub(*foreground_in_flight);
                for pending in batch {
                    let owner = wal_transport_owner(
                        pending.request.offset,
                        pending_by_owner.len(),
                        endpoint.owner_extent_records,
                    )?;
                    let owner_bit = 1_u64 << (owner % 64);
                    if pending.request.op != ZCNBLK_SHM_OP_WRITE
                        && urgent_budget != 0
                        && urgent_owner_words[owner / 64] & owner_bit == 0
                    {
                        urgent_owner_words[owner / 64] |= owner_bit;
                        urgent_budget -= 1;
                    }
                    pending_by_owner[owner].push(pending);
                    fragment_started.get_or_insert_with(Instant::now);
                }
                for (word_index, mut owners) in urgent_owner_words.iter().copied().enumerate() {
                    while owners != 0 {
                        let bit = owners.trailing_zeros() as usize;
                        owners &= owners - 1;
                        let owner = word_index * 64 + bit;
                        Self::flush_owner_queue(
                            endpoint,
                            owner,
                            &mut pending_by_owner[owner],
                            in_flight,
                            foreground_in_flight,
                            false,
                            *fragment_records,
                            true,
                        )?;
                    }
                }
                Self::flush_owner_pending(
                    endpoint,
                    pending_by_owner,
                    in_flight,
                    foreground_in_flight,
                    true,
                    *fragment_records,
                )?;
                Self::update_owner_fragment_deadline(
                    fragment_started,
                    pending_by_owner.iter().any(|pending| !pending.is_empty()),
                );
            }
        }
        Ok(())
    }

    fn decode_result(result: WalLaneTransportResult) -> io::Result<Vec<PendingRemoteRead>> {
        match result {
            WalLaneTransportResult::Batch(batch) => Ok(batch),
            WalLaneTransportResult::Sync(sequence) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected WAL sync result sequence={sequence} while awaiting a batch"),
            )),
            WalLaneTransportResult::Failed(error) => Err(error),
        }
    }

    fn recv(&mut self, mapping: &Mapping) -> io::Result<Vec<PendingRemoteRead>> {
        match self {
            Self::Inline {
                remote,
                tx,
                batches,
            } => {
                let pending = batches
                    .pop_front()
                    .ok_or_else(|| io::Error::other("inline WAL receive queue is empty"))?;
                match pending.kind {
                    WalLanePendingBatchKind::Framed => {
                        remote.recv_request_batch_into(tx, mapping, &pending.requests)?;
                    }
                    WalLanePendingBatchKind::Rma(batch_id) => {
                        remote.drain_rma_reads(mapping)?;
                        if !remote.rma_read_batch_complete(batch_id)? {
                            return Err(io::Error::new(
                                io::ErrorKind::WouldBlock,
                                format!(
                                    "OFI RMA batch id={batch_id} did not complete while draining"
                                ),
                            ));
                        }
                        remote.finish_rma_read_batch(batch_id)?;
                    }
                }
                Ok(pending.requests)
            }
            Self::Split { worker, in_flight } => {
                let result = worker.recv()?;
                *in_flight = in_flight
                    .checked_sub(1)
                    .ok_or_else(|| io::Error::other("split WAL in-flight count underflow"))?;
                Self::decode_result(result)
            }
            Self::OwnerIngress {
                endpoint,
                in_flight,
                foreground_in_flight,
                pending_by_owner,
                fragment_records,
                fragment_started,
                ..
            } => {
                if *in_flight == 0 {
                    Self::flush_owner_pending(
                        endpoint,
                        pending_by_owner,
                        in_flight,
                        foreground_in_flight,
                        false,
                        *fragment_records,
                    )?;
                    *fragment_started = None;
                }
                let result = recv_channel_spinning(&endpoint.result_rx, 65_536)?;
                *in_flight = in_flight
                    .checked_sub(1)
                    .ok_or_else(|| io::Error::other("owner ingress in-flight count underflow"))?;
                match result {
                    WalOwnerIngressResult::Batch(batch) => {
                        if Self::batch_has_foreground(&batch) {
                            *foreground_in_flight =
                                foreground_in_flight.checked_sub(1).ok_or_else(|| {
                                    io::Error::other("owner foreground in-flight count underflow")
                                })?;
                        }
                        Ok(batch)
                    }
                    WalOwnerIngressResult::Sync(sequence) => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected owner sync result sequence={sequence}"),
                    )),
                    WalOwnerIngressResult::Failed(error) => Err(error),
                }
            }
        }
    }

    fn try_recv(&mut self) -> io::Result<Option<Vec<PendingRemoteRead>>> {
        match self {
            Self::Inline {
                remote, batches, ..
            } => {
                let Some(kind) = batches.front().map(|pending| pending.kind) else {
                    return Ok(None);
                };
                if let WalLanePendingBatchKind::Rma(batch_id) = kind {
                    remote.progress_rma_reads_attached(false)?;
                    if remote.rma_read_batch_complete(batch_id)? {
                        let pending = batches.pop_front().ok_or_else(|| {
                            io::Error::other("inline WAL receive queue became empty")
                        })?;
                        remote.finish_rma_read_batch(batch_id)?;
                        return Ok(Some(pending.requests));
                    }
                }
                Ok(None)
            }
            Self::Split { worker, in_flight } => {
                let Some(result) = worker.try_recv()? else {
                    return Ok(None);
                };
                *in_flight = in_flight
                    .checked_sub(1)
                    .ok_or_else(|| io::Error::other("split WAL in-flight count underflow"))?;
                Self::decode_result(result).map(Some)
            }
            Self::OwnerIngress {
                endpoint,
                in_flight,
                foreground_in_flight,
                foreground_immediate_limit: _,
                pending_by_owner: _,
                urgent_owner_words: _,
                fragment_records: _,
                fragment_fill_us: _,
                fragment_started: _,
            } => match endpoint.result_rx.try_recv() {
                Ok(WalOwnerIngressResult::Batch(batch)) => {
                    *in_flight = in_flight.checked_sub(1).ok_or_else(|| {
                        io::Error::other("owner ingress in-flight count underflow")
                    })?;
                    if Self::batch_has_foreground(&batch) {
                        *foreground_in_flight =
                            foreground_in_flight.checked_sub(1).ok_or_else(|| {
                                io::Error::other("owner foreground in-flight count underflow")
                            })?;
                    }
                    Ok(Some(batch))
                }
                Ok(WalOwnerIngressResult::Sync(sequence)) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected owner sync result sequence={sequence}"),
                )),
                Ok(WalOwnerIngressResult::Failed(error)) => Err(error),
                Err(TryRecvError::Empty) => Ok(None),
                Err(TryRecvError::Disconnected) => Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "owner ingress result queue disconnected",
                )),
            },
        }
    }

    fn sync(&mut self, sequence: u64) -> io::Result<()> {
        if self.in_flight_len() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "WAL sync requested with transport batches still in flight",
            ));
        }
        match self {
            Self::Inline { remote, .. } => remote.sync(sequence),
            Self::Split { worker, .. } => {
                worker.send(WalLaneTransportCommand::Sync(sequence))?;
                match worker.recv()? {
                    WalLaneTransportResult::Sync(actual) if actual == sequence => Ok(()),
                    WalLaneTransportResult::Sync(actual) => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("WAL sync result mismatch expected={sequence} actual={actual}"),
                    )),
                    WalLaneTransportResult::Batch(_) => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unexpected WAL batch result while awaiting sync",
                    )),
                    WalLaneTransportResult::Failed(error) => Err(error),
                }
            }
            Self::OwnerIngress {
                endpoint,
                pending_by_owner,
                in_flight,
                foreground_in_flight,
                fragment_records,
                fragment_started,
                ..
            } => {
                Self::flush_owner_pending(
                    endpoint,
                    pending_by_owner,
                    in_flight,
                    foreground_in_flight,
                    false,
                    *fragment_records,
                )?;
                *fragment_started = None;
                if *in_flight != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "WAL sync requested before owner fragments drained",
                    ));
                }
                let owner = endpoint.ingress as usize % endpoint.owner_commands.len();
                send_sync_channel_spinning(
                    &endpoint.owner_commands[owner],
                    WalOwnerIngressCommand::Sync {
                        ingress: endpoint.ingress,
                        sequence,
                    },
                )?;
                match recv_channel_spinning(&endpoint.result_rx, 65_536)? {
                    WalOwnerIngressResult::Sync(actual) if actual == sequence => Ok(()),
                    WalOwnerIngressResult::Sync(actual) => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("owner sync result mismatch expected={sequence} actual={actual}"),
                    )),
                    WalOwnerIngressResult::Batch(_) => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unexpected owner batch result while awaiting sync",
                    )),
                    WalOwnerIngressResult::Failed(error) => Err(error),
                }
            }
        }
    }

    fn finish(self) -> io::Result<Option<RemoteWalLeaf>> {
        match self {
            Self::Inline {
                mut remote,
                tx,
                batches,
            } => {
                if !batches.is_empty() {
                    return Err(io::Error::other(
                        "inline WAL transport stopped with batches in flight",
                    ));
                }
                tx.ensure_idle()?;
                remote.eof()?;
                Ok(Some(remote))
            }
            Self::Split { worker, in_flight } => {
                if in_flight != 0 {
                    return Err(io::Error::other(
                        "split WAL transport stopped with batches in flight",
                    ));
                }
                worker.stop().map(Some)
            }
            Self::OwnerIngress {
                in_flight,
                foreground_in_flight,
                pending_by_owner,
                ..
            } => {
                if in_flight != 0
                    || foreground_in_flight != 0
                    || pending_by_owner.iter().any(|pending| !pending.is_empty())
                {
                    return Err(io::Error::other(
                        "owner ingress stopped with batches in flight",
                    ));
                }
                Ok(None)
            }
        }
    }
}

struct WalWritebackState {
    cache: ZcnblkShmArenaDirtyHwmCache,
    pending: VecDeque<PendingWalWrite>,
    releasable: Vec<Vec<u8>>,
    payload_hwm: Vec<u64>,
    writeback_batch: usize,
    writeback_batches: u64,
    writeback_writes: u64,
    writeback_bytes: u64,
    durable_submit_hwm: u64,
}

impl WalWritebackState {
    fn new(
        source: Arc<dyn ZcnblkFanWalSharedLeaseSource>,
        capacity_bytes: u64,
        channels: u32,
        payload_entries: u32,
        ring_entries: u32,
    ) -> io::Result<Self> {
        let requested = env::var("URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(2_048)
            .max(1);
        let reserve = usize::try_from(ring_entries).unwrap_or(usize::MAX).max(2);
        let payload_entries = payload_entries as usize;
        let safe_per_lane = payload_entries.saturating_sub(reserve).max(1);
        let safe_limit = safe_per_lane
            .checked_mul(channels as usize)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "WAL batch limit overflow")
            })?;
        let writeback_batch = requested.min(safe_limit);
        let logical_pages = usize::try_from(capacity_bytes / 4096).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "wal-memory logical page count exceeds usize",
            )
        })?;
        let cache = ZcnblkShmArenaDirtyHwmCache::new_shared(source, logical_pages);
        Ok(Self {
            cache,
            pending: VecDeque::with_capacity(payload_entries),
            releasable: vec![vec![0; payload_entries]; channels as usize],
            payload_hwm: vec![0; channels as usize],
            writeback_batch,
            writeback_batches: 0,
            writeback_writes: 0,
            writeback_bytes: 0,
            durable_submit_hwm: 0,
        })
    }

    fn payload_hwm(&self, channel: u32) -> io::Result<u64> {
        self.payload_hwm
            .get(channel as usize)
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WAL channel out of range"))
    }

    fn mark_releasable(&mut self, channel: u32, request_sequence: u64) -> io::Result<()> {
        let releasable = self.releasable.get_mut(channel as usize).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "WAL channel out of range")
        })?;
        let payload_hwm = self.payload_hwm.get_mut(channel as usize).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "WAL channel out of range")
        })?;
        let idx = request_sequence as usize % releasable.len();
        if releasable[idx] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "WAL payload release ring collision channel={channel} sequence={request_sequence} idx={idx} hwm={payload_hwm}",
                ),
            ));
        }
        releasable[idx] = 1;
        while releasable[*payload_hwm as usize % releasable.len()] != 0 {
            let idx = *payload_hwm as usize % releasable.len();
            releasable[idx] = 0;
            *payload_hwm += 1;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Stats {
    requests: u64,
    writes: u64,
    reads: u64,
    syncs: u64,
    write_bytes: u64,
    read_bytes: u64,
    kicks: u64,
    idle_polls: u64,
    lease_releases: u64,
    early_write_acks: u64,
    dirty_read_hits: u64,
    dirty_read_refs: u64,
    dirty_pressure_events: u64,
    dirty_pressure_evictions: u64,
    max_payload_slots_outstanding: u64,
    remote_read_misses: u64,
    remote_batches: u64,
    completion_window_stalls: u64,
    fua_requests: u64,
    polled_requests: u64,
    ioprio_requests: u64,
    registered_lease_requests: u64,
    atomic_write_requests: u64,
    write_lifetime_requests: u64,
}

#[repr(align(64))]
struct WalCompletionSlot {
    sequence: AtomicU64,
}

struct WalCompletionTracker {
    hwm: AtomicU64,
    advancing: AtomicBool,
    slots: Box<[WalCompletionSlot]>,
    mask: usize,
}

impl WalCompletionTracker {
    fn new(max_in_flight: usize) -> io::Result<Self> {
        let capacity = max_in_flight
            .max(2)
            .checked_next_power_of_two()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "completion ring overflow")
            })?;
        Ok(Self {
            hwm: AtomicU64::new(0),
            advancing: AtomicBool::new(false),
            slots: (0..capacity)
                .map(|_| WalCompletionSlot {
                    sequence: AtomicU64::new(0),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            mask: capacity - 1,
        })
    }

    fn is_complete(&self, sequence: u64) -> bool {
        if sequence == 0 || sequence <= self.hwm.load(Ordering::Acquire) {
            return true;
        }
        if self.slots[sequence as usize & self.mask]
            .sequence
            .load(Ordering::Acquire)
            == sequence
        {
            return true;
        }
        sequence <= self.hwm.load(Ordering::Acquire)
    }

    fn hwm(&self) -> u64 {
        self.hwm.load(Ordering::Acquire)
    }

    fn can_track(&self, sequence: u64) -> bool {
        if sequence == 0 {
            return false;
        }
        let hwm = self.hwm.load(Ordering::Acquire);
        sequence <= hwm || sequence - hwm <= self.slots.len() as u64
    }

    fn mark_complete_deferred(&self, sequence: u64) -> io::Result<()> {
        if sequence == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WAL completion sequence zero is reserved",
            ));
        }
        let slot = &self.slots[sequence as usize & self.mask].sequence;
        slot.compare_exchange(0, sequence, Ordering::Release, Ordering::Acquire)
            .map_err(|occupied| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "WAL completion ring collision sequence={sequence} occupied={occupied} capacity={}",
                        self.slots.len()
                    ),
                )
            })?;
        Ok(())
    }

    fn advance_hwm_locked(&self) -> u64 {
        let current = self.hwm.load(Ordering::Relaxed);
        let mut advanced = current;
        loop {
            let Some(next) = advanced.checked_add(1) else {
                break;
            };
            let next_slot = &self.slots[next as usize & self.mask].sequence;
            if next_slot.load(Ordering::Acquire) != next {
                break;
            }
            if next_slot
                .compare_exchange(next, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                break;
            }
            advanced = next;
        }
        if advanced != current {
            self.hwm.store(advanced, Ordering::Release);
        }
        self.advancing.store(false, Ordering::Release);
        advanced
    }

    fn try_advance_hwm(&self) -> Option<u64> {
        self.advancing
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        Some(self.advance_hwm_locked())
    }

    fn advance_hwm(&self) -> u64 {
        while self
            .advancing
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        self.advance_hwm_locked()
    }

    fn mark_complete(&self, sequence: u64) -> io::Result<u64> {
        self.mark_complete_deferred(sequence)?;
        Ok(self.advance_hwm())
    }

    fn mark_complete_batch(&self, sequences: impl IntoIterator<Item = u64>) -> io::Result<u64> {
        for sequence in sequences {
            self.mark_complete_deferred(sequence)?;
        }
        Ok(self.advance_hwm())
    }
}

/*
 * One lane worker is the sole publisher for each of these trackers. Other
 * lane workers only test exact predecessor tokens. Keeping the owner HWM and
 * ring cache-line isolated avoids the global CAS handoff that dominated the
 * remote 4K profile while retaining exact (not merely contiguous) dependency
 * visibility.
 */
#[repr(align(64))]
struct WalSharedLaneTracker {
    hwm: AtomicU64,
    slots: Box<[AtomicU64]>,
    mask: usize,
}

impl WalSharedLaneTracker {
    fn new(max_in_flight: usize) -> io::Result<Self> {
        let capacity = max_in_flight
            .max(2)
            .checked_next_power_of_two()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "lane tracker overflow"))?;
        Ok(Self {
            hwm: AtomicU64::new(0),
            slots: (0..capacity)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            mask: capacity - 1,
        })
    }

    fn hwm(&self) -> u64 {
        self.hwm.load(Ordering::Acquire)
    }

    fn can_track(&self, sequence: u64) -> bool {
        sequence != 0 && sequence.saturating_sub(self.hwm()) <= self.slots.len() as u64
    }

    fn is_complete(&self, sequence: u64) -> bool {
        sequence == 0
            || sequence <= self.hwm()
            || self.slots[sequence as usize & self.mask].load(Ordering::Acquire) == sequence
    }

    fn mark_complete(&self, sequence: u64) -> io::Result<u64> {
        self.mark_complete_batch(std::iter::once(sequence))
    }

    fn mark_complete_batch(&self, sequences: impl IntoIterator<Item = u64>) -> io::Result<u64> {
        let mut hwm = self.hwm.load(Ordering::Relaxed);
        for sequence in sequences {
            if sequence == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "lane completion sequence zero is reserved",
                ));
            }
            let slot = &self.slots[sequence as usize & self.mask];
            let occupied = slot.load(Ordering::Acquire);
            if occupied != 0 && occupied > hwm {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "lane completion ring collision sequence={sequence} occupied={occupied} capacity={}",
                        self.slots.len()
                    ),
                ));
            }
            slot.store(sequence, Ordering::Release);
        }
        loop {
            let Some(next) = hwm.checked_add(1) else {
                break;
            };
            let slot = &self.slots[next as usize & self.mask];
            if slot.load(Ordering::Acquire) != next {
                break;
            }
            slot.store(0, Ordering::Relaxed);
            hwm = next;
        }
        self.hwm.store(hwm, Ordering::Release);
        Ok(hwm)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WalSyncRequest {
    ordering_epoch: u64,
    lane_tails: Box<[u64]>,
    required_global_hwm: u64,
}

/*
 * A flush closes a kernel admission epoch and snapshots one FIFO tail per
 * channel. The durable marker cannot start until remote lane HWMs dominate
 * that vector. Later traffic may be included conservatively, but an older
 * request hidden in a kernel lane cannot be skipped.
 */
struct WalSyncCoordinator {
    epoch: AtomicU64,
    beginning: AtomicBool,
    requested_epoch: AtomicU64,
    requested_syncs: Mutex<BTreeMap<u64, WalSyncRequest>>,
    active_ordering_epoch: AtomicU64,
    committed_ordering_epoch: AtomicU64,
    announcements: AtomicU64,
    acknowledged_lanes: AtomicU64,
    failed: AtomicBool,
    committed_hwm: AtomicU64,
    remote_epochs: AtomicU64,
    joined_syncs: AtomicU64,
    remote_lane_hwms: Box<[AtomicU64]>,
    frozen_lane_tails: Box<[AtomicU64]>,
    lane_wake_fds: Box<[OwnedFd]>,
    lanes: u64,
    coalesce_us: u64,
}

impl WalSyncCoordinator {
    fn new(lanes: u32, coalesce_us: u64) -> io::Result<Self> {
        let remote_lane_hwms = (0..lanes).map(|_| AtomicU64::new(0)).collect();
        let frozen_lane_tails = (0..lanes).map(|_| AtomicU64::new(0)).collect();
        let lane_wake_fds = (0..lanes)
            .map(|_| {
                let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
                if fd < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
                }
            })
            .collect::<io::Result<Vec<_>>>()?
            .into_boxed_slice();
        Ok(Self {
            epoch: AtomicU64::new(0),
            beginning: AtomicBool::new(false),
            requested_epoch: AtomicU64::new(0),
            requested_syncs: Mutex::new(BTreeMap::new()),
            active_ordering_epoch: AtomicU64::new(0),
            committed_ordering_epoch: AtomicU64::new(0),
            announcements: AtomicU64::new(0),
            acknowledged_lanes: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            committed_hwm: AtomicU64::new(0),
            remote_epochs: AtomicU64::new(0),
            joined_syncs: AtomicU64::new(0),
            remote_lane_hwms,
            frozen_lane_tails,
            lane_wake_fds,
            lanes: u64::from(lanes),
            coalesce_us,
        })
    }

    fn lane_wake_fd(&self, lane: u32) -> io::Result<i32> {
        self.lane_wake_fds
            .get(lane as usize)
            .map(AsRawFd::as_raw_fd)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("WAL sync wake lane {lane} exceeds {} lanes", self.lanes),
                )
            })
    }

    fn wake_all_lanes(&self) -> io::Result<()> {
        let value = 1u64;
        for fd in &self.lane_wake_fds {
            loop {
                let written = unsafe {
                    libc::write(
                        fd.as_raw_fd(),
                        ptr::from_ref(&value).cast::<libc::c_void>(),
                        size_of::<u64>(),
                    )
                };
                if written == size_of::<u64>() as isize {
                    break;
                }
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => break,
                    _ => return Err(error),
                }
            }
        }
        Ok(())
    }

    fn drain_lane_wake(&self, lane: u32) -> io::Result<()> {
        let fd = self.lane_wake_fd(lane)?;
        let mut value = 0u64;
        loop {
            let read = unsafe {
                libc::read(
                    fd,
                    ptr::from_mut(&mut value).cast::<libc::c_void>(),
                    size_of::<u64>(),
                )
            };
            if read == size_of::<u64>() as isize {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(()),
                _ => return Err(error),
            }
        }
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    fn observe_remote_lane_hwm(&self, lane: u32, hwm: u64) -> io::Result<()> {
        let remote_hwm = self.remote_lane_hwms.get(lane as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("WAL remote lane HWM {lane} exceeds {} lanes", self.lanes),
            )
        })?;
        remote_hwm.fetch_max(hwm, Ordering::Release);
        Ok(())
    }

    fn freeze_lane_tails(&self, lane_tails: &[u64]) -> io::Result<()> {
        if lane_tails.len() != self.frozen_lane_tails.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "WAL sync vector has {} lanes, expected {}",
                    lane_tails.len(),
                    self.frozen_lane_tails.len()
                ),
            ));
        }
        for (&tail, frozen) in lane_tails.iter().zip(self.frozen_lane_tails.iter()) {
            frozen.store(tail, Ordering::Release);
        }
        Ok(())
    }

    fn vector_reached(&self, lane_tails: &[u64]) -> bool {
        lane_tails.len() == self.remote_lane_hwms.len()
            && lane_tails
                .iter()
                .zip(self.remote_lane_hwms.iter())
                .all(|(&required, remote)| remote.load(Ordering::Acquire) >= required)
    }

    fn frozen_lane_tail(&self, lane: u32) -> io::Result<u64> {
        self.frozen_lane_tails
            .get(lane as usize)
            .map(|tail| tail.load(Ordering::Acquire))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("WAL frozen lane tail {lane} exceeds {} lanes", self.lanes),
                )
            })
    }

    #[cfg(test)]
    fn begin(&self, epoch: u64, ordering_epoch: u64, lane_tails: &[u64]) -> io::Result<()> {
        if epoch == 0 || ordering_epoch == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WAL sync epoch zero is reserved",
            ));
        }
        if self
            .beginning
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another WAL lane is starting a sync epoch",
            ));
        }
        let result = if self.epoch() != 0 {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("WAL sync epoch {} is already active", self.epoch()),
            ))
        } else {
            self.acknowledged_lanes.store(0, Ordering::Relaxed);
            self.failed.store(false, Ordering::Relaxed);
            self.freeze_lane_tails(lane_tails)?;
            self.active_ordering_epoch
                .store(ordering_epoch, Ordering::Release);
            self.epoch.store(epoch, Ordering::Release);
            self.remote_epochs.fetch_add(1, Ordering::Relaxed);
            Ok(())
        };
        self.beginning.store(false, Ordering::Release);
        result
    }

    fn announce(
        &self,
        epoch: u64,
        ordering_epoch: u64,
        lane_tails: Box<[u64]>,
        required_global_hwm: u64,
    ) -> io::Result<()> {
        if epoch == 0 || ordering_epoch == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WAL sync epoch zero is reserved",
            ));
        }
        if lane_tails.len() != self.remote_lane_hwms.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "WAL sync vector has {} lanes, expected {}",
                    lane_tails.len(),
                    self.remote_lane_hwms.len()
                ),
            ));
        }
        let mut requested = self
            .requested_syncs
            .lock()
            .map_err(|_| io::Error::other("WAL sync request queue poisoned"))?;
        requested.insert(
            epoch,
            WalSyncRequest {
                ordering_epoch,
                lane_tails,
                required_global_hwm,
            },
        );
        self.requested_epoch.store(
            requested.first_key_value().map_or(0, |(&epoch, _)| epoch),
            Ordering::Release,
        );
        self.announcements.fetch_add(1, Ordering::AcqRel);
        drop(requested);
        self.wake_all_lanes()
    }

    fn retire_announcement(&self) -> io::Result<()> {
        self.announcements
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |announcements| {
                announcements.checked_sub(1)
            })
            .map(|_| ())
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "WAL sync announcement count underflow",
                )
            })
    }

    fn requested_epoch(&self) -> u64 {
        self.requested_epoch.load(Ordering::Acquire)
    }

    fn announcement_count(&self) -> u64 {
        self.announcements.load(Ordering::Acquire)
    }

    fn coalesce_us(&self) -> u64 {
        self.coalesce_us
    }

    fn try_begin_requested(&self, remote_global_hwm: u64) -> io::Result<Option<u64>> {
        if self.epoch() != 0
            || self
                .beginning
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
        {
            return Ok(None);
        }
        let result = if self.epoch() != 0 {
            None
        } else {
            let mut requested_syncs = self
                .requested_syncs
                .lock()
                .map_err(|_| io::Error::other("WAL sync request queue poisoned"))?;
            let committed = self.committed_hwm();
            requested_syncs.retain(|&epoch, _| epoch > committed);
            let mut prefix_tails = vec![0u64; self.remote_lane_hwms.len()];
            let mut prefix_ordering_epoch = 0u64;
            let mut prefix_global_hwm = 0u64;
            let mut next = None;
            for (&epoch, requested) in requested_syncs.iter() {
                prefix_ordering_epoch = prefix_ordering_epoch.max(requested.ordering_epoch);
                prefix_global_hwm = prefix_global_hwm.max(requested.required_global_hwm);
                for (tail, &requested_tail) in
                    prefix_tails.iter_mut().zip(requested.lane_tails.iter())
                {
                    *tail = (*tail).max(requested_tail);
                }
                if remote_global_hwm >= prefix_global_hwm && self.vector_reached(&prefix_tails) {
                    next = Some((
                        epoch,
                        prefix_ordering_epoch,
                        prefix_tails.clone().into_boxed_slice(),
                    ));
                }
            }
            if next.is_none() {
                self.requested_epoch.store(
                    requested_syncs
                        .first_key_value()
                        .map_or(0, |(&epoch, _)| epoch),
                    Ordering::Release,
                );
                None
            } else {
                let (requested, ordering_epoch, lane_tails) =
                    next.expect("available requested sync");
                self.acknowledged_lanes.store(0, Ordering::Relaxed);
                self.failed.store(false, Ordering::Relaxed);
                self.freeze_lane_tails(&lane_tails)?;
                self.active_ordering_epoch
                    .store(ordering_epoch, Ordering::Release);
                self.epoch.store(requested, Ordering::Release);
                self.remote_epochs.fetch_add(1, Ordering::Relaxed);
                Some(requested)
            }
        };
        self.beginning.store(false, Ordering::Release);
        if result.is_some() {
            self.wake_all_lanes()?;
        }
        Ok(result)
    }

    fn committed_hwm(&self) -> u64 {
        self.committed_hwm.load(Ordering::Acquire)
    }

    fn try_join(&self, epoch: u64, ordering_epoch: u64) -> bool {
        if epoch == 0
            || self
                .beginning
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
        {
            return false;
        }
        let joined = if self.epoch() != 0
            || self.requested_epoch() != 0
            || ordering_epoch > self.committed_ordering_epoch.load(Ordering::Acquire)
        {
            false
        } else {
            let required_hwm = epoch - 1;
            let mut committed = self.committed_hwm();
            loop {
                if committed == 0 || required_hwm > committed {
                    break false;
                }
                if epoch <= committed {
                    self.joined_syncs.fetch_add(1, Ordering::Relaxed);
                    break true;
                }
                match self.committed_hwm.compare_exchange_weak(
                    committed,
                    epoch,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        self.joined_syncs.fetch_add(1, Ordering::Relaxed);
                        break true;
                    }
                    Err(actual) => committed = actual,
                }
            }
        };
        self.beginning.store(false, Ordering::Release);
        joined
    }

    fn remote_epochs(&self) -> u64 {
        self.remote_epochs.load(Ordering::Relaxed)
    }

    fn joined_syncs(&self) -> u64 {
        self.joined_syncs.load(Ordering::Relaxed)
    }

    fn service(
        &self,
        lane: u32,
        last_synced_epoch: &mut u64,
        mut sync_remote: impl FnMut(u64) -> io::Result<()>,
    ) -> io::Result<bool> {
        if lane >= 64 || u64::from(lane) >= self.lanes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("WAL sync service lane {lane} exceeds {} lanes", self.lanes),
            ));
        }
        let epoch = self.epoch();
        if epoch == 0 || epoch == *last_synced_epoch {
            return Ok(false);
        }
        // A lane may already have admitted post-fence writes before another
        // lane observes the flush. Freezing that larger tail is conservative:
        // the marker makes those writes durable too, and the cut never grows.
        let _required_tail = self.frozen_lane_tail(lane)?;
        if let Err(error) = sync_remote(epoch) {
            self.failed.store(true, Ordering::Release);
            return Err(error);
        }
        *last_synced_epoch = epoch;
        self.acknowledged_lanes
            .fetch_or(1u64 << lane, Ordering::AcqRel);
        Ok(true)
    }

    fn acknowledged_lane_mask(&self) -> u64 {
        self.acknowledged_lanes.load(Ordering::Acquire)
    }

    fn lane_needs_service(&self, _lane: u32) -> bool {
        self.epoch() != 0
    }

    fn frozen_vector(&self) -> String {
        self.frozen_lane_tails
            .iter()
            .enumerate()
            .map(|(lane, tail)| format!("{lane}:{}", tail.load(Ordering::Acquire)))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn remote_vector(&self) -> String {
        self.remote_lane_hwms
            .iter()
            .enumerate()
            .map(|(lane, hwm)| format!("{lane}:{}", hwm.load(Ordering::Acquire)))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn expected_lane_mask(&self) -> u64 {
        if self.lanes >= 64 {
            u64::MAX
        } else {
            (1u64 << self.lanes) - 1
        }
    }

    #[cfg(test)]
    fn all_acknowledged(&self, epoch: u64) -> io::Result<bool> {
        if self.failed.load(Ordering::Acquire) {
            return Err(io::Error::other(format!(
                "one or more WAL lanes failed sync epoch {epoch}"
            )));
        }
        if self.epoch() != epoch {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("WAL sync epoch changed while waiting for {epoch}"),
            ));
        }
        Ok(self.acknowledged_lane_mask() == self.expected_lane_mask())
    }

    fn try_finish(&self, epoch: u64) -> io::Result<bool> {
        if self
            .beginning
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Ok(false);
        }
        let result = (|| {
            if self.failed.load(Ordering::Acquire) {
                return Err(io::Error::other(format!(
                    "one or more WAL lanes failed sync epoch {epoch}"
                )));
            }
            let active = self.epoch();
            if active != epoch {
                if self.committed_hwm() >= epoch {
                    return Ok(false);
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("WAL sync epoch changed while finishing {epoch}: active={active}"),
                ));
            }
            if self.acknowledged_lane_mask() != self.expected_lane_mask() {
                return Ok(false);
            }
            self.committed_hwm.fetch_max(epoch, Ordering::AcqRel);
            let ordering_epoch = self.active_ordering_epoch.load(Ordering::Acquire);
            self.committed_ordering_epoch
                .fetch_max(ordering_epoch, Ordering::AcqRel);
            match self
                .epoch
                .compare_exchange(epoch, 0, Ordering::Release, Ordering::Acquire)
            {
                Ok(_) => {
                    let mut requested = self
                        .requested_syncs
                        .lock()
                        .map_err(|_| io::Error::other("WAL sync request queue poisoned"))?;
                    requested.retain(|&requested_epoch, _| requested_epoch > epoch);
                    self.requested_epoch.store(
                        requested.first_key_value().map_or(0, |(&epoch, _)| epoch),
                        Ordering::Release,
                    );
                    self.active_ordering_epoch.store(0, Ordering::Release);
                    Ok(true)
                }
                Err(0) if self.committed_hwm() >= epoch => Ok(false),
                Err(active) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("WAL sync finish expected {epoch}, found {active}"),
                )),
            }
        })();
        self.beginning.store(false, Ordering::Release);
        result
    }

    #[cfg(test)]
    fn finish(&self, epoch: u64) -> io::Result<()> {
        if self.try_finish(epoch)? || self.committed_hwm() >= epoch {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("WAL sync epoch {epoch} is not fully acknowledged"),
            ))
        }
    }
}

struct WalConcurrentDirtySlot {
    sequence: AtomicU64,
    readers: AtomicU64,
    evicted: AtomicBool,
    logical_page: UnsafeCell<u64>,
    payload_offset: UnsafeCell<usize>,
}

// A payload slot has one lane owner. Metadata is immutable after sequence
// publication, and commit waits for readers before releasing the slot lease.
unsafe impl Sync for WalConcurrentDirtySlot {}

struct WalConcurrentDirtyCache {
    heads: Box<[AtomicPtr<WalConcurrentDirtySlot>]>,
    slots: Box<[WalConcurrentDirtySlot]>,
    payload_entries: usize,
}

#[derive(Clone, Copy)]
struct WalDirtyReadRef {
    slot_index: usize,
    source_channel: u32,
    payload_slot: u32,
}

#[derive(Clone, Copy)]
struct OutstandingWalDirtyReadRef {
    completion_marker: u64,
    dirty_ref: WalDirtyReadRef,
}

impl WalConcurrentDirtyCache {
    fn new(logical_pages: usize, channels: usize, payload_entries: usize) -> io::Result<Self> {
        let slot_count = channels.checked_mul(payload_entries).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "dirty slot count overflow")
        })?;
        Ok(Self {
            heads: (0..logical_pages.max(1))
                .map(|_| AtomicPtr::new(ptr::null_mut()))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            slots: (0..slot_count.max(1))
                .map(|_| WalConcurrentDirtySlot {
                    sequence: AtomicU64::new(0),
                    readers: AtomicU64::new(0),
                    evicted: AtomicBool::new(false),
                    logical_page: UnsafeCell::new(0),
                    payload_offset: UnsafeCell::new(0),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            payload_entries: payload_entries.max(1),
        })
    }

    fn slot_index(&self, channel: u32, payload_slot: u32) -> io::Result<usize> {
        if payload_slot as usize >= self.payload_entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "dirty payload slot is out of range",
            ));
        }
        (channel as usize)
            .checked_mul(self.payload_entries)
            .and_then(|base| base.checked_add(payload_slot as usize))
            .filter(|index| *index < self.slots.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "dirty slot index overflow"))
    }

    fn admit(
        &self,
        channel: u32,
        payload_slot: u32,
        logical_page: u64,
        payload_offset: usize,
        submit_sequence: u64,
    ) -> io::Result<()> {
        if submit_sequence == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "dirty submit sequence zero is reserved",
            ));
        }
        let page = self.heads.get(logical_page as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "dirty logical page is out of range",
            )
        })?;
        let slot = &self.slots[self.slot_index(channel, payload_slot)?];
        if slot.sequence.load(Ordering::Acquire) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "dirty payload slot reused before remote commit channel={channel} payload_slot={payload_slot}"
                ),
            ));
        }
        unsafe {
            *slot.logical_page.get() = logical_page;
            *slot.payload_offset.get() = payload_offset;
        }
        slot.evicted.store(false, Ordering::Relaxed);
        slot.sequence.store(submit_sequence, Ordering::Release);
        let old = page.swap(ptr::from_ref(slot).cast_mut(), Ordering::AcqRel);
        if !old.is_null() && old != ptr::from_ref(slot).cast_mut() {
            unsafe { &*old }.evicted.store(true, Ordering::Release);
        }
        Ok(())
    }

    fn copy_if_present(
        &self,
        logical_page: u64,
        payload_len: usize,
        source: &dyn ZcnblkFanWalSharedLeaseSource,
        out: &mut [u8],
    ) -> io::Result<bool> {
        let head = self.heads.get(logical_page as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "dirty logical page is out of range",
            )
        })?;
        loop {
            let slot_ptr = head.load(Ordering::Acquire);
            if slot_ptr.is_null() {
                return Ok(false);
            }
            let slot = unsafe { &*slot_ptr };
            slot.readers.fetch_add(1, Ordering::AcqRel);
            let sequence = slot.sequence.load(Ordering::Acquire);
            let slot_page = unsafe { *slot.logical_page.get() };
            let payload_offset = unsafe { *slot.payload_offset.get() };
            let valid = sequence != 0
                && slot_page == logical_page
                && head.load(Ordering::Acquire) == slot_ptr;
            if valid {
                let copy_result = source
                    .payload_slice(payload_offset, payload_len)
                    .map(|payload| out.copy_from_slice(payload));
                slot.readers.fetch_sub(1, Ordering::Release);
                copy_result?;
                return Ok(true);
            }
            slot.readers.fetch_sub(1, Ordering::Release);
            std::hint::spin_loop();
        }
    }

    fn acquire_ref_if_present(&self, logical_page: u64) -> io::Result<Option<WalDirtyReadRef>> {
        let head = self.heads.get(logical_page as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "dirty logical page is out of range",
            )
        })?;
        loop {
            let slot_ptr = head.load(Ordering::Acquire);
            if slot_ptr.is_null() {
                return Ok(None);
            }
            let slot = unsafe { &*slot_ptr };
            slot.readers.fetch_add(1, Ordering::AcqRel);
            let sequence = slot.sequence.load(Ordering::Acquire);
            let slot_page = unsafe { *slot.logical_page.get() };
            let valid = sequence != 0
                && slot_page == logical_page
                && head.load(Ordering::Acquire) == slot_ptr;
            if valid {
                let slot_index = unsafe { slot_ptr.offset_from(self.slots.as_ptr()) as usize };
                return Ok(Some(WalDirtyReadRef {
                    slot_index,
                    source_channel: (slot_index / self.payload_entries) as u32,
                    payload_slot: (slot_index % self.payload_entries) as u32,
                }));
            }
            slot.readers.fetch_sub(1, Ordering::Release);
            std::hint::spin_loop();
        }
    }

    fn release_ref(&self, dirty_ref: WalDirtyReadRef) -> io::Result<()> {
        let slot = self.slots.get(dirty_ref.slot_index).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "dirty read ref slot is out of range",
            )
        })?;
        let previous = slot.readers.fetch_sub(1, Ordering::AcqRel);
        if previous == 0 {
            slot.readers.fetch_add(1, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "dirty read ref reader count underflow",
            ));
        }
        Ok(())
    }

    fn is_evicted(
        &self,
        channel: u32,
        payload_slot: u32,
        submit_sequence: u64,
    ) -> io::Result<bool> {
        let slot = &self.slots[self.slot_index(channel, payload_slot)?];
        let actual = slot.sequence.load(Ordering::Acquire);
        if actual != submit_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "dirty eviction sequence mismatch expected={submit_sequence} actual={actual}"
                ),
            ));
        }
        Ok(slot.evicted.load(Ordering::Acquire))
    }

    fn retire(
        &self,
        channel: u32,
        payload_slot: u32,
        logical_page: u64,
        submit_sequence: u64,
    ) -> io::Result<bool> {
        let head = self.heads.get(logical_page as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "dirty logical page is out of range",
            )
        })?;
        let slot = &self.slots[self.slot_index(channel, payload_slot)?];
        let actual = slot.sequence.load(Ordering::Acquire);
        if actual != submit_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "dirty commit sequence mismatch expected={submit_sequence} actual={actual}"
                ),
            ));
        }
        let slot_ptr = ptr::from_ref(slot).cast_mut();
        let _ = head.compare_exchange(
            slot_ptr,
            ptr::null_mut(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if slot.readers.load(Ordering::Acquire) != 0 {
            return Ok(false);
        }
        slot.evicted.store(false, Ordering::Relaxed);
        slot.sequence
            .compare_exchange(submit_sequence, 0, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|actual| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "dirty commit lost slot ownership expected={submit_sequence} actual={actual}"
                    ),
                )
            })?;
        Ok(true)
    }
}

struct WalLaneReleaseTracker {
    hwm: u64,
    slots: Vec<u64>,
}

struct WalLaneCompletionTracker {
    pending: Vec<Option<PendingRemoteRead>>,
    ready: Vec<bool>,
    ready_queue: VecDeque<u64>,
    pending_count: usize,
}

impl WalLaneCompletionTracker {
    fn new(capacity: usize) -> Self {
        Self {
            pending: vec![None; capacity.max(1)],
            ready: vec![false; capacity.max(1)],
            ready_queue: VecDeque::with_capacity(capacity.max(1)),
            pending_count: 0,
        }
    }

    fn admit(&mut self, pending: PendingRemoteRead, ready: bool) -> io::Result<()> {
        let index = pending.request_sequence as usize % self.pending.len();
        if self.pending[index].is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "lane completion ring collision sequence={} index={index}",
                    pending.request_sequence
                ),
            ));
        }
        self.pending[index] = Some(pending);
        self.pending_count += 1;
        self.ready[index] = ready;
        if ready {
            self.ready_queue.push_back(pending.request_sequence);
        }
        Ok(())
    }

    fn mark_ready(&mut self, request_sequence: u64) -> io::Result<()> {
        let index = request_sequence as usize % self.pending.len();
        let pending = self.pending[index].ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("lane completion readiness without admission sequence={request_sequence}"),
            )
        })?;
        if pending.request_sequence != request_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "lane completion readiness mismatch expected={request_sequence} actual={}",
                    pending.request_sequence
                ),
            ));
        }
        if self.ready[index] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("lane completion sequence={request_sequence} was already ready"),
            ));
        }
        self.ready[index] = true;
        self.ready_queue.push_back(request_sequence);
        Ok(())
    }

    fn pop_ready(&mut self) -> Option<PendingRemoteRead> {
        let request_sequence = self.ready_queue.pop_front()?;
        let index = request_sequence as usize % self.pending.len();
        let pending =
            self.pending[index].expect("ready lane completion must retain its pending request");
        assert_eq!(pending.request_sequence, request_sequence);
        assert!(self.ready[index]);
        self.pending[index] = None;
        self.pending_count -= 1;
        self.ready[index] = false;
        Some(pending)
    }

    fn is_empty(&self) -> bool {
        self.pending_count == 0
    }

    fn len(&self) -> usize {
        self.pending_count
    }
}

impl WalLaneReleaseTracker {
    fn new(capacity: usize) -> Self {
        Self {
            hwm: 0,
            slots: vec![0; capacity.max(1)],
        }
    }

    fn mark_releasable(&mut self, request_sequence: u64) -> io::Result<u64> {
        let marker = request_sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "release sequence overflow")
        })?;
        let index = request_sequence as usize % self.slots.len();
        if self.slots[index] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "lane release ring collision sequence={request_sequence} occupied={}",
                    self.slots[index]
                ),
            ));
        }
        self.slots[index] = marker;
        while self.slots[self.hwm as usize % self.slots.len()] == self.hwm + 1 {
            let index = self.hwm as usize % self.slots.len();
            self.slots[index] = 0;
            self.hwm += 1;
        }
        Ok(self.hwm)
    }
}

fn wal_dirty_pressure_layout(
    payload_entries: usize,
    ring_entries: usize,
    pending_limit: usize,
    configured_reserve: usize,
) -> io::Result<(usize, u64)> {
    if payload_entries < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WAL dirty reference cache needs at least two payload slots",
        ));
    }
    let automatic_reserve = pending_limit.saturating_add(ring_entries).max(2);
    let reserve = if configured_reserve == 0 {
        automatic_reserve
    } else {
        configured_reserve
    }
    .clamp(1, payload_entries - 1);
    Ok((reserve, (payload_entries - reserve) as u64))
}

impl Stats {
    fn note_io_contract(&mut self, contract: ZcnblkWalIoContract) {
        self.fua_requests += u64::from(contract.fua);
        self.polled_requests += u64::from(contract.polled_completion);
        self.ioprio_requests += u64::from(contract.ioprio != 0);
        self.registered_lease_requests += u64::from(contract.registered_lease);
        self.atomic_write_requests += u64::from(contract.atomic_write);
        self.write_lifetime_requests += u64::from(contract.write_lifetime != 0);
    }

    fn add(&mut self, other: &Self) {
        self.requests += other.requests;
        self.writes += other.writes;
        self.reads += other.reads;
        self.syncs += other.syncs;
        self.write_bytes += other.write_bytes;
        self.read_bytes += other.read_bytes;
        self.kicks += other.kicks;
        self.idle_polls += other.idle_polls;
        self.lease_releases += other.lease_releases;
        self.early_write_acks += other.early_write_acks;
        self.dirty_read_hits += other.dirty_read_hits;
        self.dirty_read_refs += other.dirty_read_refs;
        self.dirty_pressure_events += other.dirty_pressure_events;
        self.dirty_pressure_evictions += other.dirty_pressure_evictions;
        self.max_payload_slots_outstanding = self
            .max_payload_slots_outstanding
            .max(other.max_payload_slots_outstanding);
        self.remote_read_misses += other.remote_read_misses;
        self.remote_batches += other.remote_batches;
        self.completion_window_stalls += other.completion_window_stalls;
        self.fua_requests += other.fua_requests;
        self.polled_requests += other.polled_requests;
        self.ioprio_requests += other.ioprio_requests;
        self.registered_lease_requests += other.registered_lease_requests;
        self.atomic_write_requests += other.atomic_write_requests;
        self.write_lifetime_requests += other.write_lifetime_requests;
    }
}

static RUNNING: AtomicBool = AtomicBool::new(true);

struct TargetPidFile(Option<PathBuf>);

impl TargetPidFile {
    fn from_env() -> io::Result<Self> {
        let Some(path) = env::var_os("URING_PLAY_ZCNBLK_SHM_TARGET_PID_FILE") else {
            return Ok(Self(None));
        };
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, format!("{}\n", std::process::id()))?;
        Ok(Self(Some(path)))
    }
}

impl Drop for TargetPidFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.as_ref() {
            let _ = fs::remove_file(path);
        }
    }
}

extern "C" fn stop_handler(_: libc::c_int) {
    RUNNING.store(false, Ordering::Relaxed);
}

unsafe fn atomic_load(ptr: *const u64, ordering: Ordering) -> u64 {
    unsafe { (&*ptr.cast::<AtomicU64>()).load(ordering) }
}

unsafe fn atomic_store(ptr: *mut u64, value: u64, ordering: Ordering) {
    unsafe { (&*ptr.cast::<AtomicU64>()).store(value, ordering) };
}

unsafe fn atomic_swap(ptr: *mut u64, value: u64, ordering: Ordering) -> u64 {
    unsafe { (&*ptr.cast::<AtomicU64>()).swap(value, ordering) }
}

fn release_payload_owner_token(
    owner: &AtomicU64,
    free_slots: &AtomicU64,
    expected: u64,
) -> Result<(), u64> {
    owner
        .compare_exchange(expected, 0, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|actual| actual)?;
    free_slots.fetch_add(1, Ordering::Release);
    Ok(())
}

fn checked_offset(base: u64, index: u64, stride: u64, limit: usize) -> io::Result<usize> {
    let offset = index
        .checked_mul(stride)
        .and_then(|value| base.checked_add(value))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "shared layout overflow"))?;
    let offset = usize::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "shared offset too large"))?;
    if offset >= limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shared offset exceeds mapping",
        ));
    }
    Ok(offset)
}

fn wal_transport_owner(offset: u64, owner_count: usize, extent_records: u64) -> io::Result<usize> {
    if owner_count == 0 || extent_records == 0 || offset % 4096 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WAL transport ownership needs aligned I/O, owners, and extent records",
        ));
    }
    Ok(((offset / 4096 / extent_records) % owner_count as u64) as usize)
}

fn wal_owner_count(channels: u32) -> io::Result<usize> {
    let channels = channels as usize;
    let count = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_COUNT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(channels);
    if count == 0 || count > channels {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("stable WAL owner count must be in 1..={channels}, got {count}"),
        ));
    }
    Ok(count)
}

struct SharedTarget {
    file: File,
    mapping: Arc<Mapping>,
    header: ZcnblkShmHeader,
    transfer_payload_slots: bool,
    dirty_read_payload_refs: bool,
    subpage_reads: bool,
    backend: BackendMode,
    ram: Option<RamBacking>,
    wal_state: Option<WalWritebackState>,
    remote_leaves: Vec<RemoteWalLeaf>,
    remote_workers: Vec<RemoteWalWorker>,
    ready_heads: BinaryHeap<Reverse<(u64, u32, u64)>>,
    head_queued: Vec<bool>,
    next_submit_sequence: u64,
    lane_local_sequences: bool,
    lane_completed: Vec<u64>,
    next_ready_channel: u32,
    read_batch: usize,
    read_batch_fill_us: u64,
    read_batch_fill_min: usize,
    write_batch_fill_us: u64,
    write_batch_fill_min: usize,
    kick_batch: u64,
    poll_us: u64,
    busy_poll_us: u64,
    busy_hysteresis_us: u64,
    poll_clock_check_spins: u64,
    lease_release_batch: u64,
    owner_extent_records: u64,
    owner_max_tx_iovecs: usize,
    active_started: Option<Instant>,
    active_last: Option<Instant>,
    stats: Stats,
}

impl SharedTarget {
    fn valid_wal_data_request(&self, request: &ZcnblkShmRequest) -> bool {
        match request.op {
            ZCNBLK_SHM_OP_WRITE => {
                !self.subpage_reads && request.len == 4096 && request.offset % 4096 == 0
            }
            ZCNBLK_SHM_OP_READ => {
                request.len != 0
                    && request.len <= 4096
                    && request.len.is_power_of_two()
                    && request.len >= 512
                    // blk-mq may merge adjacent logical-block reads into a
                    // larger frame whose start is aligned to the negotiated
                    // logical block, not necessarily to the merged length.
                    && request.offset % 512 == 0
                    && (self.subpage_reads || request.len == 4096)
            }
            _ => false,
        }
    }

    fn first_touch_hugetlb_arena(mapping: &Mapping, header: &ZcnblkShmHeader) -> io::Result<()> {
        let Some(cpu_text) = env::var("URING_PLAY_ZCNBLK_SHM_ARENA_CPU_LIST")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            return first_touch_pages(mapping.ptr.cast(), mapping.len, 0);
        };
        let cpus = parse_cpu_list(&cpu_text)?;
        if cpus.len() != header.channels as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "URING_PLAY_ZCNBLK_SHM_ARENA_CPU_LIST must provide one CPU per channel: got {} for {} channels",
                    cpus.len(),
                    header.channels
                ),
            ));
        }
        let payload_offset = usize::try_from(header.payload_offset).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "payload offset exceeds usize")
        })?;
        let channel_payload_bytes = usize::try_from(header.payload_entries)
            .ok()
            .and_then(|entries| entries.checked_mul(header.slot_bytes as usize))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "channel payload size overflow")
            })?;
        let original_cpu = unsafe { libc::sched_getcpu() };
        if original_cpu < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pages_by_channel = vec![0usize; cpus.len()];
        for page_offset in (0..mapping.len).step_by(mapping.hugepage_bytes) {
            let page_len = mapping.hugepage_bytes.min(mapping.len - page_offset);
            let midpoint = page_offset.saturating_add(page_len / 2);
            let channel = midpoint
                .saturating_sub(payload_offset)
                .checked_div(channel_payload_bytes)
                .unwrap_or(0)
                .min(cpus.len() - 1);
            pin_current_thread(cpus[channel])?;
            first_touch_pages(unsafe { mapping.ptr.add(page_offset) }.cast(), page_len, 0)?;
            pages_by_channel[channel] += 1;
        }
        pin_current_thread(original_cpu as usize)?;
        eprintln!(
            "zcnblk-shm-target-arena-topology: policy=metadata-on-channel0+payload-channel-midpoint hugepage_bytes={} channel_to_cpu={} pages_by_channel={} metadata_prefix_bytes={} placement=memory-locality-only block_client_placement=no",
            mapping.hugepage_bytes,
            cpus.iter()
                .enumerate()
                .map(|(channel, cpu)| format!("{channel}:{cpu}"))
                .collect::<Vec<_>>()
                .join(","),
            pages_by_channel
                .iter()
                .enumerate()
                .map(|(channel, pages)| format!("{channel}:{pages}"))
                .collect::<Vec<_>>()
                .join(","),
            payload_offset,
        );
        Ok(())
    }

    fn import_hugetlb_mapping(file: &File, header: &mut ZcnblkShmHeader) -> io::Result<Mapping> {
        if header.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] & ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_IMPORT
            == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "zcnblk client does not support external HugeTLB arena import",
            ));
        }
        let hugepage_bytes = default_hugepage_size()?;
        if !hugepage_bytes.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "default HugeTLB page size is not a power of two",
            ));
        }
        let logical_bytes = usize::try_from(header.region_bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "shared region does not fit usize",
            )
        })?;
        let mapped_bytes = logical_bytes
            .checked_add(hugepage_bytes - 1)
            .map(|value| value & !(hugepage_bytes - 1))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "HugeTLB arena length overflow")
            })?;
        let raw_fd = unsafe {
            libc::memfd_create(
                b"zcnblk-shm-arena\0".as_ptr().cast(),
                ZCNBLK_MFD_CLOEXEC | ZCNBLK_MFD_ALLOW_SEALING | ZCNBLK_MFD_HUGETLB,
            )
        };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let arena_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let truncate_len = libc::off_t::try_from(mapped_bytes).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "HugeTLB arena exceeds off_t")
        })?;
        if unsafe { libc::ftruncate(arena_fd.as_raw_fd(), truncate_len) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let staging_ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mapped_bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                arena_fd.as_raw_fd(),
                0,
            )
        };
        if staging_ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let staging = Mapping {
            ptr: staging_ptr.cast(),
            len: mapped_bytes,
            backing: SharedArenaBacking::ExternalHugeTlb,
            hugepage_bytes,
            export_fd: None,
        };
        Self::first_touch_hugetlb_arena(&staging, header)?;
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        if unsafe { libc::fcntl(arena_fd.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let import = ZcnblkShmArenaImport {
            magic: ZCNBLK_SHM_MAGIC,
            version: ZCNBLK_SHM_VERSION,
            flags: ZCNBLK_SHM_ARENA_IMPORT_F_HUGETLB,
            fd: arena_fd.as_raw_fd(),
            reserved: 0,
            region_bytes: mapped_bytes as u64,
        };
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), ZCNBLK_SHM_IOC_IMPORT_ARENA, &import) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        drop(staging);

        let mut imported_header = ZcnblkShmHeader::default();
        let ret = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                ZCNBLK_SHM_IOC_GET_INFO,
                &mut imported_header,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Self::validate_header(&imported_header)?;
        if imported_header.region_bytes != mapped_bytes as u64
            || imported_header.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES]
                & ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_ACTIVE
                == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "kernel did not activate the imported HugeTLB arena",
            ));
        }
        *header = imported_header;
        let mut mapping = Mapping::map_control(
            file,
            mapped_bytes,
            SharedArenaBacking::ExternalHugeTlb,
            hugepage_bytes,
        )?;
        mapping.export_fd = Some(arena_fd);
        Ok(mapping)
    }

    fn open(
        path: &str,
        backend: BackendMode,
        kick_batch: u64,
        poll_us: u64,
        busy_poll_us: u64,
        busy_hysteresis_us: u64,
        lease_release_batch: u64,
    ) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut header = ZcnblkShmHeader::default();
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), ZCNBLK_SHM_IOC_GET_INFO, &mut header) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Self::validate_header(&header)?;
        let required_capabilities = ZCNBLK_SHM_CAP_REQUEST_WAKE_ARMED
            | ZCNBLK_SHM_CAP_COMPLETION_WAKE_ARMED
            | ZCNBLK_SHM_CAP_IO_CONTRACT_SIDECAR;
        if header.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] & required_capabilities
            != required_capabilities
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "zcnblk shared transport lacks armed wakeups or the I/O contract sidecar",
            ));
        }
        if header.reserved[ZCNBLK_SHM_HEADER_IO_FEATURES] != ZCNBLK_SHM_IO_FEATURE_ALL {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "zcnblk shared transport I/O feature mismatch expected={ZCNBLK_SHM_IO_FEATURE_ALL:#x} actual={:#x}",
                    header.reserved[ZCNBLK_SHM_HEADER_IO_FEATURES]
                ),
            ));
        }
        if lease_release_batch == 0 || lease_release_batch > u64::from(header.payload_entries) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "lease release batch {lease_release_batch} must be in 1..={} payload entries",
                    header.payload_entries
                ),
            ));
        }
        let arena_request = SharedArenaRequest::from_env()?;
        let arena_active = header.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES]
            & ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_ACTIVE
            != 0;
        if arena_active || arena_request != SharedArenaRequest::Vmalloc {
            if let Some(cpu) = env::var("URING_PLAY_ZCNBLK_SHM_ARENA_CPU")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            {
                pin_current_thread(cpu)?;
            }
        }
        let mapping = if arena_active {
            let len = usize::try_from(header.region_bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "shared region does not fit usize",
                )
            })?;
            Arc::new(Mapping::map_control(
                &file,
                len,
                SharedArenaBacking::ExternalHugeTlb,
                default_hugepage_size()?,
            )?)
        } else {
            match arena_request {
                SharedArenaRequest::HugeTlb => {
                    Arc::new(Self::import_hugetlb_mapping(&file, &mut header)?)
                }
                SharedArenaRequest::Auto => {
                    match Self::import_hugetlb_mapping(&file, &mut header) {
                        Ok(mapping) => Arc::new(mapping),
                        Err(error) => {
                            let mut current = ZcnblkShmHeader::default();
                            let ret = unsafe {
                                libc::ioctl(file.as_raw_fd(), ZCNBLK_SHM_IOC_GET_INFO, &mut current)
                            };
                            if ret >= 0
                                && current.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES]
                                    & ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_ACTIVE
                                    != 0
                            {
                                return Err(io::Error::other(format!(
                                    "HugeTLB arena import became active but remapping failed: {error}"
                                )));
                            }
                            zc_topology_issue(
                                "zcnblk-shm-target",
                                format!(
                                    "external HugeTLB arena auto-allocation failed ({error}); using the kernel vmalloc arena"
                                ),
                            )?;
                            let len = usize::try_from(header.region_bytes).map_err(|_| {
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "shared region does not fit usize",
                                )
                            })?;
                            Arc::new(Mapping::map_control(
                                &file,
                                len,
                                SharedArenaBacking::KernelVmalloc,
                                0,
                            )?)
                        }
                    }
                }
                SharedArenaRequest::Vmalloc => {
                    let len = usize::try_from(header.region_bytes).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "shared region does not fit usize",
                        )
                    })?;
                    Arc::new(Mapping::map_control(
                        &file,
                        len,
                        SharedArenaBacking::KernelVmalloc,
                        0,
                    )?)
                }
            }
        };
        eprintln!(
            "zcnblk-shm-target-shared-arena: requested={} backing={} region_bytes={} hugepage_bytes={} import_supported={} import_active={} bio_arena_alias_supported={} first_touch_cpu={}",
            arena_request.label(),
            mapping.backing.label(),
            mapping.len,
            mapping.hugepage_bytes,
            header.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES]
                & ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_IMPORT
                != 0,
            header.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES]
                & ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_ACTIVE
                != 0,
            header.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] & ZCNBLK_SHM_CAP_BIO_ARENA_ALIAS != 0,
            unsafe { libc::sched_getcpu() },
        );
        let app_arena_export_requested = env::var_os("URING_PLAY_ZCNBLK_SHM_APP_ARENA_SOCKET")
            .is_some_and(|value| !value.is_empty());
        if app_arena_export_requested
            && (mapping.backing != SharedArenaBacking::ExternalHugeTlb
                || mapping.export_fd.is_none())
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "application arena export requires a HugeTLB arena imported by this target",
            ));
        }
        let transfer_payload_slots = (backend == BackendMode::Memory && app_arena_export_requested)
            || (backend == BackendMode::WalTcp
                && env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH", false)
                && !env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_OWNER_DISPATCH", false)
                && env_enabled_or("URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS", true));
        if app_arena_export_requested && !transfer_payload_slots {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "application arena export requires transferable payload-slot ownership",
            ));
        }
        if transfer_payload_slots {
            if header.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES]
                & ZCNBLK_SHM_CAP_TRANSFER_PAYLOAD_SLOTS
                == 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "WAL payload ownership transfer requested but the zcnblk client lacks the capability",
                ));
            }
            let owner_offset = header.reserved[ZCNBLK_SHM_HEADER_PAYLOAD_OWNER_OFFSET];
            let owner_bytes = u64::from(header.channels)
                .checked_mul(u64::from(header.payload_entries))
                .and_then(|slots| slots.checked_mul(size_of::<u64>() as u64))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "payload owner table overflow")
                })?;
            if owner_offset == 0
                || owner_offset
                    .checked_add(owner_bytes)
                    .is_none_or(|end| end > header.payload_offset)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "payload owner table is outside the negotiated shared layout",
                ));
            }
        }
        let dirty_read_payload_refs = transfer_payload_slots
            && header.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] & ZCNBLK_SHM_CAP_READ_PAYLOAD_REF
                != 0;
        let subpage_reads = backend == BackendMode::WalTcp
            && env_enabled_or("URING_PLAY_ZCNBLK_SHM_SUBPAGE_READS", false);
        if subpage_reads {
            eprintln!(
                "zcnblk-shm-target-subpage: enabled=true minimum_bytes=512 maximum_bytes=4096 writes=disabled"
            );
        }
        if backend == BackendMode::WalMemory && header.channels != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "wal-memory currently requires exactly one channel",
            ));
        }
        if matches!(backend, BackendMode::WalMemory | BackendMode::WalTcp)
            && header.slot_bytes as usize % 4096 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WAL writeback requires 4K-aligned shared payload slots",
            ));
        }
        if backend == BackendMode::WalTcp && header.slot_bytes != 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "wal-tcp currently requires max_frame_bytes=4096",
            ));
        }
        let ram = if matches!(backend, BackendMode::Memory | BackendMode::WalMemory) {
            Some(RamBacking::new(
                usize::try_from(header.capacity_bytes).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "capacity does not fit usize")
                })?,
            )?)
        } else {
            None
        };
        let wal_state = if matches!(backend, BackendMode::WalMemory | BackendMode::WalTcp) {
            let source: Arc<dyn ZcnblkFanWalSharedLeaseSource> = mapping.clone();
            Some(WalWritebackState::new(
                source,
                header.capacity_bytes,
                header.channels,
                header.payload_entries,
                header.ring_entries,
            )?)
        } else {
            None
        };
        let remote_leaves = if backend == BackendMode::WalTcp {
            let owner_ingress = env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS", false);
            let stream_count = if owner_ingress {
                wal_owner_count(header.channels)?
            } else {
                header.channels as usize
            };
            let stream_count_u32 = u32::try_from(stream_count).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "WAL stream count exceeds u32")
            })?;
            let rma_read_buffer_bytes = header.slot_bytes as usize;
            let connect_handles = (0..stream_count_u32)
                .map(|lane| {
                    thread::spawn(move || {
                        RemoteWalLeaf::connect(lane, stream_count_u32, rma_read_buffer_bytes)
                    })
                })
                .collect::<Vec<_>>();
            let mut leaves = Vec::with_capacity(stream_count);
            for handle in connect_handles {
                leaves.push(
                    handle
                        .join()
                        .map_err(|_| io::Error::other("remote WAL leaf connect panicked"))??,
                );
            }
            leaves
        } else {
            Vec::new()
        };
        let lane_local_sequences = matches!(backend, BackendMode::Memory | BackendMode::WalTcp)
            && env_enabled_or("URING_PLAY_ZCNBLK_SHM_LANE_LOCAL_SEQUENCES", true);
        if lane_local_sequences
            && header.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] & ZCNBLK_SHM_CAP_LANE_LOCAL_SEQUENCE
                == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "lane-local sequencing requested but unsupported by zcnblk",
            ));
        }
        eprintln!(
            "zcnblk-shm-target-sequencing: mode={} request_tokens={} sync_boundary={}",
            if lane_local_sequences {
                "lane-local"
            } else {
                "global"
            },
            if lane_local_sequences {
                "lane-sequence+lane-id"
            } else {
                "global-sequence"
            },
            if lane_local_sequences {
                "admitted-lane-vector-hwm"
            } else {
                "global-completion-hwm"
            },
        );
        let target = Self {
            file,
            mapping,
            header,
            transfer_payload_slots,
            dirty_read_payload_refs,
            subpage_reads,
            backend,
            ram,
            wal_state,
            remote_leaves,
            remote_workers: Vec::new(),
            ready_heads: BinaryHeap::new(),
            head_queued: vec![false; header.channels as usize],
            next_submit_sequence: 1,
            lane_local_sequences,
            lane_completed: vec![0; header.channels as usize],
            next_ready_channel: 0,
            read_batch: env::var("URING_PLAY_ZCNBLK_SHM_READ_BATCH")
                .ok()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(header.ring_entries as usize)
                .clamp(1, header.ring_entries as usize),
            read_batch_fill_us: env::var("URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_US")
                .ok()
                .map(|value| value.parse::<u64>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(0),
            read_batch_fill_min: env::var("URING_PLAY_ZCNBLK_SHM_READ_BATCH_FILL_MIN")
                .ok()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(32)
                .clamp(1, header.ring_entries as usize),
            write_batch_fill_us: env::var("URING_PLAY_ZCNBLK_SHM_OWNER_WRITE_FILL_US")
                .ok()
                .map(|value| value.parse::<u64>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(
                    if env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_OWNER_DISPATCH", false) {
                        20
                    } else {
                        0
                    },
                ),
            write_batch_fill_min: env::var("URING_PLAY_ZCNBLK_SHM_OWNER_WRITE_FILL_MIN")
                .ok()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(256)
                .clamp(1, header.ring_entries as usize),
            kick_batch: kick_batch.max(1),
            poll_us,
            busy_poll_us: busy_poll_us.max(poll_us),
            busy_hysteresis_us: busy_hysteresis_us.max(busy_poll_us),
            poll_clock_check_spins: env::var("URING_PLAY_ZCNBLK_SHM_POLL_CLOCK_CHECK_SPINS")
                .ok()
                .map(|value| value.parse::<u64>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(64)
                .max(1),
            lease_release_batch,
            owner_extent_records: env::var("URING_PLAY_ZCNBLK_SHM_OWNER_EXTENT_RECORDS")
                .ok()
                .map(|value| value.parse::<u64>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(256)
                .max(1),
            owner_max_tx_iovecs: env::var("URING_PLAY_ZCNBLK_SHM_OWNER_MAX_TX_IOVECS")
                .ok()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(960)
                .clamp(1, 1_022),
            active_started: None,
            active_last: None,
            stats: Stats::default(),
        };
        target.validate_fresh_rings()?;
        let attach = ZcnblkShmAttach {
            magic: ZCNBLK_SHM_MAGIC,
            version: ZCNBLK_SHM_VERSION,
            flags: u32::from(transfer_payload_slots) * ZCNBLK_SHM_ATTACH_F_TRANSFER_PAYLOAD_SLOTS
                | u32::from(lane_local_sequences) * ZCNBLK_SHM_ATTACH_F_LANE_LOCAL_SEQUENCE,
        };
        let ret = unsafe { libc::ioctl(target.file.as_raw_fd(), ZCNBLK_SHM_IOC_ATTACH, &attach) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(target)
    }

    fn validate_header(header: &ZcnblkShmHeader) -> io::Result<()> {
        if header.magic != ZCNBLK_SHM_MAGIC
            || header.version != ZCNBLK_SHM_VERSION
            || header.descriptor_bytes != ZCNBLK_SHM_DESC_BYTES
            || header.header_bytes < size_of::<ZcnblkShmHeader>() as u32
            || header.channels == 0
            || header.ring_entries == 0
            || header.payload_entries == 0
            || header.slot_bytes == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported or invalid zcnblk shared-memory ABI",
            ));
        }
        let payload_slots = u64::from(header.channels)
            .checked_mul(u64::from(header.payload_entries))
            .and_then(|slots| slots.checked_mul(u64::from(header.slot_bytes)))
            .and_then(|bytes| header.payload_offset.checked_add(bytes))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "payload layout overflow"))?;
        if payload_slots > header.region_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "payload layout exceeds shared region",
            ));
        }
        let descriptor_slots = u64::from(header.channels)
            .checked_mul(u64::from(header.ring_entries))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "descriptor layout overflow")
            })?;
        let io_contract_end = descriptor_slots
            .checked_mul(size_of::<ZcnblkShmIoContract>() as u64)
            .and_then(|bytes| {
                header.reserved[ZCNBLK_SHM_HEADER_IO_CONTRACT_OFFSET].checked_add(bytes)
            })
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "I/O contract layout overflow")
            })?;
        if header.reserved[ZCNBLK_SHM_HEADER_IO_CONTRACT_OFFSET] == 0
            || io_contract_end > header.reserved[ZCNBLK_SHM_HEADER_PAYLOAD_OWNER_OFFSET]
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "I/O contract sidecar is outside the negotiated shared layout",
            ));
        }
        Ok(())
    }

    fn channel_ptr(&self, channel: u32) -> io::Result<*mut ZcnblkShmChannel> {
        if channel >= self.header.channels {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "channel out of range",
            ));
        }
        let offset = checked_offset(
            self.header.channel_offset,
            u64::from(channel),
            size_of::<ZcnblkShmChannel>() as u64,
            self.mapping.len,
        )?;
        Ok(unsafe { self.mapping.ptr.add(offset).cast() })
    }

    fn flush_admission_vector(&self, ordering_epoch: u64) -> io::Result<Box<[u64]>> {
        let mut tails = Vec::with_capacity(self.header.channels as usize);
        for channel in 0..self.header.channels {
            let control = self.channel_ptr(channel)?;
            let published_epoch = unsafe {
                atomic_load(
                    ptr::addr_of!(
                        (*control).request_producer_reserved[ZCNBLK_SHM_CHANNEL_FLUSH_EPOCH]
                    ),
                    Ordering::Acquire,
                )
            };
            if published_epoch < ordering_epoch {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "flush epoch {ordering_epoch} lacks channel {channel} admission vector: published_epoch={published_epoch}"
                    ),
                ));
            }
            let tail = unsafe {
                atomic_load(
                    ptr::addr_of!(
                        (*control).request_producer_reserved[ZCNBLK_SHM_CHANNEL_FLUSH_TAIL]
                    ),
                    Ordering::Acquire,
                )
            };
            tails.push(tail);
        }
        Ok(tails.into_boxed_slice())
    }

    fn request_ptr(&self, channel: u32, sequence: u64) -> io::Result<*mut ZcnblkShmRequest> {
        let index = u64::from(channel)
            .checked_mul(u64::from(self.header.ring_entries))
            .and_then(|base| base.checked_add(sequence % u64::from(self.header.ring_entries)))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request index overflow"))?;
        let offset = checked_offset(
            self.header.request_offset,
            index,
            size_of::<ZcnblkShmRequest>() as u64,
            self.mapping.len,
        )?;
        Ok(unsafe { self.mapping.ptr.add(offset).cast() })
    }

    fn completion_ptr(&self, channel: u32, sequence: u64) -> io::Result<*mut ZcnblkShmCompletion> {
        let index = u64::from(channel)
            .checked_mul(u64::from(self.header.ring_entries))
            .and_then(|base| base.checked_add(sequence % u64::from(self.header.ring_entries)))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "completion index overflow")
            })?;
        let offset = checked_offset(
            self.header.completion_offset,
            index,
            size_of::<ZcnblkShmCompletion>() as u64,
            self.mapping.len,
        )?;
        Ok(unsafe { self.mapping.ptr.add(offset).cast() })
    }

    fn io_contract(&self, channel: u32, sequence: u64) -> io::Result<ZcnblkWalIoContract> {
        let index = u64::from(channel)
            .checked_mul(u64::from(self.header.ring_entries))
            .and_then(|base| base.checked_add(sequence % u64::from(self.header.ring_entries)))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "I/O contract index overflow")
            })?;
        let offset = checked_offset(
            self.header.reserved[ZCNBLK_SHM_HEADER_IO_CONTRACT_OFFSET],
            index,
            size_of::<ZcnblkShmIoContract>() as u64,
            self.mapping.len,
        )?;
        let sidecar: ZcnblkShmIoContract =
            unsafe { ptr::read(self.mapping.ptr.add(offset).cast()) };
        sidecar.into_wal()
    }

    fn payload_ptr(&self, channel: u32, sequence: u64) -> io::Result<*mut u8> {
        let offset = self.payload_offset(channel, sequence)?;
        Ok(unsafe { self.mapping.ptr.add(offset) })
    }

    fn payload_offset(&self, channel: u32, sequence: u64) -> io::Result<usize> {
        self.payload_slot_offset(
            channel,
            (sequence % u64::from(self.header.payload_entries)) as u32,
        )
    }

    fn payload_slot_offset(&self, channel: u32, payload_slot: u32) -> io::Result<usize> {
        if payload_slot >= self.header.payload_entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "payload slot exceeds the channel arena",
            ));
        }
        let index = u64::from(channel)
            .checked_mul(u64::from(self.header.payload_entries))
            .and_then(|base| base.checked_add(u64::from(payload_slot)))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "payload index overflow"))?;
        checked_offset(
            self.header.payload_offset,
            index,
            u64::from(self.header.slot_bytes),
            self.mapping.len,
        )
    }

    fn payload_owner_ptr(&self, channel: u32, payload_slot: u32) -> io::Result<*mut u64> {
        if !self.transfer_payload_slots {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "payload owner table was not negotiated",
            ));
        }
        if channel >= self.header.channels || payload_slot >= self.header.payload_entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "payload owner channel or slot is out of range",
            ));
        }
        let index = u64::from(channel)
            .checked_mul(u64::from(self.header.payload_entries))
            .and_then(|base| base.checked_add(u64::from(payload_slot)))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "payload owner index overflow")
            })?;
        let offset = checked_offset(
            self.header.reserved[ZCNBLK_SHM_HEADER_PAYLOAD_OWNER_OFFSET],
            index,
            size_of::<u64>() as u64,
            self.mapping.len,
        )?;
        Ok(unsafe { self.mapping.ptr.add(offset).cast() })
    }

    fn request_payload_offset(
        &self,
        channel: u32,
        request_sequence: u64,
        request: &ZcnblkShmRequest,
    ) -> io::Result<usize> {
        if !self.transfer_payload_slots {
            let expected = (request_sequence % u64::from(self.header.payload_entries)) as u32;
            if request.payload_slot != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request payload slot does not match the legacy sequence ring",
                ));
            }
            return self.payload_offset(channel, request_sequence);
        }
        let owner = unsafe {
            atomic_load(
                self.payload_owner_ptr(channel, request.payload_slot)?,
                Ordering::Acquire,
            )
        };
        if owner != request.submit_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "transferred payload owner mismatch channel={channel} slot={} expected={} actual={owner}",
                    request.payload_slot, request.submit_sequence
                ),
            ));
        }
        self.payload_slot_offset(channel, request.payload_slot)
    }

    fn release_transferred_write_slot(
        &self,
        channel: u32,
        request: &ZcnblkShmRequest,
    ) -> io::Result<()> {
        if !self.transfer_payload_slots {
            return Ok(());
        }
        if request.op != ZCNBLK_SHM_OP_WRITE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "userspace may only return transferred write slots",
            ));
        }
        let owner = unsafe {
            &*self
                .payload_owner_ptr(channel, request.payload_slot)?
                .cast::<AtomicU64>()
        };
        let control = self.channel_ptr(channel)?;
        let free_slots =
            unsafe { &*ptr::addr_of_mut!((*control).payload_free_slots).cast::<AtomicU64>() };
        if request.flags & ZCNBLK_SHM_F_APP_PAYLOAD_ALIAS != 0 {
            owner
                .compare_exchange(
                    request.submit_sequence,
                    ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .map_err(|actual| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "application payload return mismatch channel={channel} slot={} expected={} actual={actual}",
                            request.payload_slot, request.submit_sequence
                        ),
                    )
                })?;
        } else {
            release_payload_owner_token(owner, free_slots, request.submit_sequence).map_err(|actual| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "transferred payload release mismatch channel={channel} slot={} expected={} actual={actual}",
                        request.payload_slot, request.submit_sequence
                    ),
                )
            })?;
        }
        Ok(())
    }

    fn transferred_free_slots(&self, channel: u32) -> io::Result<u64> {
        if !self.transfer_payload_slots {
            return Ok(0);
        }
        let control = self.channel_ptr(channel)?;
        Ok(unsafe {
            atomic_load(
                ptr::addr_of!((*control).payload_free_slots),
                Ordering::Acquire,
            )
        })
    }

    fn mark_payload_releasable(
        &self,
        releases: &mut WalLaneReleaseTracker,
        request_sequence: u64,
    ) -> io::Result<u64> {
        if self.transfer_payload_slots {
            // Physical slot tokens, rather than a contiguous request HWM, are
            // the reuse authority in transfer mode.
            Ok(releases.hwm)
        } else {
            releases.mark_releasable(request_sequence)
        }
    }

    fn validate_fresh_rings(&self) -> io::Result<()> {
        for channel in 0..self.header.channels {
            let control = unsafe { &*self.channel_ptr(channel)? };
            if control.req_prod != 0
                || control.req_cons != 0
                || control.comp_prod != 0
                || control.comp_cons != 0
                || control.payload_lease_hwm != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "shared rings are not fresh; reload zcnblk_client_mod before attaching a new daemon",
                ));
            }
        }
        Ok(())
    }

    fn head_request(&self, channel: u32) -> io::Result<Option<(u64, ZcnblkShmRequest)>> {
        let control = self.channel_ptr(channel)?;
        let consumed =
            unsafe { atomic_load(ptr::addr_of!((*control).req_cons), Ordering::Acquire) };
        let produced =
            unsafe { atomic_load(ptr::addr_of!((*control).req_prod), Ordering::Acquire) };
        if consumed == produced {
            return Ok(None);
        }
        if produced.wrapping_sub(consumed) > u64::from(self.header.ring_entries) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request ring overrun",
            ));
        }
        let request = self.request_ptr(channel, consumed)?;
        let sequence =
            unsafe { atomic_load(ptr::addr_of!((*request).sequence), Ordering::Acquire) };
        if sequence != consumed + 1 {
            return Ok(None);
        }
        Ok(Some((consumed, unsafe { ptr::read(request) })))
    }

    fn refresh_ready_heads(&mut self) -> io::Result<()> {
        for channel in 0..self.header.channels {
            if self.head_queued[channel as usize] {
                continue;
            }
            let Some((sequence, request)) = self.head_request(channel)? else {
                continue;
            };
            self.ready_heads
                .push(Reverse((request.submit_sequence, channel, sequence)));
            self.head_queued[channel as usize] = true;
        }
        Ok(())
    }

    fn next_request_at(
        &mut self,
        expected_submit_sequence: u64,
    ) -> io::Result<Option<(u32, u64, ZcnblkShmRequest)>> {
        self.refresh_ready_heads()?;
        let Some(Reverse((submit_sequence, channel, sequence))) = self.ready_heads.peek().copied()
        else {
            return Ok(None);
        };
        if submit_sequence != expected_submit_sequence {
            return Ok(None);
        }
        self.ready_heads.pop();
        self.head_queued[channel as usize] = false;
        let request_ptr = self.request_ptr(channel, sequence)?;
        let published =
            unsafe { atomic_load(ptr::addr_of!((*request_ptr).sequence), Ordering::Acquire) };
        if published != sequence + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cached WAL request head is no longer published",
            ));
        }
        let request = unsafe { ptr::read(request_ptr) };
        if request.submit_sequence != submit_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cached WAL request head changed submit sequence",
            ));
        }
        Ok(Some((channel, sequence, request)))
    }

    fn next_request(&mut self) -> io::Result<Option<(u32, u64, ZcnblkShmRequest)>> {
        if self.lane_local_sequences {
            return self.next_lane_local_request();
        }
        self.next_request_at(self.next_submit_sequence)
    }

    fn lane_token(&self, channel: u32, request_sequence: u64) -> io::Result<u64> {
        request_sequence
            .checked_mul(u64::from(self.header.channels))
            .and_then(|base| base.checked_add(u64::from(channel) + 1))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "lane token overflow"))
    }

    fn lane_token_complete(&self, token: u64) -> bool {
        lane_token_is_complete(&self.lane_completed, token)
    }

    fn lane_tracker_token_complete(&self, trackers: &[WalSharedLaneTracker], token: u64) -> bool {
        if token == 0 {
            return true;
        }
        let zero_based = token - 1;
        let channels = u64::from(self.header.channels);
        let channel = (zero_based % channels) as usize;
        let request_sequence = zero_based / channels;
        trackers
            .get(channel)
            .is_some_and(|tracker| tracker.is_complete(request_sequence + 1))
    }

    fn next_lane_local_request(&mut self) -> io::Result<Option<(u32, u64, ZcnblkShmRequest)>> {
        for relative in 0..self.header.channels {
            let channel = (self.next_ready_channel + relative) % self.header.channels;
            let Some((request_sequence, request)) = self.head_request(channel)? else {
                continue;
            };
            let expected_token = self.lane_token(channel, request_sequence)?;
            if request.submit_sequence != expected_token {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "lane-local token mismatch channel={channel} request_sequence={request_sequence} expected={expected_token} actual={}",
                        request.submit_sequence
                    ),
                ));
            }
            if !self.lane_token_complete(request.sector_predecessor) {
                continue;
            }
            if request.op == ZCNBLK_SHM_OP_SYNC {
                let ordering_epoch = request.ordering_epoch();
                if ordering_epoch == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "lane-local sync has reserved ordering epoch zero",
                    ));
                }
                let tails = self.flush_admission_vector(ordering_epoch)?;
                if tails
                    .iter()
                    .enumerate()
                    .any(|(lane, tail)| self.lane_completed[lane] < *tail)
                {
                    continue;
                }
            }
            self.next_ready_channel = (channel + 1) % self.header.channels;
            return Ok(Some((channel, request_sequence, request)));
        }
        Ok(None)
    }

    fn requeue_request_head(&mut self, channel: u32, sequence: u64, submit_sequence: u64) {
        self.ready_heads
            .push(Reverse((submit_sequence, channel, sequence)));
        self.head_queued[channel as usize] = true;
    }

    fn completion_has_capacity(&self, channel: u32) -> io::Result<bool> {
        let control = self.channel_ptr(channel)?;
        let produced =
            unsafe { atomic_load(ptr::addr_of!((*control).comp_prod), Ordering::Acquire) };
        let consumed =
            unsafe { atomic_load(ptr::addr_of!((*control).comp_cons), Ordering::Acquire) };
        Ok(produced.wrapping_sub(consumed) < u64::from(self.header.ring_entries))
    }

    fn completion_capacity(&self, channel: u32) -> io::Result<usize> {
        let control = self.channel_ptr(channel)?;
        let produced =
            unsafe { atomic_load(ptr::addr_of!((*control).comp_prod), Ordering::Acquire) };
        let consumed =
            unsafe { atomic_load(ptr::addr_of!((*control).comp_cons), Ordering::Acquire) };
        let used = produced.wrapping_sub(consumed);
        if used > u64::from(self.header.ring_entries) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "completion ring overrun",
            ));
        }
        Ok((u64::from(self.header.ring_entries) - used) as usize)
    }

    fn publish_completion(
        &mut self,
        channel: u32,
        request_sequence: u64,
        request: ZcnblkShmRequest,
        status: i16,
        committed_hwm: u64,
    ) -> io::Result<()> {
        let control = self.channel_ptr(channel)?;
        let completion_sequence =
            unsafe { atomic_load(ptr::addr_of!((*control).comp_prod), Ordering::Acquire) };
        let completion = self.completion_ptr(channel, completion_sequence)?;
        unsafe {
            ptr::write(
                completion,
                ZcnblkShmCompletion {
                    sequence: 0,
                    request_id: request.request_id,
                    offset: request.offset,
                    committed_hwm,
                    len: request.len,
                    lane: request.lane,
                    stream: request.stream,
                    payload_slot: request.payload_slot,
                    op: request.op,
                    status,
                    flags: 0,
                    request_sequence,
                },
            );
            atomic_store(
                ptr::addr_of_mut!((*completion).sequence),
                completion_sequence + 1,
                Ordering::Release,
            );
            atomic_store(
                ptr::addr_of_mut!((*control).comp_prod),
                completion_sequence + 1,
                Ordering::Release,
            );
            atomic_store(
                ptr::addr_of_mut!((*control).req_cons),
                request_sequence + 1,
                Ordering::Release,
            );
        }
        Ok(())
    }

    fn flush_wal_memory(&mut self, max_writes: usize) -> io::Result<()> {
        let Some(ram) = self.ram.as_ref() else {
            return Err(io::Error::other("wal-memory is missing its backing arena"));
        };
        let Some(state) = self.wal_state.as_mut() else {
            return Err(io::Error::other("wal-memory state is unavailable"));
        };
        let count = state.pending.len().min(max_writes);
        if count == 0 {
            return Ok(());
        }
        let mut committed_hwm = 0u64;
        for _ in 0..count {
            let write = state
                .pending
                .pop_front()
                .ok_or_else(|| io::Error::other("wal-memory pending queue underflow"))?;
            let payload = self.mapping.slice(write.payload_offset, write.len)?;
            let offset = usize::try_from(write.offset).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "wal-memory offset exceeds usize",
                )
            })?;
            unsafe {
                ptr::copy_nonoverlapping(payload.as_ptr(), ram.ptr.add(offset), write.len);
            }
            state.mark_releasable(write.request.queue_id, write.request_sequence)?;
            state.durable_submit_hwm = state.durable_submit_hwm.max(write.submit_sequence);
            committed_hwm = committed_hwm.max(write.submit_sequence.saturating_add(1));
            state.writeback_writes += 1;
            state.writeback_bytes += write.len as u64;
        }
        state.cache.commit_hwm(committed_hwm);
        state.writeback_batches += 1;
        Ok(())
    }

    fn flush_wal_tcp(&mut self, max_writes: usize) -> io::Result<()> {
        let count = self
            .wal_state
            .as_ref()
            .ok_or_else(|| io::Error::other("WAL writeback state is unavailable"))?
            .pending
            .len()
            .min(max_writes);
        if count == 0 {
            return Ok(());
        }
        let mut lane_batches = (0..self.remote_workers.len())
            .map(|_| VecDeque::<Vec<PendingWalWrite>>::new())
            .collect::<Vec<_>>();
        let mut lane_runs = vec![0usize; self.remote_workers.len()];
        {
            let state = self
                .wal_state
                .as_ref()
                .ok_or_else(|| io::Error::other("WAL writeback state is unavailable"))?;
            for write in state.pending.iter().take(count) {
                let owner = wal_transport_owner(
                    write.offset,
                    lane_batches.len(),
                    self.owner_extent_records,
                )?;
                let batches = lane_batches.get_mut(owner).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "WAL write references an unavailable transport owner",
                    )
                })?;
                let contiguous =
                    batches
                        .back()
                        .and_then(|batch| batch.last())
                        .is_some_and(|previous| {
                            previous.payload_offset.checked_add(previous.len)
                                == Some(write.payload_offset)
                        });
                if batches.is_empty()
                    || (!contiguous && lane_runs[owner] >= self.owner_max_tx_iovecs)
                {
                    batches.push_back(Vec::new());
                    lane_runs[owner] = 0;
                }
                if !contiguous {
                    lane_runs[owner] += 1;
                }
                batches
                    .back_mut()
                    .expect("owner transport batch was created")
                    .push(*write);
            }
        }
        let mut active_lanes = Vec::new();
        for (lane, batches) in lane_batches.iter_mut().enumerate() {
            if !batches.is_empty() {
                let window = batches.drain(..).collect::<Vec<_>>();
                self.remote_workers
                    .get(lane)
                    .ok_or_else(|| io::Error::other("remote WAL lane worker is unavailable"))?
                    .send(RemoteWalCommand::WriteWindow(window))?;
                active_lanes.push(lane);
            }
        }
        for lane in active_lanes {
            self.remote_workers[lane].wait()?;
        }

        let state = self
            .wal_state
            .as_mut()
            .ok_or_else(|| io::Error::other("WAL writeback state is unavailable"))?;
        let mut committed_hwm = 0u64;
        for _ in 0..count {
            let write = state
                .pending
                .pop_front()
                .ok_or_else(|| io::Error::other("WAL pending queue underflow"))?;
            state.mark_releasable(write.request.queue_id, write.request_sequence)?;
            state.durable_submit_hwm = state.durable_submit_hwm.max(write.submit_sequence);
            committed_hwm = committed_hwm.max(write.submit_sequence.saturating_add(1));
            state.writeback_writes += 1;
            state.writeback_bytes += write.len as u64;
        }
        state.cache.commit_hwm(committed_hwm);
        state.writeback_batches += 1;
        Ok(())
    }

    fn sync_remote_leaves(&mut self, submit_sequence: u64) -> io::Result<()> {
        for worker in &self.remote_workers {
            worker.send(RemoteWalCommand::Sync(submit_sequence))?;
        }
        for worker in &self.remote_workers {
            worker.wait()?;
        }
        Ok(())
    }

    fn start_remote_workers(&mut self) -> io::Result<()> {
        if self.backend != BackendMode::WalTcp || !self.remote_workers.is_empty() {
            return Ok(());
        }
        let leaves = std::mem::take(&mut self.remote_leaves);
        let mut workers = Vec::with_capacity(leaves.len());
        for remote in leaves {
            let worker = RemoteWalWorker::start(remote, Arc::clone(&self.mapping))?;
            eprintln!(
                "zcnblk-shm-target-owner-worker: lane={} cpu={} command_result_spins={} pipeline_batches={}",
                worker.lane_id,
                worker
                    .target_cpu
                    .map_or_else(|| "unpinned".to_string(), |cpu| cpu.to_string()),
                worker.wait_spins,
                worker.pipeline_batches,
            );
            workers.push(worker);
        }
        self.remote_workers = workers;
        Ok(())
    }

    fn stop_remote_workers(&mut self) -> io::Result<()> {
        if self.remote_workers.is_empty() {
            return Ok(());
        }
        for worker in &self.remote_workers {
            worker.send(RemoteWalCommand::Eof)?;
        }
        let mut first_error = None;
        for worker in &self.remote_workers {
            if let Err(err) = worker.wait()
                && first_error.is_none()
            {
                first_error = Some(err);
            }
        }
        let mut leaves = Vec::with_capacity(self.remote_workers.len());
        for worker in &mut self.remote_workers {
            let Some(handle) = worker.handle.take() else {
                continue;
            };
            match handle.join() {
                Ok(Ok(remote)) => leaves.push(remote),
                Ok(Err(err)) if first_error.is_none() => first_error = Some(err),
                Err(_) if first_error.is_none() => {
                    first_error = Some(io::Error::other("remote WAL lane worker panicked"));
                }
                _ => {}
            }
        }
        self.remote_workers.clear();
        leaves.sort_by_key(|remote| remote.lane_id);
        self.remote_leaves = leaves;
        if let Some(err) = first_error {
            return Err(err);
        }
        Ok(())
    }

    fn remote_read_batch(&self, channel: u32, reads: Vec<PendingRemoteRead>) -> io::Result<()> {
        let owner = reads
            .first()
            .map(|read| {
                wal_transport_owner(
                    read.request.offset,
                    self.remote_workers.len(),
                    self.owner_extent_records,
                )
            })
            .transpose()?
            .unwrap_or(channel as usize);
        let worker = self
            .remote_workers
            .get(owner)
            .ok_or_else(|| io::Error::other("remote WAL transport owner is unavailable"))?;
        worker.send(RemoteWalCommand::Read(reads))?;
        worker.wait()
    }

    fn remote_read_batches(&self, lane_reads: Vec<Vec<PendingRemoteRead>>) -> io::Result<()> {
        let mut active_lanes = Vec::new();
        for (lane, reads) in lane_reads.into_iter().enumerate() {
            if !reads.is_empty() {
                self.remote_workers
                    .get(lane)
                    .ok_or_else(|| io::Error::other("remote WAL lane worker is unavailable"))?
                    .send(RemoteWalCommand::Read(reads))?;
                active_lanes.push(lane);
            }
        }
        for lane in active_lanes {
            self.remote_workers[lane].wait()?;
        }
        Ok(())
    }

    fn flush_wal_backend(&mut self, max_writes: usize) -> io::Result<()> {
        match self.backend {
            BackendMode::WalMemory => self.flush_wal_memory(max_writes),
            BackendMode::WalTcp => self.flush_wal_tcp(max_writes),
            _ => Err(io::Error::other(
                "WAL writeback flush requested for a non-WAL backend",
            )),
        }
    }

    fn process_wal_request(
        &mut self,
        channel: u32,
        request_sequence: u64,
        request: ZcnblkShmRequest,
        payload: *mut u8,
    ) -> io::Result<u64> {
        let io_contract = self.io_contract(channel, request_sequence)?;
        self.stats.note_io_contract(io_contract);
        if io_contract.fua && self.backend != BackendMode::WalTcp {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "FUA requires a remote WAL leaf with an acknowledged sync contract",
            ));
        }
        let previous_payload_hwm = self
            .wal_state
            .as_ref()
            .ok_or_else(|| io::Error::other("wal-memory state is unavailable"))?
            .payload_hwm(channel)?;
        if request.op != ZCNBLK_SHM_OP_SYNC
            && (request.len == 0 || request.len % 4096 != 0 || request.offset % 4096 != 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "wal-memory requires 4K-aligned non-empty I/O, got offset={} len={}",
                    request.offset, request.len
                ),
            ));
        }
        match request.op {
            ZCNBLK_SHM_OP_WRITE => {
                let payload_offset = self.payload_offset(channel, request_sequence)?;
                let state = self
                    .wal_state
                    .as_mut()
                    .ok_or_else(|| io::Error::other("wal-memory state is unavailable"))?;
                for chunk_offset in (0..request.len as usize).step_by(4096) {
                    let logical_page = request.offset / 4096 + (chunk_offset / 4096) as u64;
                    state.cache.admit(
                        logical_page,
                        payload_offset.checked_add(chunk_offset).ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "wal-memory payload offset overflow",
                            )
                        })?,
                        4096,
                        request.submit_sequence,
                    )?;
                }
                state.pending.push_back(PendingWalWrite {
                    request,
                    io_contract,
                    request_sequence,
                    submit_sequence: request.submit_sequence,
                    offset: request.offset,
                    len: request.len as usize,
                    payload_offset,
                });
                self.stats.early_write_acks += u64::from(!io_contract.fua);
                if state.pending.len() >= state.writeback_batch {
                    let batch = state.writeback_batch;
                    self.flush_wal_backend(batch)?;
                }
            }
            ZCNBLK_SHM_OP_READ => {
                let out = unsafe { std::slice::from_raw_parts_mut(payload, request.len as usize) };
                if self.backend == BackendMode::WalTcp {
                    if !self.valid_wal_data_request(&request) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "wal-tcp request violates its negotiated data-unit contract",
                        ));
                    }
                    let dirty = self
                        .wal_state
                        .as_ref()
                        .ok_or_else(|| io::Error::other("WAL state is unavailable"))?
                        .cache
                        .read_payload(request.offset / 4096, 4096)?;
                    if let Some(dirty) = dirty {
                        out.copy_from_slice(dirty);
                    } else {
                        self.remote_read_batch(
                            channel,
                            vec![PendingRemoteRead {
                                request,
                                io_contract,
                                request_sequence,
                                payload_offset: self.payload_offset(channel, request_sequence)?,
                                dirty_ref: None,
                            }],
                        )?;
                    }
                } else {
                    let ram = self.ram.as_ref().ok_or_else(|| {
                        io::Error::other("wal-memory is missing its backing arena")
                    })?;
                    let state = self
                        .wal_state
                        .as_ref()
                        .ok_or_else(|| io::Error::other("WAL state is unavailable"))?;
                    for chunk_offset in (0..request.len as usize).step_by(4096) {
                        let logical_page = request.offset / 4096 + (chunk_offset / 4096) as u64;
                        if let Some(dirty) = state.cache.read_payload(logical_page, 4096)? {
                            out[chunk_offset..chunk_offset + 4096].copy_from_slice(dirty);
                        } else {
                            let source_offset = usize::try_from(request.offset)
                                .ok()
                                .and_then(|offset| offset.checked_add(chunk_offset))
                                .ok_or_else(|| {
                                    io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "wal-memory read offset exceeds usize",
                                    )
                                })?;
                            unsafe {
                                ptr::copy_nonoverlapping(
                                    ram.ptr.add(source_offset),
                                    out[chunk_offset..].as_mut_ptr(),
                                    4096,
                                );
                            }
                        }
                    }
                }
                self.wal_state
                    .as_mut()
                    .ok_or_else(|| io::Error::other("WAL state is unavailable"))?
                    .mark_releasable(channel, request_sequence)?;
            }
            ZCNBLK_SHM_OP_SYNC => {
                self.flush_wal_backend(usize::MAX)?;
                if self.backend == BackendMode::WalTcp {
                    self.sync_remote_leaves(request.submit_sequence)?;
                }
                self.wal_state
                    .as_mut()
                    .ok_or_else(|| io::Error::other("wal-memory state is unavailable"))?
                    .mark_releasable(channel, request_sequence)?;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "wal-memory request op is unsupported",
                ));
            }
        }
        let near_payload_limit = {
            let state = self
                .wal_state
                .as_ref()
                .ok_or_else(|| io::Error::other("wal-memory state is unavailable"))?;
            let reserve = u64::from(self.header.ring_entries).max(2);
            let limit = u64::from(self.header.payload_entries)
                .saturating_sub(reserve)
                .max(1);
            request_sequence
                .saturating_add(1)
                .saturating_sub(state.payload_hwm(channel)?)
                >= limit
                && !state.pending.is_empty()
        };
        if near_payload_limit {
            self.flush_wal_backend(usize::MAX)?;
        }
        let (payload_hwm, durable_submit_hwm) = {
            let state = self
                .wal_state
                .as_ref()
                .ok_or_else(|| io::Error::other("wal-memory state is unavailable"))?;
            (state.payload_hwm(channel)?, state.durable_submit_hwm)
        };
        let control = self.channel_ptr(channel)?;
        unsafe {
            atomic_store(
                ptr::addr_of_mut!((*control).payload_lease_hwm),
                payload_hwm,
                Ordering::Release,
            );
        }
        if payload_hwm != previous_payload_hwm {
            self.stats.lease_releases += 1;
        }
        Ok(durable_submit_hwm)
    }

    fn wal_tcp_batch_entry(
        &self,
        channel: u32,
        request_sequence: u64,
        request: ZcnblkShmRequest,
        channel_count: usize,
        channel_capacity: usize,
    ) -> io::Result<Option<PendingRemoteRead>> {
        if self.lane_local_sequences {
            let expected = self.lane_completed[channel as usize]
                .checked_add(channel_count as u64)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "lane completion HWM overflow")
                })?;
            if request_sequence != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "batched lane request order mismatch channel={channel} expected={expected} actual={request_sequence}"
                    ),
                ));
            }
        }
        if request.queue_id != channel
            || request.payload_slot
                != (request_sequence % u64::from(self.header.payload_entries)) as u32
            || !self.valid_wal_data_request(&request)
            || request
                .offset
                .checked_add(u64::from(request.len))
                .is_none_or(|end| end > self.header.capacity_bytes)
            || !matches!(request.op, ZCNBLK_SHM_OP_READ | ZCNBLK_SHM_OP_WRITE)
            || channel_count >= channel_capacity
        {
            return Ok(None);
        }
        Ok(Some(PendingRemoteRead {
            request,
            io_contract: self.io_contract(channel, request_sequence)?,
            request_sequence,
            payload_offset: self.payload_offset(channel, request_sequence)?,
            dirty_ref: None,
        }))
    }

    fn process_wal_tcp_request_batch(
        &mut self,
        first_channel: u32,
        first_sequence: u64,
        first_request: ZcnblkShmRequest,
    ) -> io::Result<Vec<usize>> {
        let channels = self.header.channels as usize;
        let window_limit = self.read_batch.saturating_mul(channels).max(1);
        // A full completion ring is backpressure, not a malformed request
        // window.  The single-request path already waits here; do the same
        // before taking the batched capacity snapshot so a producer that
        // refills immediately after a kick cannot terminate the target.
        while !self.completion_has_capacity(first_channel)? && RUNNING.load(Ordering::Relaxed) {
            self.kick(first_channel)?;
            std::hint::spin_loop();
        }
        if !RUNNING.load(Ordering::Relaxed) {
            return Ok(vec![0; channels]);
        }
        let capacities = (0..self.header.channels)
            .map(|channel| self.completion_capacity(channel))
            .collect::<io::Result<Vec<_>>>()?;
        let mut per_channel = vec![0usize; channels];
        let mut requests = Vec::with_capacity(window_limit);
        let (fill_us, fill_min) = if first_request.op == ZCNBLK_SHM_OP_WRITE {
            (self.write_batch_fill_us, self.write_batch_fill_min)
        } else {
            (self.read_batch_fill_us, self.read_batch_fill_min)
        };
        let fill_target = fill_min.min(window_limit).max(1);
        let fill_deadline = Instant::now()
            .checked_add(Duration::from_micros(fill_us))
            .unwrap_or_else(Instant::now);
        loop {
            requests.clear();
            per_channel.fill(0);
            let mut available = vec![None::<(u32, u64)>; window_limit];
            for channel in 0..self.header.channels {
                let control = self.channel_ptr(channel)?;
                let consumed =
                    unsafe { atomic_load(ptr::addr_of!((*control).req_cons), Ordering::Acquire) };
                let produced =
                    unsafe { atomic_load(ptr::addr_of!((*control).req_prod), Ordering::Acquire) };
                let count = usize::try_from(produced.wrapping_sub(consumed))
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "request window overflow")
                    })?
                    .min(capacities[channel as usize]);
                for idx in 0..count {
                    let sequence = consumed + idx as u64;
                    let request_ptr = self.request_ptr(channel, sequence)?;
                    let published = unsafe {
                        atomic_load(ptr::addr_of!((*request_ptr).sequence), Ordering::Acquire)
                    };
                    if published != sequence + 1 {
                        break;
                    }
                    let request = unsafe { ptr::read(request_ptr) };
                    let Some(relative) = request
                        .submit_sequence
                        .checked_sub(self.next_submit_sequence)
                        .and_then(|relative| usize::try_from(relative).ok())
                    else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "WAL request precedes the next global submit sequence",
                        ));
                    };
                    if relative >= window_limit {
                        continue;
                    }
                    if available[relative].replace((channel, sequence)).is_some() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "duplicate global submit sequence in WAL request window",
                        ));
                    }
                }
            }
            for entry in available {
                let Some((channel, sequence)) = entry else {
                    break;
                };
                let request = unsafe { ptr::read(self.request_ptr(channel, sequence)?) };
                let Some(pending) = self.wal_tcp_batch_entry(
                    channel,
                    sequence,
                    request,
                    per_channel[channel as usize],
                    capacities[channel as usize],
                )?
                else {
                    break;
                };
                if requests.is_empty()
                    && (channel != first_channel
                        || sequence != first_sequence
                        || request.submit_sequence != first_request.submit_sequence)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "WAL global window did not begin with the selected request",
                    ));
                }
                requests.push((channel, pending));
                per_channel[channel as usize] += 1;
            }
            if fill_us == 0 || requests.len() >= fill_target || Instant::now() >= fill_deadline {
                break;
            }
            std::hint::spin_loop();
        }
        self.ready_heads.clear();
        self.head_queued.fill(false);
        if requests.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WAL TCP global request window is empty",
            ));
        }

        let now = Instant::now();
        self.active_started.get_or_insert(now);
        self.active_last = Some(now);

        let mut remote_reads = vec![Vec::new(); self.remote_workers.len()];
        let mut last_sequence_by_channel = vec![None::<u64>; channels];
        for (channel, pending) in &requests {
            let request = pending.request;
            self.stats.note_io_contract(pending.io_contract);
            last_sequence_by_channel[*channel as usize] = Some(pending.request_sequence);
            if request.op == ZCNBLK_SHM_OP_WRITE {
                let state = self
                    .wal_state
                    .as_mut()
                    .ok_or_else(|| io::Error::other("WAL state is unavailable"))?;
                state.cache.admit(
                    request.offset / 4096,
                    pending.payload_offset,
                    4096,
                    request.submit_sequence,
                )?;
                state.pending.push_back(PendingWalWrite {
                    request,
                    io_contract: pending.io_contract,
                    request_sequence: pending.request_sequence,
                    submit_sequence: request.submit_sequence,
                    offset: request.offset,
                    len: request.len as usize,
                    payload_offset: pending.payload_offset,
                });
                self.stats.writes += 1;
                self.stats.write_bytes += u64::from(request.len);
                self.stats.early_write_acks += u64::from(!pending.io_contract.fua);
            } else {
                let dirty = self
                    .wal_state
                    .as_ref()
                    .ok_or_else(|| io::Error::other("WAL state is unavailable"))?
                    .cache
                    .read_payload(request.offset / 4096, 4096)?;
                if let Some(dirty) = dirty {
                    unsafe {
                        ptr::copy_nonoverlapping(
                            dirty.as_ptr(),
                            self.mapping.ptr.add(pending.payload_offset),
                            4096,
                        );
                    }
                } else {
                    let owner = wal_transport_owner(
                        request.offset,
                        remote_reads.len(),
                        self.owner_extent_records,
                    )?;
                    remote_reads[owner].push(*pending);
                }
                self.stats.reads += 1;
                self.stats.read_bytes += u64::from(request.len);
            }
        }
        self.remote_read_batches(remote_reads)?;
        let should_flush = {
            let state = self
                .wal_state
                .as_ref()
                .ok_or_else(|| io::Error::other("WAL state is unavailable"))?;
            let reserve = u64::from(self.header.ring_entries).max(2);
            let payload_limit = u64::from(self.header.payload_entries)
                .saturating_sub(reserve)
                .max(1);
            let payload_hwm = (0..self.header.channels)
                .map(|channel| state.payload_hwm(channel))
                .collect::<io::Result<Vec<_>>>()?;
            requests.iter().any(|(_, request)| request.io_contract.fua)
                || state.pending.len() >= state.writeback_batch
                || (!state.pending.is_empty()
                    && last_sequence_by_channel
                        .iter()
                        .enumerate()
                        .any(|(channel, sequence)| {
                            sequence.is_some_and(|sequence| {
                                sequence
                                    .saturating_add(1)
                                    .saturating_sub(payload_hwm[channel])
                                    >= payload_limit
                            })
                        }))
        };
        if should_flush {
            self.flush_wal_backend(usize::MAX)?;
        }

        let (previous_payload_hwm, payload_hwm, committed_hwm) = {
            let state = self
                .wal_state
                .as_mut()
                .ok_or_else(|| io::Error::other("WAL state is unavailable"))?;
            let previous_payload_hwm = (0..self.header.channels)
                .map(|channel| state.payload_hwm(channel))
                .collect::<io::Result<Vec<_>>>()?;
            for (channel, pending) in &requests {
                if pending.request.op == ZCNBLK_SHM_OP_READ {
                    state.mark_releasable(*channel, pending.request_sequence)?;
                }
            }
            (
                previous_payload_hwm,
                (0..self.header.channels)
                    .map(|channel| state.payload_hwm(channel))
                    .collect::<io::Result<Vec<_>>>()?,
                state.durable_submit_hwm,
            )
        };
        for channel in 0..self.header.channels {
            let control = self.channel_ptr(channel)?;
            unsafe {
                atomic_store(
                    ptr::addr_of_mut!((*control).payload_lease_hwm),
                    payload_hwm[channel as usize],
                    Ordering::Release,
                );
            }
            if payload_hwm[channel as usize] != previous_payload_hwm[channel as usize] {
                self.stats.lease_releases += 1;
            }
        }
        for (channel, pending) in &requests {
            self.stats.requests += 1;
            self.publish_completion(
                *channel,
                pending.request_sequence,
                pending.request,
                0,
                committed_hwm,
            )?;
        }
        if self.lane_local_sequences {
            advance_lane_completion_counts(&mut self.lane_completed, &per_channel)?;
        }
        self.next_submit_sequence += requests.len() as u64;
        Ok(per_channel)
    }

    fn process_one(
        &mut self,
        channel: u32,
        request_sequence: u64,
        request: ZcnblkShmRequest,
    ) -> io::Result<()> {
        let now = Instant::now();
        self.active_started.get_or_insert(now);
        self.active_last = Some(now);
        if request.queue_id != channel
            || (!self.transfer_payload_slots
                && request.payload_slot
                    != (request_sequence % u64::from(self.header.payload_entries)) as u32)
            || request.len > self.header.slot_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request descriptor topology or payload slot mismatch",
            ));
        }
        while !self.completion_has_capacity(channel)? && RUNNING.load(Ordering::Relaxed) {
            self.kick(channel)?;
            std::hint::spin_loop();
        }
        let payload_offset = self.request_payload_offset(channel, request_sequence, &request)?;
        let payload = unsafe { self.mapping.ptr.add(payload_offset) };
        let end = request
            .offset
            .checked_add(u64::from(request.len))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request range overflow"))?;
        let mut status = 0i16;
        let mut committed_hwm = request.submit_sequence;
        if end > self.header.capacity_bytes {
            status = -(libc::EINVAL as i16);
        } else if self.backend.is_wal_writeback() {
            match request.op {
                ZCNBLK_SHM_OP_WRITE => {
                    self.stats.writes += 1;
                    self.stats.write_bytes += u64::from(request.len);
                }
                ZCNBLK_SHM_OP_READ => {
                    self.stats.reads += 1;
                    self.stats.read_bytes += u64::from(request.len);
                }
                ZCNBLK_SHM_OP_SYNC => {
                    self.stats.syncs += 1;
                    if !self.backend.can_ack_block_sync() {
                        status = -(libc::EOPNOTSUPP as i16);
                    }
                }
                _ => status = -(libc::EOPNOTSUPP as i16),
            }
            if status == 0 {
                committed_hwm =
                    self.process_wal_request(channel, request_sequence, request, payload)?;
            }
        } else {
            match request.op {
                ZCNBLK_SHM_OP_WRITE => {
                    self.stats.writes += 1;
                    self.stats.write_bytes += u64::from(request.len);
                    if let Some(ram) = self.ram.as_ref() {
                        let offset = usize::try_from(request.offset).map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidData, "RAM offset too large")
                        })?;
                        unsafe {
                            ptr::copy_nonoverlapping(
                                payload,
                                ram.ptr.add(offset),
                                request.len as usize,
                            );
                        }
                    }
                }
                ZCNBLK_SHM_OP_READ => {
                    self.stats.reads += 1;
                    self.stats.read_bytes += u64::from(request.len);
                    if let Some(ram) = self.ram.as_ref() {
                        let offset = usize::try_from(request.offset).map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidData, "RAM offset too large")
                        })?;
                        unsafe {
                            ptr::copy_nonoverlapping(
                                ram.ptr.add(offset),
                                payload,
                                request.len as usize,
                            );
                        }
                    } else {
                        unsafe {
                            ptr::write_bytes(payload, 0, request.len as usize);
                        }
                    }
                }
                ZCNBLK_SHM_OP_SYNC => {
                    self.stats.syncs += 1;
                    status = -(libc::EOPNOTSUPP as i16);
                }
                _ => status = -(libc::EOPNOTSUPP as i16),
            }
        }
        if self.backend.is_wal_writeback() && status != 0 {
            self.wal_state
                .as_mut()
                .ok_or_else(|| io::Error::other("wal-memory state is unavailable"))?
                .mark_releasable(channel, request_sequence)?;
        }

        if self.transfer_payload_slots && request.op == ZCNBLK_SHM_OP_WRITE {
            self.release_transferred_write_slot(channel, &request)?;
        }
        self.stats.requests += 1;
        self.publish_completion(channel, request_sequence, request, status, committed_hwm)?;
        if !self.backend.is_wal_writeback()
            && self.should_release_payload(request_sequence, request.op)
        {
            let control = self.channel_ptr(channel)?;
            unsafe {
                atomic_store(
                    ptr::addr_of_mut!((*control).payload_lease_hwm),
                    request_sequence + 1,
                    Ordering::Release,
                );
            }
        }
        if !self.backend.is_wal_writeback()
            && self.should_release_payload(request_sequence, request.op)
        {
            self.stats.lease_releases += 1;
        }
        if self.lane_local_sequences {
            let completed = &mut self.lane_completed[channel as usize];
            if *completed != request_sequence {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "lane completion order mismatch channel={channel} expected={} actual={request_sequence}",
                        *completed
                    ),
                ));
            }
            *completed += 1;
        }
        // Batched data requests index their global token window from this
        // cursor even when lane-local sequencing is negotiated.  A sync or
        // unsupported descriptor processed through this scalar path still
        // consumes exactly one global token.
        self.next_submit_sequence += 1;
        Ok(())
    }

    fn should_release_payload(&self, request_sequence: u64, op: u16) -> bool {
        op == ZCNBLK_SHM_OP_SYNC || (request_sequence + 1) % self.lease_release_batch == 0
    }

    fn release_channel_payloads(&self, channel: u32) -> io::Result<()> {
        let control = self.channel_ptr(channel)?;
        let consumed =
            unsafe { atomic_load(ptr::addr_of!((*control).req_cons), Ordering::Acquire) };
        unsafe {
            atomic_store(
                ptr::addr_of_mut!((*control).payload_lease_hwm),
                consumed,
                Ordering::Release,
            );
        }
        Ok(())
    }

    fn kick(&mut self, channel: u32) -> io::Result<()> {
        if self.kick_channel(channel)? {
            self.stats.kicks += 1;
        }
        Ok(())
    }

    fn poll_for_requests(&mut self, timeout_ms: i32) -> io::Result<()> {
        for channel in 0..self.header.channels {
            let control = self.channel_ptr(channel)?;
            unsafe {
                atomic_store(
                    ptr::addr_of_mut!((*control).request_wake_armed),
                    1,
                    Ordering::Release,
                );
            }
        }
        let mut ready = false;
        for channel in 0..self.header.channels {
            if self.channel_request_ready(channel)? {
                ready = true;
                break;
            }
        }
        if ready {
            for channel in 0..self.header.channels {
                let control = self.channel_ptr(channel)?;
                unsafe {
                    atomic_store(
                        ptr::addr_of_mut!((*control).request_wake_armed),
                        0,
                        Ordering::Release,
                    );
                }
            }
            return Ok(());
        }
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        for channel in 0..self.header.channels {
            let control = self.channel_ptr(channel)?;
            unsafe {
                atomic_store(
                    ptr::addr_of_mut!((*control).request_wake_armed),
                    0,
                    Ordering::Release,
                );
            }
        }
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
        self.stats.idle_polls += 1;
        Ok(())
    }

    fn kick_channel(&self, channel: u32) -> io::Result<bool> {
        let control = self.channel_ptr(channel)?;
        let armed = unsafe {
            atomic_swap(
                ptr::addr_of_mut!((*control).completion_wake_armed),
                0,
                Ordering::AcqRel,
            )
        };
        if armed == 0 {
            return Ok(false);
        }
        let ret = unsafe { libc::ioctl(self.file.as_raw_fd(), ZCNBLK_SHM_IOC_KICK, &channel) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(true)
    }

    fn force_kick_channel(&self, channel: u32) -> io::Result<()> {
        let control = self.channel_ptr(channel)?;
        unsafe {
            atomic_store(
                ptr::addr_of_mut!((*control).completion_wake_armed),
                0,
                Ordering::Release,
            );
        }
        let ret = unsafe { libc::ioctl(self.file.as_raw_fd(), ZCNBLK_SHM_IOC_KICK, &channel) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn channel_request_ready(&self, channel: u32) -> io::Result<bool> {
        let control = self.channel_ptr(channel)?;
        let consumed =
            unsafe { atomic_load(ptr::addr_of!((*control).req_cons), Ordering::Acquire) };
        let produced =
            unsafe { atomic_load(ptr::addr_of!((*control).req_prod), Ordering::Acquire) };
        Ok(consumed != produced)
    }

    fn publish_wal_lane_completion(
        &self,
        channel: u32,
        pending: &PendingRemoteRead,
        committed_hwm: u64,
    ) -> io::Result<u64> {
        let control = self.channel_ptr(channel)?;
        let request = pending.request;
        let (payload_slot, flags) =
            pending
                .dirty_ref
                .map_or((request.payload_slot, 0), |dirty_ref| {
                    (
                        dirty_ref.payload_slot,
                        ZCNBLK_SHM_CQE_F_READ_PAYLOAD_REF
                            | (dirty_ref.source_channel << ZCNBLK_SHM_CQE_REF_CHANNEL_SHIFT),
                    )
                });
        let completion_sequence =
            unsafe { atomic_load(ptr::addr_of!((*control).comp_prod), Ordering::Acquire) };
        let completion = self.completion_ptr(channel, completion_sequence)?;
        unsafe {
            ptr::write(
                completion,
                ZcnblkShmCompletion {
                    sequence: 0,
                    request_id: request.request_id,
                    offset: request.offset,
                    committed_hwm,
                    len: request.len,
                    lane: request.lane,
                    stream: request.stream,
                    payload_slot,
                    op: request.op,
                    status: 0,
                    flags,
                    request_sequence: pending.request_sequence,
                },
            );
            atomic_store(
                ptr::addr_of_mut!((*completion).sequence),
                completion_sequence + 1,
                Ordering::Release,
            );
            atomic_store(
                ptr::addr_of_mut!((*control).comp_prod),
                completion_sequence + 1,
                Ordering::Release,
            );
        }
        Ok(completion_sequence + 1)
    }

    fn release_consumed_wal_read_refs(
        &self,
        channel: u32,
        outstanding: &mut VecDeque<OutstandingWalDirtyReadRef>,
        dirty: &WalConcurrentDirtyCache,
    ) -> io::Result<usize> {
        let control = self.channel_ptr(channel)?;
        let consumed =
            unsafe { atomic_load(ptr::addr_of!((*control).comp_cons), Ordering::Acquire) };
        let mut released = 0usize;
        while outstanding
            .front()
            .is_some_and(|entry| entry.completion_marker <= consumed)
        {
            let entry = outstanding.pop_front().expect("read ref queue was checked");
            dirty.release_ref(entry.dirty_ref)?;
            released += 1;
        }
        Ok(released)
    }

    fn flush_wal_lane_completions(
        &self,
        channel: u32,
        lane_completions: &mut WalLaneCompletionTracker,
        completions: &WalCompletionTracker,
        lane_tracker: &WalSharedLaneTracker,
        scratch: &mut Vec<PendingRemoteRead>,
        outstanding_read_refs: &mut VecDeque<OutstandingWalDirtyReadRef>,
    ) -> io::Result<usize> {
        scratch.clear();
        let capacity = usize::try_from(self.completion_capacity(channel)?).unwrap_or(usize::MAX);
        while scratch.len() < capacity {
            let Some(pending) = lane_completions.pop_ready() else {
                break;
            };
            scratch.push(pending);
        }
        if scratch.is_empty() {
            return Ok(0);
        }
        let committed_hwm = if self.lane_local_sequences {
            lane_tracker
                .mark_complete_batch(scratch.iter().map(|pending| pending.request_sequence + 1))?;
            0
        } else {
            completions.mark_complete_batch(
                scratch
                    .iter()
                    .map(|pending| pending.request.submit_sequence),
            )?
        };
        for pending in scratch.iter() {
            let published_hwm = if self.lane_local_sequences {
                pending.request.submit_sequence
            } else {
                committed_hwm
            };
            let completion_marker =
                self.publish_wal_lane_completion(channel, &pending, published_hwm)?;
            if let Some(dirty_ref) = pending.dirty_ref {
                outstanding_read_refs.push_back(OutstandingWalDirtyReadRef {
                    completion_marker,
                    dirty_ref,
                });
            }
        }
        Ok(scratch.len())
    }

    fn release_wal_lane_retained(
        &self,
        channel: u32,
        retained: &mut VecDeque<PendingRemoteRead>,
        releases: &mut WalLaneReleaseTracker,
        dirty: &WalConcurrentDirtyCache,
        evicted_only: bool,
        max_release: usize,
    ) -> io::Result<usize> {
        let mut released = 0usize;
        while released < max_release
            && let Some(pending) = retained.front().copied()
        {
            if evicted_only
                && !dirty.is_evicted(
                    channel,
                    pending.request.payload_slot,
                    pending.request.submit_sequence,
                )?
            {
                break;
            }
            if !dirty.retire(
                channel,
                pending.request.payload_slot,
                pending.request.offset / 4096,
                pending.request.submit_sequence,
            )? {
                break;
            }
            retained.pop_front();
            self.release_transferred_write_slot(channel, &pending.request)?;
            self.mark_payload_releasable(releases, pending.request_sequence)?;
            released += 1;
        }
        if released != 0 {
            let control = self.channel_ptr(channel)?;
            unsafe {
                atomic_store(
                    ptr::addr_of_mut!((*control).payload_lease_hwm),
                    releases.hwm,
                    Ordering::Release,
                );
            }
        }
        Ok(released)
    }

    fn release_wal_lane_dirty_cache(
        &self,
        channel: u32,
        retained: &mut VecDeque<PendingRemoteRead>,
        releases: &mut WalLaneReleaseTracker,
        dirty: &WalConcurrentDirtyCache,
        pressure_threshold: u64,
        stats: &mut Stats,
    ) -> io::Result<()> {
        stats.lease_releases +=
            self.release_wal_lane_retained(channel, retained, releases, dirty, true, usize::MAX)?
                as u64;

        let (outstanding, pressure_release) = if self.transfer_payload_slots {
            let free = self.transferred_free_slots(channel)?;
            let outstanding = u64::from(self.header.payload_entries).saturating_sub(free);
            let reserve = u64::from(self.header.payload_entries).saturating_sub(pressure_threshold);
            (outstanding, reserve.saturating_sub(free) as usize)
        } else {
            let control = self.channel_ptr(channel)?;
            let produced =
                unsafe { atomic_load(ptr::addr_of!((*control).req_prod), Ordering::Acquire) };
            (produced.saturating_sub(releases.hwm), retained.len())
        };
        stats.max_payload_slots_outstanding = stats.max_payload_slots_outstanding.max(outstanding);
        if outstanding < pressure_threshold || retained.is_empty() || pressure_release == 0 {
            return Ok(());
        }

        // Transferred pages can retire independently. Legacy sequence rings
        // still need the historical bulk release to advance their contiguous
        // lease HWM.
        let released = self.release_wal_lane_retained(
            channel,
            retained,
            releases,
            dirty,
            false,
            pressure_release,
        )?;
        if released != 0 {
            stats.lease_releases += released as u64;
            stats.dirty_pressure_events += 1;
            stats.dirty_pressure_evictions += released as u64;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn complete_wal_transport_batch(
        &self,
        channel: u32,
        batch: &[PendingRemoteRead],
        remote_completions: &WalCompletionTracker,
        remote_lane_tracker: &WalSharedLaneTracker,
        syncs: &WalSyncCoordinator,
        lane_completions: &mut WalLaneCompletionTracker,
        retained_writes: &mut VecDeque<PendingRemoteRead>,
        releases: &mut WalLaneReleaseTracker,
        dirty: &WalConcurrentDirtyCache,
        pressure_threshold: u64,
        completions: &WalCompletionTracker,
        lane_tracker: &WalSharedLaneTracker,
        completion_scratch: &mut Vec<PendingRemoteRead>,
        outstanding_read_refs: &mut VecDeque<OutstandingWalDirtyReadRef>,
        completion_kicks: &mut usize,
        stats: &mut Stats,
    ) -> io::Result<(usize, usize)> {
        let read_count = batch
            .iter()
            .filter(|pending| pending.request.op == ZCNBLK_SHM_OP_READ)
            .count();
        while self.completion_capacity(channel)? < read_count {
            if *completion_kicks != 0 {
                stats.kicks += u64::from(self.kick_channel(channel)?);
                *completion_kicks = 0;
            }
            std::hint::spin_loop();
        }
        remote_lane_tracker
            .mark_complete_batch(batch.iter().map(|pending| pending.request_sequence + 1))?;
        if !self.lane_local_sequences {
            remote_completions
                .mark_complete_batch(batch.iter().map(|pending| pending.request.submit_sequence))?;
        }
        for pending in batch {
            let request = pending.request;
            match request.op {
                ZCNBLK_SHM_OP_WRITE => {
                    if pending.io_contract.fua {
                        lane_completions.mark_ready(pending.request_sequence)?;
                    }
                    retained_writes.push_back(*pending);
                }
                ZCNBLK_SHM_OP_READ => {
                    lane_completions.mark_ready(pending.request_sequence)?;
                    self.mark_payload_releasable(releases, pending.request_sequence)?;
                    stats.requests += 1;
                    stats.reads += 1;
                    stats.read_bytes += u64::from(request.len);
                }
                _ => unreachable!("lane batch validated request operation"),
            }
        }
        syncs.observe_remote_lane_hwm(channel, remote_lane_tracker.hwm())?;
        self.release_wal_lane_dirty_cache(
            channel,
            retained_writes,
            releases,
            dirty,
            pressure_threshold,
            stats,
        )?;
        let control = self.channel_ptr(channel)?;
        unsafe {
            atomic_store(
                ptr::addr_of_mut!((*control).payload_lease_hwm),
                releases.hwm,
                Ordering::Release,
            );
        }
        let published = self.flush_wal_lane_completions(
            channel,
            lane_completions,
            completions,
            lane_tracker,
            completion_scratch,
            outstanding_read_refs,
        )?;
        stats.lease_releases += read_count as u64;
        Ok((read_count, published))
    }

    fn run_wal_lane_channel<const RATE_LIMITED: bool>(
        &self,
        channel: u32,
        completions: &WalCompletionTracker,
        remote_completions: &WalCompletionTracker,
        lane_trackers: &[WalSharedLaneTracker],
        remote_lane_trackers: &[WalSharedLaneTracker],
        syncs: &WalSyncCoordinator,
        vector_hwm: bool,
        dirty: &WalConcurrentDirtyCache,
        cpu: Option<usize>,
        transport_cpu: Option<usize>,
        remote: Option<RemoteWalLeaf>,
        owner_ingress: Option<WalOwnerIngressEndpoint>,
        rate_mailbox: Option<Arc<LaneBudgetMailbox>>,
        rate_epoch: Option<Arc<Instant>>,
    ) -> io::Result<(Stats, Duration, Option<RemoteWalLeaf>)> {
        if let Some(cpu) = cpu {
            pin_current_thread(cpu)?;
        }
        let started = Instant::now();
        // Keep the unthrottled transport loop byte-for-byte free of HTB work.
        // RATE_LIMITED is a monomorphization boundary, so LLVM removes the
        // mailbox, clock, credit, and branch machinery from the normal path.
        let rate_epoch = RATE_LIMITED
            .then(|| rate_epoch.expect("rate-limited lane requires the shared rate epoch"));
        let mut rate_limiter = RATE_LIMITED.then(|| {
            let mailbox = rate_mailbox
                .as_ref()
                .expect("rate-limited lane requires a rate mailbox");
            LaneLimiter::new(0, mailbox.load())
        });
        let mut rate_credit = 0usize;
        let mut active = Duration::ZERO;
        let mut active_epoch = None;
        let mut stats = Stats::default();
        let lane_window = env::var("URING_PLAY_ZCNBLK_SHM_WAL_LANE_WINDOW")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(4)
            .max(1);
        if remote
            .as_ref()
            .is_some_and(|remote| remote.stream.transport_label() == "ofi")
        {
            println!(
                "zcnblk-shm-target-ofi-framed-window: lane={channel} configured_batch_window={lane_window} effective_framed_batch_window=1 rma_read_batch_window={lane_window} reason=efa-rdm-rendezvous-requires-peer-receive"
            );
        }
        let mut transport = if let Some(owner_ingress) = owner_ingress {
            if remote.is_some() || transport_cpu.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "stable owner ingress cannot also own a lane transport",
                ));
            }
            WalLaneTransport::start_owner_ingress(owner_ingress)
        } else {
            let mut remote = remote.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "lane transport is missing")
            })?;
            remote.target_cpu = transport_cpu.or(cpu);
            WalLaneTransport::start(
                remote,
                Arc::clone(&self.mapping),
                transport_cpu,
                lane_window,
            )?
        };
        let extent_records = env::var("URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_RECORDS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(2_048)
            .max(1)
            .min(self.header.payload_entries as usize);
        let extent_fill_us = env::var("URING_PLAY_ZCNBLK_SHM_WAL_EXTENT_FILL_US")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(if transport_cpu.is_some() { 50 } else { 20 });
        let split_min_batch_records = env::var("URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_MIN_BATCH_RECORDS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(64)
            .max(1)
            .min(extent_records);
        // Preserve latency-first behavior by default.  Saturation runs may
        // opt into the same bounded extent-fill policy already used for
        // writes, avoiding tiny RMA issue batches at high aggregate depth.
        // This remains a userspace transport decision; the block client does
        // not learn or own placement, stripe, mirror, or lane policy.
        let foreground_read_immediate =
            env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_FOREGROUND_READ_IMMEDIATE", true);
        let cq_delay_spins = env::var("URING_PLAY_ZCNBLK_SHM_WAL_CQ_DELAY_SPINS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(0);
        eprintln!(
            "zcnblk-shm-target-wal-read-policy: lane={channel} foreground_immediate={foreground_read_immediate} extent_records={extent_records} extent_fill_us={extent_fill_us} minimum_batch_records={split_min_batch_records} cq_delay_spins={cq_delay_spins}"
        );
        let pending_limit = extent_records.checked_mul(lane_window).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "lane pending limit overflow")
        })?;
        let payload_entries = self.header.payload_entries as usize;
        let configured_pressure_reserve = env::var("URING_PLAY_ZCNBLK_SHM_DIRTY_PRESSURE_RESERVE")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(0);
        let (_pressure_reserve, pressure_threshold) = wal_dirty_pressure_layout(
            payload_entries,
            self.header.ring_entries as usize,
            pending_limit,
            configured_pressure_reserve,
        )?;
        let completion_kick_batch = usize::try_from(self.kick_batch)
            .unwrap_or(usize::MAX)
            .max(1);
        let mut pending_send = VecDeque::<PendingRemoteRead>::with_capacity(extent_records);
        let mut pending_syncs = VecDeque::<PendingRemoteRead>::new();
        let mut retained_writes = VecDeque::<PendingRemoteRead>::new();
        let mut releases = WalLaneReleaseTracker::new(self.header.payload_entries as usize);
        let lane_tracker = lane_trackers.get(channel as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "logical lane tracker is missing",
            )
        })?;
        let remote_lane_tracker = remote_lane_trackers.get(channel as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote lane tracker is missing",
            )
        })?;
        let mut lane_completions =
            WalLaneCompletionTracker::new(self.header.payload_entries as usize);
        let mut completion_scratch = Vec::with_capacity(self.header.ring_entries as usize);
        let mut outstanding_read_refs = VecDeque::<OutstandingWalDirtyReadRef>::new();
        let mut completion_kicks = 0usize;
        let mut fill_started = None::<Instant>;
        let mut sync_coalesce_started = None::<Instant>;
        let mut last_synced_epoch = 0u64;
        let debug_state = env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_DEBUG_STATE", false);
        let mut last_debug = Instant::now();
        while RUNNING.load(Ordering::Relaxed)
            || !pending_send.is_empty()
            || transport.in_flight_len() != 0
            || transport.has_pending()
            || !lane_completions.is_empty()
            || !pending_syncs.is_empty()
        {
            self.release_consumed_wal_read_refs(channel, &mut outstanding_read_refs, dirty)?;
            if transport.in_flight_len() == 0
                && !transport.has_pending()
                && syncs.lane_needs_service(channel)
            {
                let active_sync_epoch = syncs.epoch();
                if syncs.service(channel, &mut last_synced_epoch, |epoch| {
                    transport.sync(epoch)
                })? {
                    stats.lease_releases += self.release_wal_lane_retained(
                        channel,
                        &mut retained_writes,
                        &mut releases,
                        dirty,
                        false,
                        usize::MAX,
                    )? as u64;
                }
                let _ = syncs.try_finish(active_sync_epoch)?;
            }
            let control = self.channel_ptr(channel)?;
            if debug_state && last_debug.elapsed() >= Duration::from_secs(1) {
                let request_cons =
                    unsafe { atomic_load(ptr::addr_of!((*control).req_cons), Ordering::Acquire) };
                let request_prod =
                    unsafe { atomic_load(ptr::addr_of!((*control).req_prod), Ordering::Acquire) };
                eprintln!(
                    "zcnblk-shm-target-wal-debug: channel={channel} req_cons={request_cons} req_prod={request_prod} pending_send={} pending_front_submit={} pending_front_predecessor={} transport_pending={} in_flight={} lane_completion_ready={} lane_completions={} logical_hwm={} remote_hwm={} sync_epoch={} sync_requested_epoch={} sync_announcements={} sync_ack_mask={:#x} sync_expected_mask={:#x} sync_committed_hwm={} release_hwm={}",
                    pending_send.len(),
                    pending_send
                        .front()
                        .map_or(0, |pending| pending.request.submit_sequence),
                    pending_send
                        .front()
                        .map_or(0, |pending| pending.request.sector_predecessor),
                    transport.has_pending(),
                    transport.in_flight_len(),
                    lane_completions.ready_queue.len(),
                    lane_completions.len(),
                    completions.hwm(),
                    remote_completions.hwm(),
                    syncs.epoch(),
                    syncs.requested_epoch(),
                    syncs.announcement_count(),
                    syncs.acknowledged_lane_mask(),
                    syncs.expected_lane_mask(),
                    syncs.committed_hwm(),
                    releases.hwm,
                );
                last_debug = Instant::now();
            }
            let mut progressed = false;
            let mut force_send = false;
            let mut sync_completion_ready = false;
            let mut deferred_remote_completions = 0usize;
            while let Some(batch) = transport.try_recv()? {
                let (read_count, published) = self.complete_wal_transport_batch(
                    channel,
                    &batch,
                    remote_completions,
                    remote_lane_tracker,
                    syncs,
                    &mut lane_completions,
                    &mut retained_writes,
                    &mut releases,
                    dirty,
                    pressure_threshold,
                    completions,
                    lane_tracker,
                    &mut completion_scratch,
                    &mut outstanding_read_refs,
                    &mut completion_kicks,
                    &mut stats,
                )?;
                completion_kicks += published;
                if read_count != 0 || completion_kicks >= completion_kick_batch {
                    stats.kicks += u64::from(self.kick_channel(channel)?);
                    completion_kicks = 0;
                }
                progressed = true;
            }
            while RUNNING.load(Ordering::Relaxed)
                && !syncs.lane_needs_service(channel)
                && pending_send.len() < pending_limit
            {
                let consumed =
                    unsafe { atomic_load(ptr::addr_of!((*control).req_cons), Ordering::Acquire) };
                let produced =
                    unsafe { atomic_load(ptr::addr_of!((*control).req_prod), Ordering::Acquire) };
                if consumed == produced {
                    break;
                }
                active_epoch.get_or_insert_with(Instant::now);
                let request_ptr = self.request_ptr(channel, consumed)?;
                let published = unsafe {
                    atomic_load(ptr::addr_of!((*request_ptr).sequence), Ordering::Acquire)
                };
                if published != consumed + 1 {
                    break;
                }
                let request = unsafe { ptr::read(request_ptr) };
                let ordering_epoch = request.ordering_epoch();
                if ordering_epoch == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "lane WAL descriptor has reserved ordering epoch zero",
                    ));
                }
                let completion_trackable = if self.lane_local_sequences {
                    lane_tracker.can_track(consumed + 1)
                        && remote_lane_tracker.can_track(consumed + 1)
                } else {
                    completions.can_track(request.submit_sequence)
                        && remote_completions.can_track(request.submit_sequence)
                };
                if !completion_trackable {
                    stats.completion_window_stalls += 1;
                    force_send = true;
                    break;
                }
                if request.op == ZCNBLK_SHM_OP_SYNC {
                    if request.queue_id != channel || request.len != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "lane WAL sync descriptor mismatch",
                        ));
                    }
                    let sync_payload_offset =
                        self.request_payload_offset(channel, consumed, &request)?;
                    let pending = PendingRemoteRead {
                        request,
                        io_contract: self.io_contract(channel, consumed)?,
                        request_sequence: consumed,
                        payload_offset: sync_payload_offset,
                        dirty_ref: None,
                    };
                    stats.note_io_contract(pending.io_contract);
                    let required_hwm = request.submit_sequence.saturating_sub(1);
                    let sync_key = if self.lane_local_sequences {
                        ordering_epoch
                    } else {
                        request.submit_sequence
                    };
                    let joined = (self.lane_local_sequences
                        || remote_completions.hwm() >= required_hwm)
                        && syncs.try_join(sync_key, ordering_epoch);
                    remote_lane_tracker.mark_complete(consumed + 1)?;
                    if !self.lane_local_sequences {
                        remote_completions.mark_complete(request.submit_sequence)?;
                    }
                    // A sync descriptor carries no data to the leaf. Let the
                    // transport HWM cross it immediately so a later flush
                    // vector can conservatively include this sync without
                    // waiting on the durable marker it is trying to start.
                    syncs.observe_remote_lane_hwm(channel, remote_lane_tracker.hwm())?;
                    if joined {
                        lane_completions.admit(pending, !pending.io_contract.fua)?;
                        let payload_hwm = self.mark_payload_releasable(&mut releases, consumed)?;
                        unsafe {
                            atomic_store(
                                ptr::addr_of_mut!((*control).payload_lease_hwm),
                                payload_hwm,
                                Ordering::Release,
                            );
                        }
                        stats.lease_releases += 1;
                        sync_completion_ready = true;
                    } else {
                        let (lane_tails, required_global_hwm) = if vector_hwm {
                            (self.flush_admission_vector(ordering_epoch)?, 0)
                        } else {
                            (
                                vec![0; self.header.channels as usize].into_boxed_slice(),
                                required_hwm,
                            )
                        };
                        syncs.announce(
                            sync_key,
                            ordering_epoch,
                            lane_tails,
                            required_global_hwm,
                        )?;
                        lane_completions.admit(pending, false)?;
                        pending_syncs.push_back(pending);
                        sync_coalesce_started.get_or_insert_with(Instant::now);
                    }
                    unsafe {
                        atomic_store(
                            ptr::addr_of_mut!((*control).req_cons),
                            consumed + 1,
                            Ordering::Release,
                        );
                    }
                    stats.requests += 1;
                    stats.syncs += 1;
                    progressed = true;
                    continue;
                }
                if request.queue_id != channel
                    || !self.valid_wal_data_request(&request)
                    || request
                        .offset
                        .checked_add(u64::from(request.len))
                        .is_none_or(|end| end > self.header.capacity_bytes)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "lane WAL request topology or range mismatch: channel={channel} queue={} op={} len={} offset={} capacity={} valid_data={}",
                            request.queue_id,
                            request.op,
                            request.len,
                            request.offset,
                            self.header.capacity_bytes,
                            self.valid_wal_data_request(&request),
                        ),
                    ));
                }
                if request.sector_predecessor != 0
                    && if self.lane_local_sequences {
                        !self.lane_tracker_token_complete(lane_trackers, request.sector_predecessor)
                    } else {
                        !completions.is_complete(request.sector_predecessor)
                    }
                {
                    force_send = true;
                    break;
                }
                let payload_offset = self.request_payload_offset(channel, consumed, &request)?;
                let mut pending = PendingRemoteRead {
                    request,
                    io_contract: self.io_contract(channel, consumed)?,
                    request_sequence: consumed,
                    payload_offset,
                    dirty_ref: None,
                };
                stats.note_io_contract(pending.io_contract);
                match request.op {
                    ZCNBLK_SHM_OP_WRITE => {
                        dirty.admit(
                            channel,
                            request.payload_slot,
                            request.offset / 4096,
                            payload_offset,
                            request.submit_sequence,
                        )?;
                        lane_completions.admit(pending, !pending.io_contract.fua)?;
                        pending_send.push_back(pending);
                        stats.requests += 1;
                        stats.writes += 1;
                        stats.write_bytes += u64::from(request.len);
                        stats.early_write_acks += u64::from(!pending.io_contract.fua);
                    }
                    ZCNBLK_SHM_OP_READ => {
                        let dirty_hit = if self.dirty_read_payload_refs {
                            pending.dirty_ref =
                                dirty.acquire_ref_if_present(request.offset / 4096)?;
                            pending.dirty_ref.is_some()
                        } else {
                            let out = unsafe {
                                std::slice::from_raw_parts_mut(
                                    self.mapping.ptr.add(payload_offset),
                                    request.len as usize,
                                )
                            };
                            dirty.copy_if_present(
                                request.offset / 4096,
                                request.len as usize,
                                self.mapping.as_ref(),
                                out,
                            )?
                        };
                        if dirty_hit {
                            stats.dirty_read_refs += u64::from(pending.dirty_ref.is_some());
                            remote_lane_tracker.mark_complete(consumed + 1)?;
                            if !self.lane_local_sequences {
                                remote_completions
                                    .mark_complete_deferred(request.submit_sequence)?;
                                deferred_remote_completions += 1;
                            }
                            syncs.observe_remote_lane_hwm(channel, remote_lane_tracker.hwm())?;
                            lane_completions.admit(pending, true)?;
                            let payload_hwm =
                                self.mark_payload_releasable(&mut releases, consumed)?;
                            unsafe {
                                atomic_store(
                                    ptr::addr_of_mut!((*control).payload_lease_hwm),
                                    payload_hwm,
                                    Ordering::Release,
                                );
                            }
                            stats.requests += 1;
                            stats.reads += 1;
                            stats.read_bytes += u64::from(request.len);
                            stats.dirty_read_hits += 1;
                            stats.lease_releases += 1;
                        } else {
                            lane_completions.admit(pending, false)?;
                            pending_send.push_back(pending);
                            stats.remote_read_misses += 1;
                        }
                    }
                    _ => unreachable!("lane request operation validated"),
                }
                unsafe {
                    atomic_store(
                        ptr::addr_of_mut!((*control).req_cons),
                        consumed + 1,
                        Ordering::Release,
                    );
                }
                progressed = true;
                if pending_send.len() >= extent_records {
                    break;
                }
            }
            if deferred_remote_completions != 0 {
                remote_completions.advance_hwm();
            }

            if syncs.epoch() == 0
                && syncs.requested_epoch() != 0
                && sync_coalesce_started.is_some_and(|started| {
                    syncs.announcement_count() > 1
                        || started.elapsed() >= Duration::from_micros(syncs.coalesce_us())
                })
                && syncs
                    .try_begin_requested(if self.lane_local_sequences {
                        0
                    } else {
                        remote_completions.hwm()
                    })?
                    .is_some()
            {
                progressed = true;
            }

            if transport.in_flight_len() == 0
                && !transport.has_pending()
                && syncs.lane_needs_service(channel)
            {
                let active_sync_epoch = syncs.epoch();
                if syncs.service(channel, &mut last_synced_epoch, |epoch| {
                    transport.sync(epoch)
                })? {
                    stats.lease_releases += self.release_wal_lane_retained(
                        channel,
                        &mut retained_writes,
                        &mut releases,
                        dirty,
                        false,
                        usize::MAX,
                    )? as u64;
                }
                let _ = syncs.try_finish(active_sync_epoch)?;
                progressed = true;
            }

            let committed_sync_hwm = syncs.committed_hwm();
            while pending_syncs.front().is_some_and(|pending| {
                let sync_key = if self.lane_local_sequences {
                    pending.request.ordering_epoch()
                } else {
                    pending.request.submit_sequence
                };
                sync_key <= committed_sync_hwm
            }) {
                let pending = pending_syncs.pop_front().expect("pending sync front");
                lane_completions.mark_ready(pending.request_sequence)?;
                let payload_hwm =
                    self.mark_payload_releasable(&mut releases, pending.request_sequence)?;
                unsafe {
                    atomic_store(
                        ptr::addr_of_mut!((*control).payload_lease_hwm),
                        payload_hwm,
                        Ordering::Release,
                    );
                }
                syncs.retire_announcement()?;
                stats.lease_releases += 1;
                sync_completion_ready = true;
                progressed = true;
            }
            if pending_syncs.is_empty() {
                sync_coalesce_started = None;
            }

            self.release_wal_lane_dirty_cache(
                channel,
                &mut retained_writes,
                &mut releases,
                dirty,
                pressure_threshold,
                &mut stats,
            )?;
            completion_kicks += self.flush_wal_lane_completions(
                channel,
                &mut lane_completions,
                completions,
                lane_tracker,
                &mut completion_scratch,
                &mut outstanding_read_refs,
            )?;

            // Stable owners are shared across ingress lanes, so owner progress
            // can keep this loop non-idle after the local request ring drains.
            // Force the final partial completion batch through the armed-wake
            // transition race: the kernel cannot publish more work on this
            // lane until it is woken to consume those completions.
            let local_request_ready = self.channel_request_ready(channel)?;
            let force_completion_tail = completion_kicks != 0 && !local_request_ready;
            if force_completion_tail {
                self.force_kick_channel(channel)?;
                stats.kicks += 1;
                completion_kicks = 0;
            } else if sync_completion_ready || completion_kicks >= completion_kick_batch {
                stats.kicks += u64::from(self.kick_channel(channel)?);
                completion_kicks = 0;
            }

            let mut send_ready = 0usize;
            if !syncs.lane_needs_service(channel) {
                for pending in pending_send.iter().take(extent_records) {
                    let predecessor = pending.request.sector_predecessor;
                    let predecessor_complete = if self.lane_local_sequences {
                        self.lane_tracker_token_complete(remote_lane_trackers, predecessor)
                    } else {
                        remote_completions.is_complete(predecessor)
                    };
                    if predecessor != 0 && !predecessor_complete {
                        break;
                    }
                    send_ready += 1;
                }
            }
            let channel_ready = local_request_ready;
            if send_ready != 0 && transport.submit_available(lane_window) {
                let fill_expired = extent_fill_us == 0
                    || fill_started.get_or_insert_with(Instant::now).elapsed()
                        >= Duration::from_micros(extent_fill_us);
                let dependency_boundary = send_ready < pending_send.len().min(extent_records);
                let latency_sensitive_read = foreground_read_immediate
                    && transport.foreground_immediate_available()
                    && pending_send
                        .iter()
                        .take(send_ready)
                        .any(|pending| pending.request.op == ZCNBLK_SHM_OP_READ);
                let should_send = send_ready >= extent_records
                    || pending_send.len() >= pending_limit
                    || dependency_boundary
                    || !RUNNING.load(Ordering::Relaxed)
                    || syncs.lane_needs_service(channel)
                    || force_send
                    || latency_sensitive_read
                    || (!channel_ready && (send_ready >= split_min_batch_records || fill_expired));
                if should_send {
                    if RATE_LIMITED {
                        let mailbox = rate_mailbox
                            .as_ref()
                            .expect("rate-limited lane requires a rate mailbox");
                        let limiter = rate_limiter
                            .as_mut()
                            .expect("rate-limited lane requires a limiter");
                        if rate_credit == 0 {
                            let now_ns = rate_epoch
                                .as_ref()
                                .expect("rate-limited lane requires the shared rate epoch")
                                .elapsed()
                                .as_nanos()
                                .min(u128::from(u64::MAX))
                                as u64;
                            let _ = limiter.refresh(now_ns, mailbox);
                            rate_credit = usize::try_from(limiter.admit(
                                now_ns,
                                limiter.budget().quantum_ops.min(u64::from(u32::MAX)) as u32,
                            ))
                            .expect("u32 rate grant fits usize");
                        }
                        send_ready = send_ready.min(rate_credit);
                        if send_ready == 0 {
                            continue;
                        }
                    }
                    let mut batch = Vec::with_capacity(send_ready);
                    for _ in 0..send_ready {
                        batch.push(
                            pending_send
                                .pop_front()
                                .expect("send-ready count came from pending queue"),
                        );
                    }
                    let submitted = batch.len();
                    transport.submit(self.mapping.as_ref(), batch)?;
                    rate_credit = rate_credit.saturating_sub(submitted);
                    stats.remote_batches += 1;
                    fill_started = None;
                    progressed = true;
                }
            } else if pending_send.is_empty() {
                fill_started = None;
            }

            if transport.flush_owner_pending_if_due(
                !RUNNING.load(Ordering::Relaxed) || syncs.lane_needs_service(channel),
            )? {
                progressed = true;
            }

            let foreground_in_flight = transport.foreground_in_flight_len();
            let receive_now = transport.in_flight_len() != 0
                && (!transport.submit_available(lane_window)
                    || !RUNNING.load(Ordering::Relaxed)
                    || syncs.lane_needs_service(channel)
                    || (!progressed
                        && foreground_in_flight != 0
                        && (!channel_ready || send_ready == 0)));
            if receive_now {
                for _ in 0..cq_delay_spins {
                    std::hint::spin_loop();
                }
                let batch = transport.recv(self.mapping.as_ref())?;
                let (read_count, published) = self.complete_wal_transport_batch(
                    channel,
                    &batch,
                    remote_completions,
                    remote_lane_tracker,
                    syncs,
                    &mut lane_completions,
                    &mut retained_writes,
                    &mut releases,
                    dirty,
                    pressure_threshold,
                    completions,
                    lane_tracker,
                    &mut completion_scratch,
                    &mut outstanding_read_refs,
                    &mut completion_kicks,
                    &mut stats,
                )?;
                completion_kicks += published;
                if read_count != 0 || completion_kicks >= completion_kick_batch {
                    stats.kicks += u64::from(self.kick_channel(channel)?);
                    completion_kicks = 0;
                }
                continue;
            }

            if !RUNNING.load(Ordering::Relaxed) {
                if pending_send.is_empty()
                    && transport.in_flight_len() == 0
                    && !transport.has_pending()
                {
                    break;
                }
                continue;
            }
            if !progressed {
                if syncs.requested_epoch() != 0 {
                    std::hint::spin_loop();
                    continue;
                }
                if !pending_send.is_empty() && transport.submit_available(lane_window) {
                    std::hint::spin_loop();
                    continue;
                }
                if completion_kicks != 0 {
                    stats.kicks += u64::from(self.kick_channel(channel)?);
                    completion_kicks = 0;
                }
                let deadline = Instant::now()
                    .checked_add(Duration::from_micros(self.busy_poll_us))
                    .unwrap_or_else(Instant::now);
                let mut deadline_spins = 0u32;
                while !self.channel_request_ready(channel)?
                    && RUNNING.load(Ordering::Relaxed)
                    && !syncs.lane_needs_service(channel)
                    && transport.in_flight_len() == 0
                    && !transport.has_pending()
                {
                    std::hint::spin_loop();
                    deadline_spins = deadline_spins.wrapping_add(1);
                    if deadline_spins & 63 == 0 && Instant::now() >= deadline {
                        break;
                    }
                }
                if syncs.lane_needs_service(channel) {
                    continue;
                }
                if self.channel_request_ready(channel)?
                    || transport.in_flight_len() != 0
                    || transport.has_pending()
                {
                    active_epoch.get_or_insert_with(Instant::now);
                    continue;
                }
                if let Some(epoch) = active_epoch.take() {
                    active += epoch.elapsed();
                }
                unsafe {
                    atomic_store(
                        ptr::addr_of_mut!((*control).request_wake_armed),
                        1,
                        Ordering::Release,
                    );
                }
                if self.channel_request_ready(channel)? {
                    unsafe {
                        atomic_store(
                            ptr::addr_of_mut!((*control).request_wake_armed),
                            0,
                            Ordering::Release,
                        );
                    }
                    active_epoch.get_or_insert_with(Instant::now);
                    continue;
                }
                let mut pfds = [
                    libc::pollfd {
                        fd: self.file.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: syncs.lane_wake_fd(channel)?,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                let ret = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, 100) };
                unsafe {
                    atomic_store(
                        ptr::addr_of_mut!((*control).request_wake_armed),
                        0,
                        Ordering::Release,
                    );
                }
                if ret < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return Err(io::Error::last_os_error());
                }
                if pfds[1].revents & libc::POLLIN != 0 {
                    syncs.drain_lane_wake(channel)?;
                }
                stats.idle_polls += 1;
            }
        }
        while !outstanding_read_refs.is_empty() {
            stats.kicks += u64::from(self.kick_channel(channel)?);
            self.release_consumed_wal_read_refs(channel, &mut outstanding_read_refs, dirty)?;
            std::hint::spin_loop();
        }
        stats.lease_releases += self.release_wal_lane_retained(
            channel,
            &mut retained_writes,
            &mut releases,
            dirty,
            false,
            usize::MAX,
        )? as u64;
        let remote = transport.finish()?;
        self.release_channel_payloads(channel)?;
        if let Some(epoch) = active_epoch {
            active += epoch.elapsed();
        }
        let _wall = started.elapsed();
        Ok((stats, active, remote))
    }

    fn run_wal_lane_parallel(
        &mut self,
        cpus: Option<&[usize]>,
        transport_cpus: Option<&[usize]>,
        owner_cpus: Option<&[usize]>,
    ) -> io::Result<()> {
        let started = Instant::now();
        if self.header.reserved[0] & ZCNBLK_SHM_CAP_SECTOR_PREDECESSOR == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "lane-batched WAL requires sector-predecessor descriptors from zcnblk",
            ));
        }
        let max_in_flight = (self.header.channels as usize)
            .checked_mul(self.header.payload_entries as usize)
            .and_then(|value| value.checked_mul(2))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WAL inflight overflow"))?;
        let completions = WalCompletionTracker::new(max_in_flight)?;
        let remote_completions = WalCompletionTracker::new(max_in_flight)?;
        let lane_tracker_capacity = (self.header.payload_entries as usize)
            .checked_mul(2)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "lane tracker overflow"))?;
        let lane_trackers = (0..self.header.channels)
            .map(|_| WalSharedLaneTracker::new(lane_tracker_capacity))
            .collect::<io::Result<Vec<_>>>()?;
        let remote_lane_trackers = (0..self.header.channels)
            .map(|_| WalSharedLaneTracker::new(lane_tracker_capacity))
            .collect::<io::Result<Vec<_>>>()?;
        let sync_coalesce_us = env::var("URING_PLAY_ZCNBLK_SHM_SYNC_COALESCE_US")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
            .unwrap_or(if self.header.channels > 1 { 20 } else { 0 });
        let vector_hwm =
            self.lane_local_sequences || env_enabled_or("URING_PLAY_ZCNBLK_SHM_VECTOR_HWM", false);
        let ordering_caps = ZCNBLK_SHM_CAP_ORDERING_EPOCH | ZCNBLK_SHM_CAP_ORDERING_VECTOR;
        if vector_hwm
            && self.header.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES] & ordering_caps != ordering_caps
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "vector WAL synchronization requires ordering epochs and per-lane admission cuts from zcnblk",
            ));
        }
        let syncs = WalSyncCoordinator::new(self.header.channels, sync_coalesce_us)?;
        let logical_pages = usize::try_from(self.header.capacity_bytes / 4096).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "WAL dirty-cache logical page count exceeds usize",
            )
        })?;
        let dirty = WalConcurrentDirtyCache::new(
            logical_pages,
            self.header.channels as usize,
            self.header.payload_entries as usize,
        )?;
        let rate_control = wal_lane_rate_control_from_env(self.header.channels)?;
        let mut leaves = std::mem::take(&mut self.remote_leaves);
        for remote in &mut leaves {
            remote.attach_mapping(Arc::clone(&self.mapping))?;
        }
        if owner_cpus.is_some() && transport_cpus.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stable owner ingress cannot use split lane transport CPUs",
            ));
        }
        let owner_mode = owner_cpus.is_some();
        let expected_streams = if owner_mode {
            wal_owner_count(self.header.channels)?
        } else {
            self.header.channels as usize
        };
        if leaves.len() != expected_streams {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "lane-batched WAL path expected {expected_streams} remote streams, got {}",
                    leaves.len()
                ),
            ));
        }
        let mut owner_workers = Vec::<WalOwnerIngressWorker>::new();
        let lane_inputs = if let Some(owner_cpus) = owner_cpus {
            if owner_cpus.len() != leaves.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "stable owner CPU count must equal the remote owner count",
                ));
            }
            let queue_depth = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_QUEUE_DEPTH")
                .ok()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
                .unwrap_or(128)
                .max(2);
            let ingress_count = self.header.channels as usize;
            let mut result_txs = Vec::with_capacity(ingress_count);
            let mut result_rxs = Vec::with_capacity(ingress_count);
            for _ in 0..ingress_count {
                // Owner workers must never block publishing a result while an
                // ingress worker is blocked publishing its next command. Two
                // bounded channels in opposite directions form a circular
                // wait at high QD. Command admission remains bounded; the
                // result side is therefore indirectly bounded by accepted
                // commands and can safely remain non-blocking to producers.
                let (result_tx, result_rx) = channel();
                result_txs.push(result_tx);
                result_rxs.push(result_rx);
            }
            let result_txs: Arc<[Sender<WalOwnerIngressResult>]> = result_txs.into();
            let queued_records_by_owner: Arc<[AtomicUsize]> = (0..leaves.len())
                .map(|_| AtomicUsize::new(0))
                .collect::<Vec<_>>()
                .into();
            for (remote, cpu) in leaves.into_iter().zip(owner_cpus.iter().copied()) {
                owner_workers.push(WalOwnerIngressWorker::start(
                    remote,
                    Arc::clone(&self.mapping),
                    cpu,
                    Arc::clone(&result_txs),
                    Arc::clone(&queued_records_by_owner),
                    queue_depth,
                )?);
            }
            let owner_commands: Arc<[SyncSender<WalOwnerIngressCommand>]> = owner_workers
                .iter()
                .map(|worker| worker.command_tx.clone())
                .collect::<Vec<_>>()
                .into();
            result_rxs
                .into_iter()
                .enumerate()
                .map(|(ingress, result_rx)| {
                    (
                        None,
                        Some(WalOwnerIngressEndpoint {
                            ingress: ingress as u32,
                            owner_commands: Arc::clone(&owner_commands),
                            queued_records_by_owner: Arc::clone(&queued_records_by_owner),
                            result_rx,
                            owner_extent_records: self.owner_extent_records,
                        }),
                    )
                })
                .collect::<Vec<_>>()
        } else {
            leaves
                .into_iter()
                .map(|remote| (Some(remote), None))
                .collect::<Vec<_>>()
        };
        let results = thread::scope(|scope| {
            let mut handles = Vec::with_capacity(lane_inputs.len());
            for (channel, (remote, owner_ingress)) in lane_inputs.into_iter().enumerate() {
                let completions = &completions;
                let remote_completions = &remote_completions;
                let lane_trackers = &lane_trackers;
                let remote_lane_trackers = &remote_lane_trackers;
                let syncs = &syncs;
                let dirty = &dirty;
                let target: &SharedTarget = self;
                let cpu = cpus.and_then(|values| values.get(channel)).copied();
                let transport_cpu = transport_cpus
                    .and_then(|values| values.get(channel))
                    .copied();
                let rate_mailbox = rate_control
                    .as_ref()
                    .and_then(|control| control.mailboxes.get(channel))
                    .cloned();
                let rate_epoch = rate_control
                    .as_ref()
                    .map(|control| Arc::clone(&control.epoch));
                handles.push(scope.spawn(move || {
                    let result = if rate_mailbox.is_some() {
                        target.run_wal_lane_channel::<true>(
                            channel as u32,
                            completions,
                            remote_completions,
                            lane_trackers,
                            remote_lane_trackers,
                            syncs,
                            vector_hwm,
                            dirty,
                            cpu,
                            transport_cpu,
                            remote,
                            owner_ingress,
                            rate_mailbox,
                            rate_epoch,
                        )
                    } else {
                        target.run_wal_lane_channel::<false>(
                            channel as u32,
                            completions,
                            remote_completions,
                            lane_trackers,
                            remote_lane_trackers,
                            syncs,
                            vector_hwm,
                            dirty,
                            cpu,
                            transport_cpu,
                            remote,
                            owner_ingress,
                            None,
                            None,
                        )
                    };
                    if result.is_err() {
                        RUNNING.store(false, Ordering::Release);
                    }
                    result
                }));
            }
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| io::Error::other("zcnblk lane-batched WAL worker panicked"))?
                })
                .collect::<io::Result<Vec<_>>>()
        });
        let mut owner_remotes = Vec::with_capacity(owner_workers.len());
        let mut owner_stop_error = None;
        for worker in owner_workers {
            match worker.stop() {
                Ok(remote) => owner_remotes.push(remote),
                Err(error) if owner_stop_error.is_none() => owner_stop_error = Some(error),
                Err(_) => {}
            }
        }
        let results = results?;
        if let Some(error) = owner_stop_error {
            return Err(error);
        }
        let wall_seconds = started.elapsed().as_secs_f64().max(f64::MIN_POSITIVE);
        let mut total = Stats::default();
        let mut max_active = Duration::ZERO;
        let mut remotes = owner_remotes;
        for (channel, (stats, active, remote)) in results.into_iter().enumerate() {
            let remote_records = remote
                .as_ref()
                .map_or(stats.writes + stats.remote_read_misses, |remote| {
                    remote.read_records + remote.write_records
                });
            eprintln!(
                "zcnblk-shm-target-wal-lane: channel={} cpu={} transport_cpu={} requests={} writes={} reads={} syncs={} early_write_acks={} dirty_read_hits={} dirty_read_refs={} dirty_pressure_events={} dirty_pressure_evictions={} max_payload_slots_outstanding={} remote_read_misses={} completion_window_stalls={} active_seconds={:.6} active_iops={:.0} remote_batches={} avg_remote_batch_records={:.2} kicks={} idle_polls={}",
                channel,
                cpus.and_then(|values| values.get(channel))
                    .map_or_else(|| "unpinned".to_string(), |cpu| cpu.to_string()),
                if owner_mode {
                    owner_cpus
                        .and_then(|values| values.get(channel))
                        .map_or_else(
                            || "owner-unpinned".to_string(),
                            |cpu| format!("owner:{cpu}"),
                        )
                } else {
                    transport_cpus
                        .and_then(|values| values.get(channel))
                        .map_or_else(|| "inline".to_string(), |cpu| cpu.to_string())
                },
                stats.requests,
                stats.writes,
                stats.reads,
                stats.syncs,
                stats.early_write_acks,
                stats.dirty_read_hits,
                stats.dirty_read_refs,
                stats.dirty_pressure_events,
                stats.dirty_pressure_evictions,
                stats.max_payload_slots_outstanding,
                stats.remote_read_misses,
                stats.completion_window_stalls,
                active.as_secs_f64(),
                stats.requests as f64 / active.as_secs_f64().max(f64::MIN_POSITIVE),
                stats.remote_batches,
                remote_records as f64 / stats.remote_batches.max(1) as f64,
                stats.kicks,
                stats.idle_polls,
            );
            total.add(&stats);
            max_active = max_active.max(active);
            if let Some(remote) = remote {
                remotes.push(remote);
            }
        }
        remotes.sort_by_key(|remote| remote.lane_id);
        self.remote_leaves = remotes;
        let active_seconds = max_active.as_secs_f64().max(f64::MIN_POSITIVE);
        let payload_bytes = total.write_bytes + total.read_bytes;
        let transfer_free_slots = if self.transfer_payload_slots {
            (0..self.header.channels).try_fold(0u64, |total, channel| {
                self.transferred_free_slots(channel).and_then(|free| {
                    total.checked_add(free).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "transferred free-slot count overflow",
                        )
                    })
                })
            })?
        } else {
            0
        };
        let transfer_total_slots =
            u64::from(self.header.channels) * u64::from(self.header.payload_entries);
        let remote_transport = self
            .remote_leaves
            .first()
            .map_or("none", |remote| remote.stream.transport_label());
        let rma_read_negotiated = self
            .remote_leaves
            .iter()
            .any(|remote| remote.rma_read_window.is_some());
        let rma_write_negotiated = self
            .remote_leaves
            .iter()
            .any(|remote| remote.rma_write_window.is_some());
        let read_payload_destination = match (self.dirty_read_payload_refs, rma_read_negotiated) {
            (true, true) => "dirty-shared-slot-reference-or-rma-direct-shared-slot",
            (false, true) => "dirty-shared-slot-copy-or-rma-direct-shared-slot",
            (true, false) => {
                "dirty-shared-slot-reference-or-remote-result-payload+shared-slot-copy"
            }
            (false, false) => "dirty-shared-slot-copy-or-remote-result-payload+shared-slot-copy",
        };
        let transport_copy_contract = if rma_write_negotiated {
            "registered-shared-slot-rma-direct-to-leaf-memory;no-userspace-payload-gather;metadata-doorbell-only"
        } else if remote_transport == "ofi" {
            "one-userspace-ofi-message-gather;ofi-provider-copy"
        } else {
            "none-userspace;tcp-kernel-copy"
        };
        eprintln!(
            "zcnblk-shm-target-summary: backend={} remote_transport={remote_transport} channels={} requests={} writes={} reads={} syncs={} write_bytes={} read_bytes={} wall_seconds={wall_seconds:.6} active_seconds={active_seconds:.6} active_descriptor_iops={:.0} active_4k_equivalent_iops={:.0} active_payload_Gibitps={:.2} kicks={} idle_polls={} lease_releases={} early_write_acks={} fua_requests={} polled_requests={} ioprio_requests={} registered_lease_requests={} atomic_write_requests={} write_lifetime_requests={} dirty_read_hits={} dirty_read_refs={} dirty_pressure_events={} dirty_pressure_evictions={} max_payload_slots_outstanding={} remote_read_misses={} completion_window_stalls={} remote_batches={} avg_remote_batch_records={:.2} payload_ownership={} payload_slots_free={}/{} completion_order=ready-order+early-local-write+remote-read+global-commit-hwm data_order={} sync_boundary={} dirty_retention={} placement_owner=downstream-userspace-stage block_client_placement=no write_ingress=shared-slot-lease read_payload_destination={} kernel_payload_copies=ordinary-bio-one-per-direction+optional-debugfs-counted-arena-alias writeback_materialization_copies={transport_copy_contract} representative={}",
            if owner_mode {
                "WalTcpStableOwnerExtent"
            } else {
                "WalTcpLaneExtent"
            },
            self.header.channels,
            total.requests,
            total.writes,
            total.reads,
            total.syncs,
            total.write_bytes,
            total.read_bytes,
            total.requests as f64 / active_seconds,
            payload_bytes as f64 / 4096.0 / active_seconds,
            payload_bytes as f64 * 8.0 / active_seconds / (1024.0 * 1024.0 * 1024.0),
            total.kicks,
            total.idle_polls,
            total.lease_releases,
            total.early_write_acks,
            total.fua_requests,
            total.polled_requests,
            total.ioprio_requests,
            total.registered_lease_requests,
            total.atomic_write_requests,
            total.write_lifetime_requests,
            total.dirty_read_hits,
            total.dirty_read_refs,
            total.dirty_pressure_events,
            total.dirty_pressure_evictions,
            total.max_payload_slots_outstanding,
            total.remote_read_misses,
            total.completion_window_stalls,
            total.remote_batches,
            (total.writes + total.remote_read_misses) as f64 / total.remote_batches.max(1) as f64,
            if self.transfer_payload_slots {
                "submit-sequence-token-transfer"
            } else {
                "legacy-contiguous-hwm"
            },
            transfer_free_slots,
            transfer_total_slots,
            if owner_mode {
                "stable-extent-owner+sector-predecessor+lock-free-dirty-directory"
            } else {
                "hashed-sector-predecessor+lock-free-dirty-directory"
            },
            if vector_hwm {
                "remote-per-lane-admission-vector+all-lane-marker"
            } else {
                "remote-global-publication-hwm+all-lane-marker"
            },
            if self.transfer_payload_slots {
                "transferred-page-reference-until-remote-sync-or-targeted-pressure-retire"
            } else {
                "shared-reference-until-remote-sync-or-arena-pressure"
            },
            read_payload_destination,
            cpus.is_some() && env_enabled_or("URING_PLAY_TOPOLOGY_REPRESENTATIVE", false),
        );
        eprintln!(
            "zcnblk-shm-target-sync-summary: logical_syncs={} remote_sync_epochs={} collapsed_syncs={} joined_syncs={} coalesce_us={} remote_lane_syncs_expected={} committed_submit_hwm={} pending_requested_epoch={} pending_announcements={} pending_ack_mask={:#x} expected_ack_mask={:#x} frozen_lane_hwm={} remote_lane_hwm={} vector_hwm={} lane_release=coordinated-global-hwm",
            total.syncs,
            syncs.remote_epochs(),
            total.syncs.saturating_sub(syncs.remote_epochs()),
            syncs.joined_syncs(),
            syncs.coalesce_us(),
            syncs
                .remote_epochs()
                .saturating_mul(u64::from(self.header.channels)),
            syncs.committed_hwm(),
            syncs.requested_epoch(),
            syncs.announcement_count(),
            syncs.acknowledged_lane_mask(),
            syncs.expected_lane_mask(),
            syncs.frozen_vector(),
            syncs.remote_vector(),
            vector_hwm,
        );
        if let Some(first) = self.remote_leaves.first() {
            let write_batches = self
                .remote_leaves
                .iter()
                .map(|remote| remote.write_batches)
                .sum::<u64>();
            let write_records = self
                .remote_leaves
                .iter()
                .map(|remote| remote.write_records)
                .sum::<u64>();
            let write_bytes = self
                .remote_leaves
                .iter()
                .map(|remote| remote.write_bytes)
                .sum::<u64>();
            let write_payload_iovecs = self
                .remote_leaves
                .iter()
                .map(|remote| remote.write_payload_iovecs)
                .sum::<u64>();
            let write_payload_tx_iovecs = self
                .remote_leaves
                .iter()
                .map(|remote| remote.write_payload_tx_iovecs)
                .sum::<u64>();
            let write_payload_runs = self
                .remote_leaves
                .iter()
                .map(|remote| remote.write_payload_runs)
                .sum::<u64>();
            let max_write_payload_run_bytes = self
                .remote_leaves
                .iter()
                .map(|remote| remote.max_write_payload_run_bytes)
                .max()
                .unwrap_or(0);
            let compact_write_batches = self
                .remote_leaves
                .iter()
                .map(|remote| remote.compact_write_batches)
                .sum::<u64>();
            let request_descriptor_bytes = self
                .remote_leaves
                .iter()
                .map(|remote| remote.request_descriptor_bytes)
                .sum::<u64>();
            let wire_descriptor_bytes = self
                .remote_leaves
                .iter()
                .map(|remote| remote.wire_descriptor_bytes)
                .sum::<u64>();
            let read_batches = self
                .remote_leaves
                .iter()
                .map(|remote| remote.read_batches)
                .sum::<u64>();
            let read_records = self
                .remote_leaves
                .iter()
                .map(|remote| remote.read_records)
                .sum::<u64>();
            let read_bytes = self
                .remote_leaves
                .iter()
                .map(|remote| remote.read_bytes)
                .sum::<u64>();
            let rma_read_calls = self
                .remote_leaves
                .iter()
                .map(|remote| remote.rma_read_calls)
                .sum::<u64>();
            let rma_read_time = self
                .remote_leaves
                .iter()
                .fold(Duration::ZERO, |total, remote| total + remote.rma_read_time);
            let rma_read_copy_time = self
                .remote_leaves
                .iter()
                .fold(Duration::ZERO, |total, remote| {
                    total + remote.rma_read_copy_time
                });
            let read_payload_destination = if rma_read_negotiated {
                "registered-shared-slot-direct-rma"
            } else {
                "remote-result-payload+shared-slot-copy"
            };
            let rma_read_completion = if rma_read_negotiated {
                "initiator-local-cq-data-visible"
            } else {
                "not-negotiated"
            };
            let rma_write_completion = if rma_write_negotiated {
                "initiator-delivery-cq-before-doorbell-result-hwm"
            } else {
                "not-negotiated"
            };
            let sync_count = self
                .remote_leaves
                .iter()
                .map(|remote| remote.syncs)
                .sum::<u64>();
            let send_zc_notifications = self
                .remote_leaves
                .iter()
                .map(|remote| remote.send_zc_notifications)
                .sum::<u64>();
            let control_writev_batches = self
                .remote_leaves
                .iter()
                .map(|remote| remote.control_writev_batches)
                .sum::<u64>();
            let send_zc_copied_notifications = self
                .remote_leaves
                .iter()
                .map(|remote| remote.send_zc_copied_notifications)
                .sum::<u64>();
            let (
                recv_spin_hits,
                recv_blocking_fallbacks,
                recv_would_block_polls,
                recv_grows,
                recv_shrinks,
            ) = self
                .remote_leaves
                .iter()
                .fold((0u64, 0u64, 0u64, 0u64, 0u64), |totals, remote| {
                    let counters = remote.recv_wait.counters();
                    (
                        totals.0.saturating_add(counters.0),
                        totals.1.saturating_add(counters.1),
                        totals.2.saturating_add(counters.2),
                        totals.3.saturating_add(counters.3),
                        totals.4.saturating_add(counters.4),
                    )
                });
            eprintln!(
                "zcnblk-shm-target-remote-leaf-summary: address={} lanes={} transport={} send_mode={} recv_policy={} recv_spin_hits={} recv_blocking_fallbacks={} recv_would_block_polls={} recv_grows={} recv_shrinks={} fan_stage=userspace placement=single-leaf write_batches={} write_records={} write_bytes={} compact_write_batches={} request_descriptor_bytes={} wire_descriptor_bytes={} descriptor_bytes_saved={} write_payload_iovecs={} write_payload_tx_iovecs={} write_payload_runs={} avg_write_iovecs_per_batch={:.2} avg_write_tx_iovecs_per_batch={:.2} avg_write_run_bytes={:.0} max_write_payload_run_bytes={} read_batches={} read_records={} read_bytes={} rma_read_calls={} rma_read_seconds={:.6} avg_rma_read_us={:.3} rma_read_copy_seconds={:.6} avg_rma_read_copy_us={:.3} syncs={} control_writev_batches={} send_zc_notifications={} send_zc_copied_notifications={} payload_source=shared-slot-coalesced-iovec read_payload_destination={} rma_read_completion={} rma_write_completion={} result_contract=fifo-mixed-request-batch+global-sync-epoch",
                first.address,
                self.remote_leaves.len(),
                first.stream.transport_label(),
                first.send_mode.label(),
                first.recv_wait.policy().label(),
                recv_spin_hits,
                recv_blocking_fallbacks,
                recv_would_block_polls,
                recv_grows,
                recv_shrinks,
                write_batches,
                write_records,
                write_bytes,
                compact_write_batches,
                request_descriptor_bytes,
                wire_descriptor_bytes,
                request_descriptor_bytes.saturating_sub(wire_descriptor_bytes),
                write_payload_iovecs,
                write_payload_tx_iovecs,
                write_payload_runs,
                write_payload_iovecs as f64 / write_batches.max(1) as f64,
                write_payload_tx_iovecs as f64 / write_batches.max(1) as f64,
                write_bytes as f64 / write_payload_runs.max(1) as f64,
                max_write_payload_run_bytes,
                read_batches,
                read_records,
                read_bytes,
                rma_read_calls,
                rma_read_time.as_secs_f64(),
                rma_read_time.as_secs_f64() * 1_000_000.0 / rma_read_calls.max(1) as f64,
                rma_read_copy_time.as_secs_f64(),
                rma_read_copy_time.as_secs_f64() * 1_000_000.0 / rma_read_calls.max(1) as f64,
                sync_count,
                control_writev_batches,
                send_zc_notifications,
                send_zc_copied_notifications,
                read_payload_destination,
                rma_read_completion,
                rma_write_completion,
            );
            for remote in &self.remote_leaves {
                remote.report_rma_read_queue();
                remote.report_rma_write_queue();
                let (incoming_cpu, incoming_napi_id) = remote.stream.locality();
                eprintln!(
                    "zcnblk-shm-target-remote-timing: lane={} target_cpu={} incoming_cpu={} incoming_napi_id={} tcp_nodelay={} quickack={} recv_policy={} recv_spin_budget={} recv_spin_hits={} recv_blocking_fallbacks={} recv_would_block_polls={} recv_grows={} recv_shrinks={} request_send_calls={} request_send_seconds={:.6} avg_request_send_us={:.3} result_recv_calls={} result_recv_seconds={:.6} avg_result_recv_us={:.3} rma_read_calls={} rma_read_seconds={:.6} avg_rma_read_us={:.3} rma_read_copy_seconds={:.6} avg_rma_read_copy_us={:.3} sync_calls={} sync_seconds={:.6} avg_sync_us={:.3} result_header_seconds={:.6} result_descriptor_seconds={:.6} result_payload_seconds={:.6}",
                    remote.lane_id,
                    remote
                        .target_cpu
                        .map_or_else(|| "unpinned".to_string(), |cpu| cpu.to_string()),
                    incoming_cpu,
                    incoming_napi_id,
                    remote.tcp_nodelay,
                    remote.quickack,
                    remote.recv_wait.policy().label(),
                    remote.recv_wait.current_budget_label(),
                    remote.recv_wait.counters().0,
                    remote.recv_wait.counters().1,
                    remote.recv_wait.counters().2,
                    remote.recv_wait.counters().3,
                    remote.recv_wait.counters().4,
                    remote.request_send_calls,
                    remote.request_send_time.as_secs_f64(),
                    remote.request_send_time.as_secs_f64() * 1_000_000.0
                        / remote.request_send_calls.max(1) as f64,
                    remote.result_recv_calls,
                    remote.result_recv_time.as_secs_f64(),
                    remote.result_recv_time.as_secs_f64() * 1_000_000.0
                        / remote.result_recv_calls.max(1) as f64,
                    remote.rma_read_calls,
                    remote.rma_read_time.as_secs_f64(),
                    remote.rma_read_time.as_secs_f64() * 1_000_000.0
                        / remote.rma_read_calls.max(1) as f64,
                    remote.rma_read_copy_time.as_secs_f64(),
                    remote.rma_read_copy_time.as_secs_f64() * 1_000_000.0
                        / remote.rma_read_calls.max(1) as f64,
                    remote.syncs,
                    remote.sync_time.as_secs_f64(),
                    remote.sync_time.as_secs_f64() * 1_000_000.0 / remote.syncs.max(1) as f64,
                    remote.result_header_time.as_secs_f64(),
                    remote.result_descriptor_time.as_secs_f64(),
                    remote.result_payload_time.as_secs_f64(),
                );
            }
        }
        Ok(())
    }

    fn run_channel(
        &self,
        channel: u32,
        completions: &WalCompletionTracker,
        lane_hwms: &[AtomicU64],
        cpu: Option<usize>,
    ) -> io::Result<(Stats, Duration)> {
        if let Some(cpu) = cpu {
            pin_current_thread(cpu)?;
        }
        let started = Instant::now();
        let mut active = Duration::ZERO;
        let mut active_epoch = None;
        let mut stats = Stats::default();
        let mut pending_kick = 0u64;
        let mut burst_requests = 0u64;
        let busy_activation_requests = self.kick_batch;
        let mut busy_until = None;
        let mut completions_since_advance = 0u64;
        while RUNNING.load(Ordering::Relaxed) {
            let control = self.channel_ptr(channel)?;
            let request_sequence =
                unsafe { atomic_load(ptr::addr_of!((*control).req_cons), Ordering::Acquire) };
            let produced =
                unsafe { atomic_load(ptr::addr_of!((*control).req_prod), Ordering::Acquire) };
            if request_sequence == produced {
                let now = Instant::now();
                let burst_busy = busy_until.is_some_and(|deadline| now < deadline);
                let spin_us = if burst_busy {
                    self.busy_poll_us
                } else {
                    self.poll_us
                };
                let deadline = now
                    .checked_add(Duration::from_micros(spin_us))
                    .unwrap_or_else(Instant::now);
                let mut clock_check_countdown = self.poll_clock_check_spins;
                while unsafe { atomic_load(ptr::addr_of!((*control).req_prod), Ordering::Acquire) }
                    == request_sequence
                    && RUNNING.load(Ordering::Relaxed)
                {
                    std::hint::spin_loop();
                    clock_check_countdown -= 1;
                    if clock_check_countdown == 0 {
                        if Instant::now() >= deadline {
                            break;
                        }
                        clock_check_countdown = self.poll_clock_check_spins;
                    }
                }
                if unsafe { atomic_load(ptr::addr_of!((*control).req_prod), Ordering::Acquire) }
                    != request_sequence
                {
                    active_epoch.get_or_insert_with(Instant::now);
                    continue;
                }
                if pending_kick != 0 {
                    stats.kicks += u64::from(self.kick_channel(channel)?);
                    pending_kick = 0;
                }
                if let Some(epoch) = active_epoch.take() {
                    active += epoch.elapsed();
                }
                unsafe {
                    atomic_store(
                        ptr::addr_of_mut!((*control).request_wake_armed),
                        1,
                        Ordering::Release,
                    );
                }
                if unsafe { atomic_load(ptr::addr_of!((*control).req_prod), Ordering::Acquire) }
                    != request_sequence
                {
                    unsafe {
                        atomic_store(
                            ptr::addr_of_mut!((*control).request_wake_armed),
                            0,
                            Ordering::Release,
                        );
                    }
                    active_epoch.get_or_insert_with(Instant::now);
                    continue;
                }
                let mut pfd = libc::pollfd {
                    fd: self.file.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ret = unsafe { libc::poll(&mut pfd, 1, 100) };
                unsafe {
                    atomic_store(
                        ptr::addr_of_mut!((*control).request_wake_armed),
                        0,
                        Ordering::Release,
                    );
                }
                if ret < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return Err(io::Error::last_os_error());
                }
                stats.idle_polls += 1;
                burst_requests = 0;
                continue;
            }
            active_epoch.get_or_insert_with(Instant::now);
            burst_requests = burst_requests.saturating_add(1);
            if burst_requests >= busy_activation_requests {
                busy_until =
                    Instant::now().checked_add(Duration::from_micros(self.busy_hysteresis_us));
                burst_requests = 0;
            }
            if produced.wrapping_sub(request_sequence) > u64::from(self.header.ring_entries) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request ring overrun",
                ));
            }
            let request_ptr = self.request_ptr(channel, request_sequence)?;
            let published =
                unsafe { atomic_load(ptr::addr_of!((*request_ptr).sequence), Ordering::Acquire) };
            if published != request_sequence + 1 {
                std::hint::spin_loop();
                continue;
            }
            let request = unsafe { ptr::read(request_ptr) };
            if request.queue_id != channel
                || (!self.transfer_payload_slots
                    && request.payload_slot
                        != (request_sequence % u64::from(self.header.payload_entries)) as u32)
                || request.len > self.header.slot_bytes
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request descriptor topology or payload slot mismatch",
                ));
            }
            if self.lane_local_sequences
                && request.submit_sequence != self.lane_token(channel, request_sequence)?
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "parallel lane-local request token mismatch",
                ));
            }
            let dependency_ready = match request.op {
                ZCNBLK_SHM_OP_SYNC if self.lane_local_sequences => {
                    let tails = self.flush_admission_vector(request.ordering_epoch())?;
                    tails
                        .iter()
                        .enumerate()
                        .all(|(lane, tail)| lane_hwms[lane].load(Ordering::Acquire) >= *tail)
                }
                ZCNBLK_SHM_OP_SYNC => {
                    completions.advance_hwm() >= request.submit_sequence.saturating_sub(1)
                }
                ZCNBLK_SHM_OP_WRITE | ZCNBLK_SHM_OP_READ if self.lane_local_sequences => {
                    let token = request.sector_predecessor;
                    if token == 0 {
                        true
                    } else {
                        let zero_based = token - 1;
                        let lanes = u64::from(self.header.channels);
                        let predecessor_lane = (zero_based % lanes) as usize;
                        let predecessor_sequence = zero_based / lanes;
                        lane_hwms[predecessor_lane].load(Ordering::Acquire) > predecessor_sequence
                    }
                }
                ZCNBLK_SHM_OP_WRITE | ZCNBLK_SHM_OP_READ => {
                    completions.is_complete(request.sector_predecessor)
                }
                _ => true,
            };
            if !dependency_ready {
                std::hint::spin_loop();
                continue;
            }
            while !self.completion_has_capacity(channel)? && RUNNING.load(Ordering::Relaxed) {
                stats.kicks += u64::from(self.kick_channel(channel)?);
                std::hint::spin_loop();
            }

            let payload_offset =
                self.request_payload_offset(channel, request_sequence, &request)?;
            let payload = unsafe { self.mapping.ptr.add(payload_offset) };
            let end = request
                .offset
                .checked_add(u64::from(request.len))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "request range overflow")
                })?;
            let mut status = 0i16;
            if end > self.header.capacity_bytes {
                status = -(libc::EINVAL as i16);
            } else {
                match request.op {
                    ZCNBLK_SHM_OP_WRITE => {
                        stats.writes += 1;
                        stats.write_bytes += u64::from(request.len);
                        if let Some(ram) = self.ram.as_ref() {
                            let offset = usize::try_from(request.offset).map_err(|_| {
                                io::Error::new(io::ErrorKind::InvalidData, "RAM offset too large")
                            })?;
                            unsafe {
                                ptr::copy_nonoverlapping(
                                    payload,
                                    ram.ptr.add(offset),
                                    request.len as usize,
                                );
                            }
                        }
                    }
                    ZCNBLK_SHM_OP_READ => {
                        stats.reads += 1;
                        stats.read_bytes += u64::from(request.len);
                        if let Some(ram) = self.ram.as_ref() {
                            let offset = usize::try_from(request.offset).map_err(|_| {
                                io::Error::new(io::ErrorKind::InvalidData, "RAM offset too large")
                            })?;
                            unsafe {
                                ptr::copy_nonoverlapping(
                                    ram.ptr.add(offset),
                                    payload,
                                    request.len as usize,
                                );
                            }
                        } else {
                            unsafe {
                                ptr::write_bytes(payload, 0, request.len as usize);
                            }
                        }
                    }
                    ZCNBLK_SHM_OP_SYNC => stats.syncs += 1,
                    _ => status = -(libc::EOPNOTSUPP as i16),
                }
            }

            if self.transfer_payload_slots && request.op == ZCNBLK_SHM_OP_WRITE {
                self.release_transferred_write_slot(channel, &request)?;
            }
            stats.requests += 1;
            if self.lane_local_sequences {
                lane_hwms[channel as usize].store(request_sequence + 1, Ordering::Release);
            } else {
                completions.mark_complete_deferred(request.submit_sequence)?;
                completions_since_advance = completions_since_advance.saturating_add(1);
                if request.op == ZCNBLK_SHM_OP_SYNC {
                    completions.advance_hwm();
                    completions_since_advance = 0;
                } else if completions_since_advance >= self.kick_batch {
                    let _ = completions.try_advance_hwm();
                    completions_since_advance = 0;
                }
            }
            let completion_sequence =
                unsafe { atomic_load(ptr::addr_of!((*control).comp_prod), Ordering::Acquire) };
            let completion = self.completion_ptr(channel, completion_sequence)?;
            unsafe {
                ptr::write(
                    completion,
                    ZcnblkShmCompletion {
                        sequence: 0,
                        request_id: request.request_id,
                        offset: request.offset,
                        committed_hwm: request.submit_sequence,
                        len: request.len,
                        lane: request.lane,
                        stream: request.stream,
                        payload_slot: request.payload_slot,
                        op: request.op,
                        status,
                        flags: 0,
                        request_sequence,
                    },
                );
                atomic_store(
                    ptr::addr_of_mut!((*completion).sequence),
                    completion_sequence + 1,
                    Ordering::Release,
                );
                atomic_store(
                    ptr::addr_of_mut!((*control).comp_prod),
                    completion_sequence + 1,
                    Ordering::Release,
                );
                atomic_store(
                    ptr::addr_of_mut!((*control).req_cons),
                    request_sequence + 1,
                    Ordering::Release,
                );
                if self.should_release_payload(request_sequence, request.op) {
                    atomic_store(
                        ptr::addr_of_mut!((*control).payload_lease_hwm),
                        request_sequence + 1,
                        Ordering::Release,
                    );
                }
            }
            if self.should_release_payload(request_sequence, request.op) {
                stats.lease_releases += 1;
            }
            pending_kick += 1;
            if pending_kick >= self.kick_batch {
                stats.kicks += u64::from(self.kick_channel(channel)?);
                pending_kick = 0;
            }
        }
        if pending_kick != 0 {
            stats.kicks += u64::from(self.kick_channel(channel)?);
        }
        if !self.lane_local_sequences {
            completions.advance_hwm();
        }
        self.release_channel_payloads(channel)?;
        if let Some(epoch) = active_epoch {
            active += epoch.elapsed();
        }
        let _wall = started.elapsed();
        Ok((stats, active))
    }

    fn run_parallel(&mut self, cpus: Option<&[usize]>) -> io::Result<()> {
        let started = Instant::now();
        let max_in_flight = (self.header.channels as usize)
            .checked_mul(self.header.payload_entries.max(self.header.ring_entries) as usize)
            .and_then(|value| value.checked_mul(2))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "completion tracker overflow")
            })?;
        let completions = WalCompletionTracker::new(max_in_flight)?;
        let lane_hwms = (0..self.header.channels)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>();
        let results = thread::scope(|scope| {
            let mut handles = Vec::with_capacity(self.header.channels as usize);
            for channel in 0..self.header.channels {
                let completions = &completions;
                let lane_hwms = &lane_hwms;
                let target: &SharedTarget = self;
                let cpu = cpus
                    .and_then(|values| values.get(channel as usize))
                    .copied();
                handles.push(
                    scope.spawn(move || target.run_channel(channel, completions, lane_hwms, cpu)),
                );
            }
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| io::Error::other("zcnblk shm channel worker panicked"))?
                })
                .collect::<io::Result<Vec<_>>>()
        })?;
        let wall_seconds = started.elapsed().as_secs_f64().max(f64::MIN_POSITIVE);
        let mut total = Stats::default();
        let mut max_active = Duration::ZERO;
        for (channel, (stats, active)) in results.iter().enumerate() {
            eprintln!(
                "zcnblk-shm-target-channel: channel={} cpu={} requests={} writes={} reads={} syncs={} active_seconds={:.6} active_iops={:.0} kicks={} idle_polls={} lease_releases={}",
                channel,
                cpus.and_then(|values| values.get(channel))
                    .map_or_else(|| "unpinned".to_string(), |cpu| cpu.to_string()),
                stats.requests,
                stats.writes,
                stats.reads,
                stats.syncs,
                active.as_secs_f64(),
                stats.requests as f64 / active.as_secs_f64().max(f64::MIN_POSITIVE),
                stats.kicks,
                stats.idle_polls,
                stats.lease_releases,
            );
            total.add(stats);
            max_active = max_active.max(*active);
        }
        let active_seconds = max_active.as_secs_f64().max(f64::MIN_POSITIVE);
        let payload_bytes = total.write_bytes + total.read_bytes;
        let mut request_publishes = 0u64;
        let mut request_wake_kicks = 0u64;
        let mut completion_kicks = 0u64;
        for channel in 0..self.header.channels {
            let control = self.channel_ptr(channel)?;
            request_publishes = request_publishes.saturating_add(unsafe {
                atomic_load(
                    ptr::addr_of!((*control).request_publishes),
                    Ordering::Acquire,
                )
            });
            request_wake_kicks = request_wake_kicks.saturating_add(unsafe {
                atomic_load(ptr::addr_of!((*control).request_kicks), Ordering::Acquire)
            });
            completion_kicks = completion_kicks.saturating_add(unsafe {
                atomic_load(
                    ptr::addr_of!((*control).completion_kicks),
                    Ordering::Acquire,
                )
            });
        }
        eprintln!(
            "zcnblk-shm-target-summary: backend={:?} channels={} requests={} writes={} reads={} syncs={} write_bytes={} read_bytes={} wall_seconds={wall_seconds:.6} active_seconds={active_seconds:.6} active_descriptor_iops={:.0} active_4k_equivalent_iops={:.0} active_payload_Gibitps={:.2} completion_ioctl_kicks={} request_publishes={} request_wake_kicks={} request_wake_pct={:.4} idle_polls={} lease_releases={} lease_release_batch={} poll_us={} busy_poll_us={} busy_hysteresis_us={} poll_clock_check_spins={} busy_activation_requests={} ordering=per-channel-fifo+sector-predecessor sequence_mode={} sync_boundary={} placement_owner=downstream-userspace-stage block_client_placement=no kernel_payload_copies=ordinary-bio-one-per-direction+optional-debugfs-counted-arena-alias",
            self.backend,
            self.header.channels,
            total.requests,
            total.writes,
            total.reads,
            total.syncs,
            total.write_bytes,
            total.read_bytes,
            total.requests as f64 / active_seconds,
            payload_bytes as f64 / 4096.0 / active_seconds,
            payload_bytes as f64 * 8.0 / active_seconds / (1024.0 * 1024.0 * 1024.0),
            completion_kicks,
            request_publishes,
            request_wake_kicks,
            request_wake_kicks as f64 * 100.0 / request_publishes.max(1) as f64,
            total.idle_polls,
            total.lease_releases,
            self.lease_release_batch,
            self.poll_us,
            self.busy_poll_us,
            self.busy_hysteresis_us,
            self.poll_clock_check_spins,
            self.kick_batch,
            if self.lane_local_sequences {
                "lane-local-token"
            } else {
                "global-sequence"
            },
            if self.lane_local_sequences {
                "admitted-lane-vector-hwm-before-sync"
            } else {
                "global-completion-hwm-before-sync"
            },
        );
        Ok(())
    }

    fn run(&mut self) -> io::Result<()> {
        let start = Instant::now();
        let mut pending_kicks = vec![0u64; self.header.channels as usize];
        let mut pending_total = 0u64;
        while RUNNING.load(Ordering::Relaxed) {
            let mut next = self.next_request()?;
            let spin_us = if pending_total != 0 {
                self.busy_poll_us
            } else {
                self.poll_us
            };
            if next.is_none() && spin_us != 0 {
                let deadline = Instant::now()
                    .checked_add(Duration::from_micros(spin_us))
                    .unwrap_or_else(Instant::now);
                while next.is_none() && RUNNING.load(Ordering::Relaxed) && Instant::now() < deadline
                {
                    std::hint::spin_loop();
                    next = self.next_request()?;
                }
            }
            if next.is_none() && pending_total != 0 {
                for (channel, pending) in pending_kicks.iter_mut().enumerate() {
                    if *pending != 0 {
                        self.kick(channel as u32)?;
                        *pending = 0;
                    }
                }
                pending_total = 0;
            }
            match next {
                Some((channel, sequence, request)) => {
                    let batchable = self.backend == BackendMode::WalTcp
                        && self.valid_wal_data_request(&request)
                        && request
                            .offset
                            .checked_add(u64::from(request.len))
                            .is_some_and(|end| end <= self.header.capacity_bytes);
                    if batchable {
                        let completed =
                            self.process_wal_tcp_request_batch(channel, sequence, request)?;
                        for (channel, count) in completed.into_iter().enumerate() {
                            if count == 0 {
                                continue;
                            }
                            let count = count as u64;
                            let pending = &mut pending_kicks[channel];
                            *pending = pending.saturating_add(count);
                            pending_total = pending_total.saturating_add(count);
                            if *pending >= self.kick_batch {
                                self.kick(channel as u32)?;
                                pending_total = pending_total.saturating_sub(*pending);
                                *pending = 0;
                            }
                        }
                    } else {
                        self.process_one(channel, sequence, request)?;
                        let pending = &mut pending_kicks[channel as usize];
                        *pending = pending.saturating_add(1);
                        pending_total = pending_total.saturating_add(1);
                        if *pending >= self.kick_batch {
                            self.kick(channel)?;
                            pending_total = pending_total.saturating_sub(*pending);
                            *pending = 0;
                        }
                    }
                }
                None => {
                    self.poll_for_requests(100)?;
                }
            }
        }
        if self.backend.is_wal_writeback() {
            self.flush_wal_backend(usize::MAX)?;
            if self.backend == BackendMode::WalTcp {
                self.stop_remote_workers()?;
            }
        }
        for (channel, pending) in pending_kicks.iter().enumerate() {
            if *pending != 0 {
                self.kick(channel as u32)?;
            }
            self.release_channel_payloads(channel as u32)?;
        }
        let elapsed = start.elapsed().as_secs_f64();
        let active_seconds = self
            .active_started
            .zip(self.active_last)
            .map(|(started, last)| last.saturating_duration_since(started).as_secs_f64())
            .unwrap_or(0.0)
            .max(f64::MIN_POSITIVE);
        let payload_bytes = self.stats.write_bytes + self.stats.read_bytes;
        let (
            writeback_batch,
            writeback_batches,
            writeback_writes,
            writeback_bytes,
            durable_submit_hwm,
            pending_writes,
        ) = self.wal_state.as_ref().map_or((0, 0, 0, 0, 0, 0), |state| {
            (
                state.writeback_batch,
                state.writeback_batches,
                state.writeback_writes,
                state.writeback_bytes,
                state.durable_submit_hwm,
                state.pending.len(),
            )
        });
        let remote_transport = self
            .remote_leaves
            .first()
            .map_or("none", |remote| remote.stream.transport_label());
        let rma_read_negotiated = self
            .remote_leaves
            .iter()
            .any(|remote| remote.rma_read_window.is_some());
        let rma_write_negotiated = self
            .remote_leaves
            .iter()
            .any(|remote| remote.rma_write_window.is_some());
        let remote_read_source = if rma_read_negotiated {
            "registered-shared-slot-direct-rma"
        } else {
            "remote-result-payload+shared-slot-copy"
        };
        let (sync_boundary, write_ingress, dirty_read_source, writeback_copy_count) = match self
            .backend
        {
            BackendMode::WalMemory => (
                "materialize-prior-writes",
                "shared-slot-reference",
                "shared-slot-reference",
                "one-per-dirty-write",
            ),
            BackendMode::WalTcp => (
                "remote-leaf-hwm",
                "shared-slot-reference",
                remote_read_source,
                if rma_write_negotiated {
                    "registered-shared-slot-rma-direct-to-leaf-memory;no-userspace-payload-gather;metadata-doorbell-only"
                } else if remote_transport == "ofi" {
                    "one-userspace-ofi-message-gather;ofi-provider-copy"
                } else {
                    "none-userspace;tcp-kernel-copy"
                },
            ),
            _ => ("request-order", "immediate-copy", "reduced-memory", "none"),
        };
        eprintln!(
            "zcnblk-shm-target-summary: backend={:?} remote_transport={remote_transport} channels={} requests={} writes={} reads={} syncs={} write_bytes={} read_bytes={} wall_seconds={elapsed:.6} active_seconds={active_seconds:.6} active_descriptor_iops={:.0} active_4k_equivalent_iops={:.0} active_payload_Gibitps={:.2} kicks={} idle_polls={} lease_releases={} early_write_acks={} fua_requests={} polled_requests={} ioprio_requests={} registered_lease_requests={} atomic_write_requests={} write_lifetime_requests={} lease_release_batch={} writeback_batch={} writeback_batches={} writeback_writes={} writeback_bytes={} durable_submit_hwm={} pending_writes={} poll_us={} busy_poll_us={} busy_hysteresis_us={} poll_clock_check_spins={} completion_order=global-fifo data_order=per-sector+sync-hwm sync_contract={} sync_boundary={} placement_owner=downstream-userspace-stage block_client_placement=no write_ingress={} dirty_read_source={} kernel_payload_copies=ordinary-bio-one-per-direction+optional-debugfs-counted-arena-alias writeback_materialization_copies={}",
            self.backend,
            self.header.channels,
            self.stats.requests,
            self.stats.writes,
            self.stats.reads,
            self.stats.syncs,
            self.stats.write_bytes,
            self.stats.read_bytes,
            self.stats.requests as f64 / active_seconds,
            payload_bytes as f64 / 4096.0 / active_seconds,
            payload_bytes as f64 * 8.0 / active_seconds / (1024.0 * 1024.0 * 1024.0),
            self.stats.kicks,
            self.stats.idle_polls,
            self.stats.lease_releases,
            self.stats.early_write_acks,
            self.stats.fua_requests,
            self.stats.polled_requests,
            self.stats.ioprio_requests,
            self.stats.registered_lease_requests,
            self.stats.atomic_write_requests,
            self.stats.write_lifetime_requests,
            self.lease_release_batch,
            writeback_batch,
            writeback_batches,
            writeback_writes,
            writeback_bytes,
            durable_submit_hwm,
            pending_writes,
            self.poll_us,
            self.busy_poll_us,
            self.busy_hysteresis_us,
            self.poll_clock_check_spins,
            self.backend.sync_contract(),
            sync_boundary,
            write_ingress,
            dirty_read_source,
            writeback_copy_count,
        );
        if let Some(first) = self.remote_leaves.first() {
            let write_batches = self
                .remote_leaves
                .iter()
                .map(|remote| remote.write_batches)
                .sum::<u64>();
            let write_records = self
                .remote_leaves
                .iter()
                .map(|remote| remote.write_records)
                .sum::<u64>();
            let write_bytes = self
                .remote_leaves
                .iter()
                .map(|remote| remote.write_bytes)
                .sum::<u64>();
            let write_payload_iovecs = self
                .remote_leaves
                .iter()
                .map(|remote| remote.write_payload_iovecs)
                .sum::<u64>();
            let write_payload_tx_iovecs = self
                .remote_leaves
                .iter()
                .map(|remote| remote.write_payload_tx_iovecs)
                .sum::<u64>();
            let write_payload_runs = self
                .remote_leaves
                .iter()
                .map(|remote| remote.write_payload_runs)
                .sum::<u64>();
            let max_write_payload_run_bytes = self
                .remote_leaves
                .iter()
                .map(|remote| remote.max_write_payload_run_bytes)
                .max()
                .unwrap_or(0);
            let read_batches = self
                .remote_leaves
                .iter()
                .map(|remote| remote.read_batches)
                .sum::<u64>();
            let read_records = self
                .remote_leaves
                .iter()
                .map(|remote| remote.read_records)
                .sum::<u64>();
            let read_bytes = self
                .remote_leaves
                .iter()
                .map(|remote| remote.read_bytes)
                .sum::<u64>();
            let rma_read_calls = self
                .remote_leaves
                .iter()
                .map(|remote| remote.rma_read_calls)
                .sum::<u64>();
            let rma_read_time = self
                .remote_leaves
                .iter()
                .fold(Duration::ZERO, |total, remote| total + remote.rma_read_time);
            let rma_read_copy_time = self
                .remote_leaves
                .iter()
                .fold(Duration::ZERO, |total, remote| {
                    total + remote.rma_read_copy_time
                });
            let read_payload_destination = if rma_read_negotiated {
                "registered-shared-slot-direct-rma"
            } else {
                "remote-result-payload+shared-slot-copy"
            };
            let rma_read_completion = if rma_read_negotiated {
                "initiator-local-cq-data-visible"
            } else {
                "not-negotiated"
            };
            let rma_write_completion = if rma_write_negotiated {
                "initiator-delivery-cq-before-doorbell-result-hwm"
            } else {
                "not-negotiated"
            };
            let syncs = self
                .remote_leaves
                .iter()
                .map(|remote| remote.syncs)
                .sum::<u64>();
            let send_zc_notifications = self
                .remote_leaves
                .iter()
                .map(|remote| remote.send_zc_notifications)
                .sum::<u64>();
            let control_writev_batches = self
                .remote_leaves
                .iter()
                .map(|remote| remote.control_writev_batches)
                .sum::<u64>();
            let send_zc_copied_notifications = self
                .remote_leaves
                .iter()
                .map(|remote| remote.send_zc_copied_notifications)
                .sum::<u64>();
            let (
                recv_spin_hits,
                recv_blocking_fallbacks,
                recv_would_block_polls,
                recv_grows,
                recv_shrinks,
            ) = self
                .remote_leaves
                .iter()
                .fold((0u64, 0u64, 0u64, 0u64, 0u64), |totals, remote| {
                    let counters = remote.recv_wait.counters();
                    (
                        totals.0.saturating_add(counters.0),
                        totals.1.saturating_add(counters.1),
                        totals.2.saturating_add(counters.2),
                        totals.3.saturating_add(counters.3),
                        totals.4.saturating_add(counters.4),
                    )
                });
            eprintln!(
                "zcnblk-shm-target-remote-leaf-summary: address={} lanes={} transport={} send_mode={} recv_policy={} recv_spin_hits={} recv_blocking_fallbacks={} recv_would_block_polls={} recv_grows={} recv_shrinks={} fan_stage=userspace placement=single-leaf write_batches={} write_records={} write_bytes={} write_payload_iovecs={} write_payload_tx_iovecs={} write_payload_runs={} avg_write_iovecs_per_batch={:.2} avg_write_tx_iovecs_per_batch={:.2} avg_write_run_bytes={:.0} max_write_payload_run_bytes={} read_batches={} read_records={} read_bytes={} rma_read_calls={} rma_read_seconds={:.6} avg_rma_read_us={:.3} rma_read_copy_seconds={:.6} avg_rma_read_copy_us={:.3} syncs={} control_writev_batches={} send_zc_notifications={} send_zc_copied_notifications={} payload_source=shared-slot-coalesced-iovec read_payload_destination={} rma_read_completion={} rma_write_completion={} result_contract=range-hwm+fifo-read-batch",
                first.address,
                self.remote_leaves.len(),
                first.stream.transport_label(),
                first.send_mode.label(),
                first.recv_wait.policy().label(),
                recv_spin_hits,
                recv_blocking_fallbacks,
                recv_would_block_polls,
                recv_grows,
                recv_shrinks,
                write_batches,
                write_records,
                write_bytes,
                write_payload_iovecs,
                write_payload_tx_iovecs,
                write_payload_runs,
                write_payload_iovecs as f64 / write_batches.max(1) as f64,
                write_payload_tx_iovecs as f64 / write_batches.max(1) as f64,
                write_bytes as f64 / write_payload_runs.max(1) as f64,
                max_write_payload_run_bytes,
                read_batches,
                read_records,
                read_bytes,
                rma_read_calls,
                rma_read_time.as_secs_f64(),
                rma_read_time.as_secs_f64() * 1_000_000.0 / rma_read_calls.max(1) as f64,
                rma_read_copy_time.as_secs_f64(),
                rma_read_copy_time.as_secs_f64() * 1_000_000.0 / rma_read_calls.max(1) as f64,
                syncs,
                control_writev_batches,
                send_zc_notifications,
                send_zc_copied_notifications,
                read_payload_destination,
                rma_read_completion,
                rma_write_completion,
            );
            for remote in &self.remote_leaves {
                remote.report_rma_read_queue();
                remote.report_rma_write_queue();
                let (incoming_cpu, incoming_napi_id) = remote.stream.locality();
                eprintln!(
                    "zcnblk-shm-target-remote-lane: lane={} lane_count={} target_cpu={} incoming_cpu={} incoming_napi_id={} send_mode={} recv_policy={} recv_spin_budget={} recv_spin_hits={} recv_blocking_fallbacks={} recv_would_block_polls={} recv_grows={} recv_shrinks={} write_batches={} write_records={} write_bytes={} write_payload_iovecs={} write_payload_tx_iovecs={} write_payload_runs={} max_write_payload_run_bytes={} read_batches={} read_records={} read_bytes={} rma_read_calls={} rma_read_seconds={:.6} avg_rma_read_us={:.3} rma_read_copy_seconds={:.6} avg_rma_read_copy_us={:.3} syncs={} control_writev_batches={} send_zc_notifications={} send_zc_copied_notifications={} tcp_nodelay={} quickack={} request_send_calls={} request_send_seconds={:.6} avg_request_send_us={:.3} result_recv_calls={} result_recv_seconds={:.6} avg_result_recv_us={:.3} result_header_seconds={:.6} result_descriptor_seconds={:.6} result_payload_seconds={:.6}",
                    remote.lane_id,
                    remote.lane_count,
                    remote
                        .target_cpu
                        .map_or_else(|| "unpinned".to_string(), |cpu| cpu.to_string()),
                    incoming_cpu,
                    incoming_napi_id,
                    remote.send_mode.label(),
                    remote.recv_wait.policy().label(),
                    remote.recv_wait.current_budget_label(),
                    remote.recv_wait.counters().0,
                    remote.recv_wait.counters().1,
                    remote.recv_wait.counters().2,
                    remote.recv_wait.counters().3,
                    remote.recv_wait.counters().4,
                    remote.write_batches,
                    remote.write_records,
                    remote.write_bytes,
                    remote.write_payload_iovecs,
                    remote.write_payload_tx_iovecs,
                    remote.write_payload_runs,
                    remote.max_write_payload_run_bytes,
                    remote.read_batches,
                    remote.read_records,
                    remote.read_bytes,
                    remote.rma_read_calls,
                    remote.rma_read_time.as_secs_f64(),
                    remote.rma_read_time.as_secs_f64() * 1_000_000.0
                        / remote.rma_read_calls.max(1) as f64,
                    remote.rma_read_copy_time.as_secs_f64(),
                    remote.rma_read_copy_time.as_secs_f64() * 1_000_000.0
                        / remote.rma_read_calls.max(1) as f64,
                    remote.syncs,
                    remote.control_writev_batches,
                    remote.send_zc_notifications,
                    remote.send_zc_copied_notifications,
                    remote.tcp_nodelay,
                    remote.quickack,
                    remote.request_send_calls,
                    remote.request_send_time.as_secs_f64(),
                    remote.request_send_time.as_secs_f64() * 1_000_000.0
                        / remote.request_send_calls.max(1) as f64,
                    remote.result_recv_calls,
                    remote.result_recv_time.as_secs_f64(),
                    remote.result_recv_time.as_secs_f64() * 1_000_000.0
                        / remote.result_recv_calls.max(1) as f64,
                    remote.result_header_time.as_secs_f64(),
                    remote.result_descriptor_time.as_secs_f64(),
                    remote.result_payload_time.as_secs_f64(),
                );
            }
        }
        Ok(())
    }
}

struct AppArenaSocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Drop for AppArenaSocketGuard {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.dev() == self.dev
            && metadata.ino() == self.ino
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn spawn_app_arena_exporter(mapping: Arc<Mapping>, header: ZcnblkShmHeader) -> io::Result<()> {
    let Some(path) = env::var_os("URING_PLAY_ZCNBLK_SHM_APP_ARENA_SOCKET")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    else {
        return Ok(());
    };
    let export_fd = mapping.export_fd.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "application arena exporter has no retained HugeTLB memfd",
        )
    })?;
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o660))?;
    listener.set_nonblocking(true)?;
    let metadata = fs::symlink_metadata(&path)?;
    let guard = AppArenaSocketGuard {
        path: path.clone(),
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    let descriptor = ZcnblkAppArenaDescriptor {
        magic: ZCNBLK_APP_ARENA_MAGIC,
        version: ZCNBLK_APP_ARENA_VERSION,
        descriptor_bytes: size_of::<ZcnblkAppArenaDescriptor>() as u32,
        flags: ZCNBLK_APP_ARENA_F_EXTERNAL_HUGETLB,
        channels: header.channels,
        payload_entries: header.payload_entries,
        slot_bytes: header.slot_bytes,
        channel_bytes: size_of::<ZcnblkShmChannel>() as u32,
        payload_free_slots_offset: std::mem::offset_of!(ZcnblkShmChannel, payload_free_slots)
            as u32,
        reserved: 0,
        reserved2: 0,
        channel_offset: header.channel_offset,
        payload_owner_offset: header.reserved[ZCNBLK_SHM_HEADER_PAYLOAD_OWNER_OFFSET],
        payload_offset: header.payload_offset,
        region_bytes: header.region_bytes,
    };
    let raw_fd = export_fd.as_raw_fd();
    thread::Builder::new()
        .name("zcnblk-app-arena-export".to_string())
        .spawn(move || {
            let _guard = guard;
            let _mapping = mapping;
            eprintln!(
                "zcnblk-shm-target-app-arena: socket={} channels={} slots_per_lane={} slot_bytes={} ownership=application-token-to-kernel-lease copy_on_block_edge=no",
                path.display(), descriptor.channels, descriptor.payload_entries, descriptor.slot_bytes,
            );
            while RUNNING.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if let Err(error) = send_descriptor(&mut stream, descriptor, raw_fd) {
                            eprintln!("zcnblk-shm-target-app-arena-client-error: {error}");
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => {
                        eprintln!("zcnblk-shm-target-app-arena-listener-error: {error}");
                        break;
                    }
                }
            }
        })?;
    Ok(())
}

fn spawn_bio_arena_alias_selftest(
    mapping: Arc<Mapping>,
    header: ZcnblkShmHeader,
) -> io::Result<()> {
    if !env_enabled_or("URING_PLAY_ZCNBLK_SHM_BIO_ARENA_ALIAS_SELFTEST", false) {
        return Ok(());
    }
    if header.channels != 1
        || header.slot_bytes < 4096
        || header.reserved[ZCNBLK_SHM_HEADER_CAPABILITIES]
            & (ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_ACTIVE | ZCNBLK_SHM_CAP_BIO_ARENA_ALIAS)
            != (ZCNBLK_SHM_CAP_EXTERNAL_HUGETLB_ACTIVE | ZCNBLK_SHM_CAP_BIO_ARENA_ALIAS)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "bio arena alias selftest requires one channel, a 4K slot, and the active HugeTLB alias capability",
        ));
    }
    thread::Builder::new()
        .name("zcnblk-bio-alias-selftest".to_string())
        .spawn(move || {
            thread::sleep(Duration::from_millis(250));
            let payload_offset = usize::try_from(header.payload_offset)
                .map_err(|_| io::Error::other("payload offset exceeds usize"))?;
            if payload_offset
                .checked_add(header.slot_bytes as usize)
                .and_then(|offset| offset.checked_add(4096))
                .is_none_or(|end| end > mapping.len)
            {
                return Err(io::Error::other("payload slot falls outside shared arena"));
            }
            let payload = unsafe { mapping.ptr.add(payload_offset) };
            let read_payload = unsafe { payload.add(header.slot_bytes as usize) };
            unsafe { ptr::write_bytes(payload, 0x5a, 4096) };
            let fd = unsafe {
                libc::open(
                    b"/dev/zcnblk0\0".as_ptr().cast(),
                    libc::O_RDWR | libc::O_DIRECT | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let offset = 61 * 4096;
            let written = unsafe { libc::pwrite(fd, payload.cast(), 4096, offset) };
            if written != 4096 {
                let error = if written < 0 {
                    io::Error::last_os_error()
                } else {
                    io::Error::new(io::ErrorKind::WriteZero, "short alias selftest write")
                };
                unsafe { libc::close(fd) };
                return Err(error);
            }
            if unsafe { libc::fsync(fd) } != 0 {
                let error = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(error);
            }
            unsafe { ptr::write_bytes(read_payload, 0, 4096) };
            let read = unsafe { libc::pread(fd, read_payload.cast(), 4096, offset) };
            unsafe { libc::close(fd) };
            if read != 4096 {
                return Err(if read < 0 {
                    io::Error::last_os_error()
                } else {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "short alias selftest read")
                });
            }
            let bytes = unsafe { std::slice::from_raw_parts(read_payload, 4096) };
            if bytes.iter().any(|byte| *byte != 0x5a) {
                return Err(io::Error::other("bio arena alias selftest data mismatch"));
            }
            eprintln!(
                "zcnblk-shm-target-bio-arena-alias-selftest: PASS writes=1 reads=1 bytes_each=4096 placement_owner=userspace block_client_placement=no"
            );
            Ok(())
        })?;
    Ok(())
}

fn pin_current_thread(cpu: usize) -> io::Result<()> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    if cpu >= libc::CPU_SETSIZE as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CPU out of range",
        ));
    }
    unsafe {
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn parse_cpu_list(value: &str) -> io::Result<Vec<usize>> {
    let mut cpus = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if let Some((start, end)) = item.split_once('-') {
            let start = start
                .parse::<usize>()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
            let end = end
                .parse::<usize>()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
            if end < start {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "CPU range ends before it starts",
                ));
            }
            cpus.extend(start..=end);
        } else {
            cpus.push(
                item.parse::<usize>()
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?,
            );
        }
    }
    if cpus.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CPU list is empty",
        ));
    }
    if cpus
        .iter()
        .enumerate()
        .any(|(index, cpu)| cpus[..index].contains(cpu))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CPU list contains duplicates",
        ));
    }
    Ok(cpus)
}

pub fn cli(mut args: impl Iterator<Item = String>) -> io::Result<()> {
    let device = args
        .next()
        .unwrap_or_else(|| "/dev/zcnblk-shmctl".to_string());
    if matches!(device.as_str(), "help" | "--help" | "-h") {
        println!(
            "usage: zcnblk-shm-target [control-device] [null|memory|wal-memory|wal-tcp] [kick-batch] [cpu-list] [poll-us] [busy-poll-us] [busy-hysteresis-us]\n\
             wal-tcp connects to URING_PLAY_ZCNBLK_SHM_LEAF_ADDR (default 127.0.0.1:29000).\n\
             The target preserves channel order and serializes overlapping sectors. It does not own RAID placement."
        );
        return Ok(());
    }
    let backend = BackendMode::parse(&args.next().unwrap_or_else(|| "null".to_string()))?;
    let kick_batch = args
        .next()
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
        .unwrap_or(64)
        .max(1);
    let cpu_text = args
        .next()
        .or_else(|| env::var("URING_PLAY_ZCNBLK_SHM_TARGET_CPU_LIST").ok());
    let cpus = cpu_text.as_deref().map(parse_cpu_list).transpose()?;
    let remote_transport =
        env::var("URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT").unwrap_or_else(|_| "tcp".to_string());
    let direct_ofi = backend == BackendMode::WalTcp
        && matches!(remote_transport.as_str(), "ofi" | "rdm" | "efa");
    if backend == BackendMode::WalTcp
        && !matches!(remote_transport.as_str(), "tcp" | "ofi" | "rdm" | "efa")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT must be tcp or ofi, got {remote_transport:?}"
            ),
        ));
    }
    let rma_writes_requested =
        direct_ofi && env_enabled_or("URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES", false);
    let rma_write_owner_mode = env::var("URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_OWNER_MODE")
        .unwrap_or_else(|_| "placement".to_string());
    if !matches!(
        rma_write_owner_mode.as_str(),
        "placement" | "single-domain-fan-in"
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_OWNER_MODE must be placement or single-domain-fan-in, got {rma_write_owner_mode:?}"
            ),
        ));
    }
    let rma_write_multi_endpoint_confirmed = env_enabled_or(
        "URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITE_MULTI_ENDPOINT_CONFIRMED",
        false,
    );
    if rma_writes_requested {
        if !env_enabled_or("URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE", true) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "OFI RMA WAL writes require URING_PLAY_OFI_RMA_WRITE_DELIVERY_COMPLETE=1 so payload delivery precedes the metadata doorbell",
            ));
        }
        if !env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS", false) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "OFI RMA WAL writes require stable userspace owner ingress; set URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS=1",
            ));
        }
        let pipeline_batches = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_BATCHES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(16)
            .max(1);
        if pipeline_batches != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "OFI RMA WAL writes require exactly one unacknowledged payload doorbell per owner lane; set URING_PLAY_ZCNBLK_SHM_OWNER_PIPELINE_BATCHES=1, got {pipeline_batches}"
                ),
            ));
        }
        if !env_enabled_or("URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES_REQUIRED", false) {
            zc_topology_issue(
                "zcnblk-shm-target",
                "RMA writes may silently fall back to message payloads; set URING_PLAY_ZCNBLK_SHM_OFI_RMA_WRITES_REQUIRED=1 for an attributable benchmark",
            )?;
        }
    }
    let poll_us = args
        .next()
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
        .unwrap_or(50);
    let busy_poll_us = args
        .next()
        .or_else(|| env::var("URING_PLAY_ZCNBLK_SHM_TARGET_BUSY_POLL_US").ok())
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
        .unwrap_or(1_000)
        .max(poll_us);
    let busy_hysteresis_us = args
        .next()
        .or_else(|| env::var("URING_PLAY_ZCNBLK_SHM_TARGET_BUSY_HYSTERESIS_US").ok())
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
        .unwrap_or(10_000)
        .max(busy_poll_us);
    let lease_release_batch = env::var("URING_PLAY_ZCNBLK_SHM_LEASE_RELEASE_BATCH")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
        .unwrap_or(1);
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many arguments",
        ));
    }
    if cpus.is_none() {
        zc_topology_issue(
            "zcnblk-shm-target",
            "target workers are not CPU-pinned; pass a cpu-list before treating benchmark numbers as representative",
        )?;
    }
    if direct_ofi {
        let cq_sleep_ns = env::var("URING_PLAY_OFI_CQ_SLEEP_NS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(50_000);
        if cq_sleep_ns != 0 {
            zc_topology_issue(
                "zcnblk-shm-target",
                format!(
                    "direct OFI completion polling sleeps for {cq_sleep_ns} ns; set URING_PLAY_OFI_CQ_SLEEP_NS=0 for low-latency fabric results"
                ),
            )?;
        }
        if remote_transport == "efa"
            || env::var("URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER")
                .map(|provider| provider == "efa")
                .unwrap_or(true)
        {
            if !env_enabled_or("FI_EFA_USE_DEVICE_RDMA", false) {
                zc_topology_issue(
                    "zcnblk-shm-target",
                    "EFA device RDMA is not explicitly enabled; set FI_EFA_USE_DEVICE_RDMA=1 and verify the provider before trusting latency results",
                )?;
            }
        }
        if !env_enabled_or("URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED", false) {
            zc_topology_issue(
                "zcnblk-shm-target",
                "OFI hugetlb topology is not confirmed; reserve huge pages and set URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED=1 only after verifying the registered-buffer policy",
            )?;
        }
    }
    unsafe {
        libc::signal(
            libc::SIGINT,
            stop_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            stop_handler as *const () as libc::sighandler_t,
        );
    }
    let mut target = SharedTarget::open(
        &device,
        backend,
        kick_batch,
        poll_us,
        busy_poll_us,
        busy_hysteresis_us,
        lease_release_batch,
    )?;
    if rma_writes_requested {
        let endpoint_count = target.remote_leaves.len();
        if rma_write_owner_mode == "single-domain-fan-in" && endpoint_count != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "single-domain-fan-in requires exactly one stable userspace owner/OFI endpoint, got {endpoint_count}"
                ),
            ));
        }
        let provider = env::var("URING_PLAY_ZCNBLK_SHM_REMOTE_OFI_PROVIDER")
            .unwrap_or_else(|_| "efa".to_string());
        if rma_write_owner_mode == "placement"
            && provider == "efa"
            && endpoint_count > 1
            && !rma_write_multi_endpoint_confirmed
        {
            zc_topology_issue(
                "zcnblk-shm-target",
                format!(
                    "EFA RMA writes use {endpoint_count} stable-owner endpoints on one configured OFI domain; use single-domain-fan-in for one terminal leaf/rail or explicitly confirm the multi-endpoint placement topology"
                ),
            )?;
        }
        eprintln!(
            "zcnblk-shm-target-rma-write-owner-topology: mode={rma_write_owner_mode} block_ingress_lanes={} owner_endpoints={endpoint_count} ingress_lane_fan_in={} multi_endpoint_confirmed={rma_write_multi_endpoint_confirmed} placement_owner=separate-userspace-stable-owner block_client_placement=no",
            target.header.channels,
            if endpoint_count < target.header.channels as usize {
                format!("{}-to-{endpoint_count}", target.header.channels)
            } else {
                "none".to_string()
            },
        );
    }
    if direct_ofi {
        let message_bytes = env::var("URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
            .unwrap_or(1024 * 1024);
        let endpoint_count = target.remote_leaves.len();
        let mut estimated_registered_bytes = endpoint_count
            .checked_mul(message_bytes)
            .and_then(|bytes| bytes.checked_mul(2))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "OFI message pool size overflow",
                )
            })?;
        if env_enabled_or("URING_PLAY_ZCNBLK_SHM_OFI_RMA_READS", false) {
            let rma_registered_bytes =
                endpoint_count
                    .checked_mul(target.mapping.len)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "OFI RMA shared mapping registration size overflow",
                        )
                    })?;
            estimated_registered_bytes = estimated_registered_bytes
                .checked_add(rma_registered_bytes)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "OFI total registration size overflow",
                    )
                })?;
            eprintln!(
                "zcnblk-shm-target-ofi-rma-preflight: lanes={} shared_mapping_bytes={} estimated_per_endpoint_domain_registered_bytes={} registration_scope=whole-shared-mapping shared_mapping_registered=yes destination=request-owned-shared-slot copy_after_cq=no",
                endpoint_count, target.mapping.len, rma_registered_bytes,
            );
        }
        if rma_writes_requested {
            let rma_write_registered_bytes = endpoint_count
                .checked_mul(target.mapping.len)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "OFI RMA shared-mapping registration size overflow",
                    )
                })?;
            estimated_registered_bytes = estimated_registered_bytes
                .checked_add(rma_write_registered_bytes)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "OFI total registration size overflow",
                    )
                })?;
            eprintln!(
                "zcnblk-shm-target-ofi-rma-write-preflight: owner_lanes={endpoint_count} shared_mapping_bytes={} estimated_per_endpoint_domain_registered_bytes={rma_write_registered_bytes} registration_scope=whole-shared-mapping shared_mapping_registered=yes source_lease=until-delivery-cq remote_range_reuse=after-result-hwm overlap_order=delivery-barrier",
                target.mapping.len,
            );
        }
        if let Some(limit) = memlock_rlimit_bytes()? {
            if limit < estimated_registered_bytes as u64 {
                zc_topology_issue(
                    "zcnblk-shm-target",
                    format!(
                        "RLIMIT_MEMLOCK={limit} bytes is below the approximately {estimated_registered_bytes} bytes of per-lane OFI message and RMA buffers; raise memlock before representative runs"
                    ),
                )?;
            }
        }
    }
    let wal_lane_batch = backend == BackendMode::WalTcp
        && env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_LANE_BATCH", false)
        && !env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_OWNER_DISPATCH", false);
    let owner_dispatch = backend == BackendMode::WalTcp
        && env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_OWNER_DISPATCH", false);
    let owner_ingress = backend == BackendMode::WalTcp
        && env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS", false);
    if owner_dispatch && owner_ingress {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "legacy owner dispatch and lane-local stable owner ingress are mutually exclusive",
        ));
    }
    let split_transport =
        wal_lane_batch && env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_TRANSPORT", false);
    let transport_cpus = env::var("URING_PLAY_ZCNBLK_SHM_WAL_TRANSPORT_CPU_LIST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| parse_cpu_list(&value))
        .transpose()?;
    let owner_cpus = env::var("URING_PLAY_ZCNBLK_SHM_OWNER_CPU_LIST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| parse_cpu_list(&value))
        .transpose()?;
    let coordinator_cpu = env::var("URING_PLAY_ZCNBLK_SHM_COORDINATOR_CPU")
        .ok()
        .filter(|value| value != "none")
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    let _pid_file = TargetPidFile::from_env()?;
    if split_transport {
        let values = transport_cpus.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "split WAL transport requires URING_PLAY_ZCNBLK_SHM_WAL_TRANSPORT_CPU_LIST",
            )
        })?;
        if values.len() != target.header.channels as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "split WAL transport CPU list has {} entries but shared transport has {} channels",
                    values.len(),
                    target.header.channels
                ),
            ));
        }
        if cpus
            .as_ref()
            .is_some_and(|foreground| values.iter().any(|cpu| foreground.contains(cpu)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "split WAL foreground and transport CPU lists must use distinct logical CPUs",
            ));
        }
    } else if transport_cpus.is_some() {
        eprintln!(
            "PERF WARNING: WAL transport CPU list is ignored unless URING_PLAY_ZCNBLK_SHM_WAL_SPLIT_TRANSPORT=1"
        );
    }
    if owner_ingress {
        let owner_count = wal_owner_count(target.header.channels)?;
        let values = owner_cpus.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "stable owner ingress requires URING_PLAY_ZCNBLK_SHM_OWNER_CPU_LIST",
            )
        })?;
        if values.len() != owner_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "stable owner CPU list must provide exactly {owner_count} CPUs, got {}",
                    values.len()
                ),
            ));
        }
        if cpus
            .as_ref()
            .is_some_and(|ingress| values.iter().any(|cpu| ingress.contains(cpu)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stable owner and ingress CPU lists must be disjoint",
            ));
        }
    } else if owner_cpus.is_some() {
        eprintln!(
            "PERF WARNING: owner CPU list is ignored unless URING_PLAY_ZCNBLK_SHM_WAL_OWNER_INGRESS=1"
        );
    }
    if let Some(cpus) = cpus.as_ref() {
        if cpus.len() != target.header.channels as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "CPU list has {} entries but shared transport has {} channels",
                    cpus.len(),
                    target.header.channels
                ),
            ));
        }
        if target.header.channels == 1 {
            pin_current_thread(cpus[0])?;
        } else if wal_lane_batch {
            for (lane, remote) in target.remote_leaves.iter_mut().enumerate() {
                let cpu = transport_cpus
                    .as_ref()
                    .and_then(|values| values.get(lane))
                    .copied()
                    .unwrap_or(cpus[lane]);
                remote.target_cpu = Some(cpu);
            }
        } else if backend.is_wal_writeback() {
            let coordinator_cpu = coordinator_cpu.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "multi-lane WAL target requires URING_PLAY_ZCNBLK_SHM_COORDINATOR_CPU",
                )
            })?;
            if cpus.contains(&coordinator_cpu) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "WAL coordinator CPU must be distinct from every lane worker CPU",
                ));
            }
            pin_current_thread(coordinator_cpu)?;
            for (remote, cpu) in target.remote_leaves.iter_mut().zip(cpus.iter().copied()) {
                remote.target_cpu = Some(cpu);
            }
        }
    }
    eprintln!(
        "zcnblk-shm-target: device={} backend={backend:?} remote_transport={remote_transport} channels={} ring_entries={} payload_entries={} slot_bytes={} region_bytes={} capacity_bytes={} kick_batch={} lease_release_batch={} writeback_batch={} read_batch={} read_batch_fill_us={} read_batch_fill_min={} write_batch_fill_us={} write_batch_fill_min={} remote_leaf={} cpu_list={} owner_dispatch={} owner_count={} owner_extent_records={} owner_max_tx_iovecs={} split_transport={} transport_cpu_list={} transport_wait={} poll_us={} busy_poll_us={} busy_hysteresis_us={} poll_clock_check_spins={} wait_policy={} ordering={} sync_contract={} shared_payload_slots=true payload_ownership={} placement_owner=downstream-userspace-stage block_client_placement=no representative={} ",
        device,
        target.header.channels,
        target.header.ring_entries,
        target.header.payload_entries,
        target.header.slot_bytes,
        target.header.region_bytes,
        target.header.capacity_bytes,
        kick_batch,
        lease_release_batch,
        target
            .wal_state
            .as_ref()
            .map_or(0, |state| state.writeback_batch),
        target.read_batch,
        target.read_batch_fill_us,
        target.read_batch_fill_min,
        target.write_batch_fill_us,
        target.write_batch_fill_min,
        target
            .remote_leaves
            .first()
            .map_or("none", |remote| remote.address.as_str()),
        cpus.as_ref().map_or_else(
            || "unpinned".to_string(),
            |values| values
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
        owner_dispatch,
        if owner_ingress {
            wal_owner_count(target.header.channels)?
        } else {
            target.header.channels as usize
        },
        target.owner_extent_records,
        target.owner_max_tx_iovecs,
        split_transport,
        transport_cpus.as_ref().map_or_else(
            || "inline".to_string(),
            |values| values
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
        if env_enabled_or("URING_PLAY_ZCNBLK_SHM_WAL_TRANSPORT_GREEDY", false) {
            "greedy"
        } else {
            "adaptive"
        },
        poll_us,
        busy_poll_us,
        busy_hysteresis_us,
        target.poll_clock_check_spins,
        if poll_us == busy_poll_us {
            "fixed-active"
        } else {
            "adaptive-active-burst"
        },
        if owner_ingress {
            "lane-local-ingress+stable-extent-owner+sector-predecessor+global-sync-hwm"
        } else if owner_dispatch {
            "global-submit-batch+stable-userspace-transport-owner+global-sync-hwm"
        } else if wal_lane_batch {
            "per-lane-batch+sector-predecessor+global-sync-hwm"
        } else if target.header.channels == 1 || backend.is_wal_writeback() {
            "global-submit-order"
        } else {
            "per-channel-fifo+sector-lock"
        },
        backend.sync_contract(),
        if target.transfer_payload_slots {
            "submit-sequence-token-transfer"
        } else {
            "legacy-contiguous-hwm"
        },
        cpus.is_some() && env_enabled_or("URING_PLAY_TOPOLOGY_REPRESENTATIVE", false),
    );
    if target.header.channels > 1 && !backend.is_wal_writeback() {
        eprintln!(
            "zcnblk-shm-target-topology: channel_worker_cpu_map={} sector_order=descriptor-predecessor independent_sectors_parallel=true",
            cpus.as_ref().map_or_else(
                || "unpinned".to_string(),
                |values| values
                    .iter()
                    .enumerate()
                    .map(|(channel, cpu)| format!("ch{channel}:cpu{cpu}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        );
    } else if target.header.channels > 1 && backend.is_wal_writeback() {
        eprintln!(
            "zcnblk-shm-target-topology: coordinator_cpu={} foreground_lane_cpu_map={} transport_lane_cpu_map={} remote_ports={} ordering={}",
            if wal_lane_batch {
                "none-lane-owned".to_string()
            } else {
                coordinator_cpu.map_or_else(|| "unpinned".to_string(), |cpu| cpu.to_string())
            },
            cpus.as_ref().map_or_else(
                || "unpinned".to_string(),
                |values| values
                    .iter()
                    .enumerate()
                    .map(|(lane, cpu)| format!("lane{lane}:cpu{cpu}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            target
                .remote_leaves
                .iter()
                .map(|remote| format!(
                    "lane{}:cpu{}",
                    remote.lane_id,
                    remote
                        .target_cpu
                        .map_or_else(|| "unpinned".to_string(), |cpu| cpu.to_string())
                ))
                .collect::<Vec<_>>()
                .join(","),
            target
                .remote_leaves
                .iter()
                .map(|remote| remote.address.as_str())
                .collect::<Vec<_>>()
                .join(","),
            if owner_ingress {
                "lane-local-ingress+stable-extent-owner+sector-predecessor+global-sync-hwm"
            } else if owner_dispatch {
                "global-submit-batch+stable-userspace-transport-owner+global-sync-hwm"
            } else if wal_lane_batch {
                "per-lane-batch+sector-predecessor+global-sync-hwm"
            } else {
                "global-submit+parallel-lane-writeback"
            },
        );
    }
    if !wal_lane_batch {
        target.start_remote_workers()?;
    }
    spawn_app_arena_exporter(Arc::clone(&target.mapping), target.header)?;
    spawn_bio_arena_alias_selftest(Arc::clone(&target.mapping), target.header)?;
    if wal_lane_batch {
        target.run_wal_lane_parallel(
            cpus.as_deref(),
            transport_cpus.as_deref(),
            owner_ingress.then_some(owner_cpus.as_deref()).flatten(),
        )
    } else if target.header.channels == 1 || backend.is_wal_writeback() {
        target.run()
    } else {
        target.run_parallel(cpus.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ZcnblkFanWalDirtyCache;
    use crate::wal_contract::ZCNBLK_WAL_WRITE_LIFE_EXTREME;

    struct TestSharedLease(Vec<u8>);

    #[test]
    fn remote_leaf_address_accepts_numeric_and_dns_hosts() {
        assert_eq!(
            resolve_remote_wal_leaf_address("127.0.0.1:29000").unwrap(),
            "127.0.0.1:29000".parse::<SocketAddr>().unwrap()
        );
        let localhost = resolve_remote_wal_leaf_address("localhost:29000").unwrap();
        assert_eq!(localhost.port(), 29000);
        assert!(localhost.ip().is_loopback());
    }

    #[test]
    fn remote_leaf_address_requires_a_port() {
        assert!(resolve_remote_wal_leaf_address("localhost").is_err());
    }

    #[test]
    fn wal_transport_owner_is_stable_and_extent_local() {
        let owners = 8;
        let extent_records = 256;
        for extent in 0..32u64 {
            let expected = extent as usize % owners;
            for record in 0..extent_records {
                let offset = (extent * extent_records + record) * 4096;
                assert_eq!(
                    wal_transport_owner(offset, owners, extent_records).unwrap(),
                    expected
                );
            }
        }
        assert!(wal_transport_owner(1, owners, extent_records).is_err());
        assert!(wal_transport_owner(0, 0, extent_records).is_err());
    }

    #[test]
    fn batched_lane_completion_advances_sector_predecessor_hwm() {
        let mut completed = vec![0, 0];
        // Token 7 is lane 0, request sequence 3 in a two-lane topology.
        assert!(!lane_token_is_complete(&completed, 7));
        advance_lane_completion_counts(&mut completed, &[4, 2]).unwrap();
        assert!(lane_token_is_complete(&completed, 7));
        assert!(!lane_token_is_complete(&completed, 8));
        assert!(advance_lane_completion_counts(&mut completed, &[1]).is_err());
    }

    #[test]
    fn rma_read_queue_keeps_pending_request_when_ring_is_full() {
        let read = |offset: u64| PendingRemoteRead {
            request: ZcnblkShmRequest {
                op: ZCNBLK_SHM_OP_READ,
                len: 4096,
                offset,
                ..ZcnblkShmRequest::default()
            },
            io_contract: ZcnblkWalIoContract::default(),
            request_sequence: offset / 4096,
            payload_offset: offset as usize,
            dirty_ref: None,
        };
        let mut queue = RemoteWalRmaReadQueue::new(4096, 1).unwrap();
        queue
            .submit_batch(
                RemoteWalRmaReadWindow {
                    addr: 0,
                    key: 0,
                    len: 8192,
                },
                8192,
                &[read(0), read(4096)],
            )
            .unwrap();

        let (slot, first) = queue.take_postable().unwrap();
        assert_eq!(slot, 0);
        assert_eq!(first.remote_offset, 0);
        assert_eq!(queue.pending.len(), 1);
        assert!(queue.take_postable().is_none());
        assert_eq!(queue.pending.len(), 1);

        queue.free_slots.push(slot);
        let (_, second) = queue.take_postable().unwrap();
        assert_eq!(second.remote_offset, 4096);
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn rma_read_batch_validates_payload_range_before_queueing() {
        let request = PendingRemoteRead {
            request: ZcnblkShmRequest {
                op: ZCNBLK_SHM_OP_READ,
                len: 4096,
                offset: 0,
                ..ZcnblkShmRequest::default()
            },
            io_contract: ZcnblkWalIoContract::default(),
            request_sequence: 0,
            payload_offset: 4096,
            dirty_ref: None,
        };
        let mut queue = RemoteWalRmaReadQueue::new(4096, 1).unwrap();
        let error = queue
            .submit_batch(
                RemoteWalRmaReadWindow {
                    addr: 0,
                    key: 0,
                    len: 4096,
                },
                4096,
                &[request],
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(queue.pending.is_empty());
        assert!(queue.batches.is_empty());
    }

    #[test]
    fn rma_read_batch_fifo_uses_direct_monotonic_indexing() {
        let read = |offset: u64| PendingRemoteRead {
            request: ZcnblkShmRequest {
                op: ZCNBLK_SHM_OP_READ,
                len: 4096,
                offset,
                ..ZcnblkShmRequest::default()
            },
            io_contract: ZcnblkWalIoContract::default(),
            request_sequence: offset / 4096,
            payload_offset: offset as usize,
            dirty_ref: None,
        };
        let window = RemoteWalRmaReadWindow {
            addr: 0,
            key: 0,
            len: 8192,
        };
        let mut queue = RemoteWalRmaReadQueue::new(4096, 2).unwrap();
        let first = queue.submit_batch(window, 8192, &[read(0)]).unwrap();
        let second = queue.submit_batch(window, 8192, &[read(4096)]).unwrap();
        assert_eq!(queue.batch_index(first), Some(0));
        assert_eq!(queue.batch_index(second), Some(1));
        queue.batches[0].batch.complete = true;
        queue.batches[1].batch.complete = true;
        assert!(queue.finish_batch(second).is_err());
        assert_eq!(queue.finish_batch(first).unwrap().records, 1);
        assert_eq!(queue.batch_index(second), Some(0));
        assert_eq!(queue.finish_batch(second).unwrap().records, 1);
        assert!(queue.batches.is_empty());
    }

    #[test]
    fn rma_read_batch_run_accounts_group_once() {
        let read = |offset: u64| PendingRemoteRead {
            request: ZcnblkShmRequest {
                op: ZCNBLK_SHM_OP_READ,
                len: 4096,
                offset,
                ..ZcnblkShmRequest::default()
            },
            io_contract: ZcnblkWalIoContract::default(),
            request_sequence: offset / 4096,
            payload_offset: offset as usize,
            dirty_ref: None,
        };
        let mut queue = RemoteWalRmaReadQueue::new(4096, 3).unwrap();
        let batch_id = queue
            .submit_batch(
                RemoteWalRmaReadWindow {
                    addr: 0,
                    key: 0,
                    len: 12_288,
                },
                12_288,
                &[read(0), read(4096), read(8192)],
            )
            .unwrap();
        queue.complete_batch_run(batch_id, 2).unwrap();
        assert_eq!(queue.batches.front().unwrap().batch.remaining, 1);
        assert!(!queue.batch_complete(batch_id).unwrap());
        queue.complete_batch_run(batch_id, 1).unwrap();
        assert!(queue.batch_complete(batch_id).unwrap());
    }

    #[test]
    fn rma_read_tail_policy_replaces_synthetic_flush_with_real_marker() {
        assert_eq!(rma_read_post_policy(false, true, 1, 8, 7), (false, false));
        assert_eq!(rma_read_post_policy(true, false, 1, 8, 0), (false, true));
        assert_eq!(rma_read_post_policy(true, false, 1, 7, 1), (true, false));
        assert_eq!(rma_read_post_policy(true, false, 2, 7, 1), (false, false));
        assert_eq!(rma_read_post_policy(true, true, 1, 7, 1), (false, true));
        assert_eq!(rma_read_post_policy(true, false, 3, 1, 7), (false, true));
    }

    #[test]
    fn rma_write_batch_validates_every_range_before_posting() {
        let window = RemoteWalRmaReadWindow {
            addr: 0x1000,
            key: 7,
            len: 16_384,
        };
        let valid = [
            RemoteWalRmaWriteRun {
                source_offset: 0,
                remote_offset: 4096,
                len: 4096,
            },
            RemoteWalRmaWriteRun {
                source_offset: 8192,
                remote_offset: 12_288,
                len: 4096,
            },
        ];
        validate_remote_wal_rma_write_runs(16_384, window, &valid).unwrap();

        let invalid = |source_offset, remote_offset, len| {
            [RemoteWalRmaWriteRun {
                source_offset,
                remote_offset,
                len,
            }]
        };
        assert!(validate_remote_wal_rma_write_runs(16_384, window, &invalid(0, 0, 0)).is_err());
        assert!(
            validate_remote_wal_rma_write_runs(16_384, window, &invalid(usize::MAX, 0, 2),)
                .is_err()
        );
        assert!(
            validate_remote_wal_rma_write_runs(16_384, window, &invalid(12_288, 0, 8192)).is_err()
        );
        assert!(
            validate_remote_wal_rma_write_runs(16_384, window, &invalid(0, u64::MAX, 2)).is_err()
        );
        assert!(
            validate_remote_wal_rma_write_runs(16_384, window, &invalid(0, 12_288, 8192)).is_err()
        );
        assert!(
            validate_remote_wal_rma_write_runs(
                16_384,
                RemoteWalRmaReadWindow {
                    addr: u64::MAX - 1024,
                    ..window
                },
                &invalid(0, 4096, 4096),
            )
            .is_err()
        );
    }

    #[test]
    fn rma_write_overlap_barrier_allows_disjoint_payload_operations() {
        let active = vec![
            Some(RemoteWalRmaActiveWrite {
                token: 1,
                remote_offset: 4096,
                remote_end: 8192,
            }),
            Some(RemoteWalRmaActiveWrite {
                token: 2,
                remote_offset: 16_384,
                remote_end: 20_480,
            }),
        ];
        let run = |remote_offset, len| RemoteWalRmaWriteRun {
            source_offset: 0,
            remote_offset,
            len,
        };

        assert!(remote_wal_rma_write_overlaps_active(
            &active,
            run(6144, 4096)
        ));
        assert!(remote_wal_rma_write_overlaps_active(
            &active,
            run(12_288, 8192)
        ));
        assert!(!remote_wal_rma_write_overlaps_active(
            &active,
            run(8192, 4096)
        ));
        assert!(!remote_wal_rma_write_overlaps_active(
            &active,
            run(20_480, 4096)
        ));
    }

    impl ZcnblkFanWalSharedLeaseSource for TestSharedLease {
        fn payload_slice(&self, start: usize, len: usize) -> io::Result<&[u8]> {
            self.0.get(start..start + len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "test shared lease out of range")
            })
        }
    }

    #[test]
    fn shared_abi_layout_matches_kernel_header() {
        assert_eq!(size_of::<ZcnblkShmChannel>(), 320);
        assert_eq!(std::mem::offset_of!(ZcnblkShmChannel, req_prod), 0);
        assert_eq!(std::mem::offset_of!(ZcnblkShmChannel, req_cons), 64);
        assert_eq!(
            std::mem::offset_of!(ZcnblkShmChannel, request_wake_armed),
            72
        );
        assert_eq!(std::mem::offset_of!(ZcnblkShmChannel, comp_prod), 128);
        assert_eq!(std::mem::offset_of!(ZcnblkShmChannel, comp_cons), 192);
        assert_eq!(
            std::mem::offset_of!(ZcnblkShmChannel, completion_wake_armed),
            208
        );
        assert_eq!(
            std::mem::offset_of!(ZcnblkShmChannel, payload_free_slots),
            256
        );
        assert_eq!(size_of::<ZcnblkShmRequest>(), 64);
        assert_eq!(size_of::<ZcnblkShmCompletion>(), 64);
        assert_eq!(size_of::<ZcnblkShmIoContract>(), 16);
        assert_eq!(size_of::<ZcnblkShmHeader>(), 144);
        assert_eq!(size_of::<ZcnblkShmArenaImport>(), 32);
        assert_eq!(ZCNBLK_SHM_VERSION, 6);
        assert_eq!(ZCNBLK_SHM_IO_FEATURE_ALL, 0x7f);
        assert_eq!(ZCNBLK_SHM_IOC_ATTACH, 0x4010_bc01);
        assert_eq!(ZCNBLK_SHM_IOC_KICK, 0x4004_bc02);
        assert_eq!(ZCNBLK_SHM_IOC_GET_INFO, 0x8090_bc03);
        assert_eq!(ZCNBLK_SHM_IOC_IMPORT_ARENA, 0x4020_bc04);
    }

    #[test]
    fn shared_io_contract_maps_all_seven_wal_features() {
        let expected_lease_id = 0x1122_3344_5566_7788;
        let contract = ZcnblkShmIoContract {
            flags: ZCNBLK_SHM_IO_F_FUA
                | ZCNBLK_SHM_IO_F_POLLED_COMPLETION
                | ZCNBLK_SHM_IO_F_REGISTERED_LEASE
                | ZCNBLK_SHM_IO_F_ATOMIC_WRITE,
            ioprio: 0x4123,
            write_lifetime: ZCNBLK_WAL_WRITE_LIFE_EXTREME,
            reserved: 0,
            lease_id: expected_lease_id,
        }
        .into_wal()
        .unwrap();
        assert!(contract.fua);
        assert!(contract.polled_completion);
        assert!(contract.registered_lease);
        assert!(contract.atomic_write);
        assert_eq!(contract.ioprio, 0x4123);
        assert_eq!(contract.write_lifetime, ZCNBLK_WAL_WRITE_LIFE_EXTREME);
        assert_eq!(contract.lease_id, expected_lease_id);
        assert_eq!(contract.required_features(true), ZCNBLK_WAL_FEATURE_ALL);

        assert!(
            ZcnblkShmIoContract {
                reserved: 1,
                ..ZcnblkShmIoContract::default()
            }
            .into_wal()
            .is_err()
        );
    }

    #[test]
    fn io_contract_stats_count_each_per_request_feature() {
        let mut stats = Stats::default();
        stats.note_io_contract(ZcnblkWalIoContract {
            fua: true,
            polled_completion: true,
            registered_lease: true,
            atomic_write: true,
            ioprio: 0x4001,
            write_lifetime: ZCNBLK_WAL_WRITE_LIFE_EXTREME,
            lease_id: 1,
        });

        assert_eq!(stats.fua_requests, 1);
        assert_eq!(stats.polled_requests, 1);
        assert_eq!(stats.ioprio_requests, 1);
        assert_eq!(stats.registered_lease_requests, 1);
        assert_eq!(stats.atomic_write_requests, 1);
        assert_eq!(stats.write_lifetime_requests, 1);
    }

    #[test]
    fn transferred_payload_owner_rejects_stale_release_without_free_count_drift() {
        let owner = AtomicU64::new(41);
        let free_slots = AtomicU64::new(7);

        assert_eq!(
            release_payload_owner_token(&owner, &free_slots, 40),
            Err(41)
        );
        assert_eq!(owner.load(Ordering::Acquire), 41);
        assert_eq!(free_slots.load(Ordering::Acquire), 7);

        release_payload_owner_token(&owner, &free_slots, 41).unwrap();
        assert_eq!(owner.load(Ordering::Acquire), 0);
        assert_eq!(free_slots.load(Ordering::Acquire), 8);
    }

    #[test]
    fn shared_payload_lease_round_trips_through_dirty_cache() {
        let cache = ZcnblkFanWalDirtyCache::new();
        assert!(!cache.retain_committed);
        let mut bytes = vec![0u8; 8192];
        bytes[..4096].fill(0x5a);
        bytes[4096..].fill(0xa5);
        let source: Arc<dyn ZcnblkFanWalSharedLeaseSource> = Arc::new(TestSharedLease(bytes));

        cache
            .admit_write_shared_lease(0, source, 0, 8192, 17)
            .unwrap();
        let mut out = vec![0u8; 8192];
        assert!(cache.overlay_read(0, 8192, &mut out).unwrap().is_empty());
        assert!(out[..4096].iter().all(|value| *value == 0x5a));
        assert!(out[4096..].iter().all(|value| *value == 0xa5));

        cache.mark_committed(0, 8192, 17).unwrap();
        assert_eq!(cache.snapshot_records().unwrap(), 0);
    }

    #[test]
    fn wal_payload_release_hwm_is_lane_local_and_gap_aware() {
        let source: Arc<dyn ZcnblkFanWalSharedLeaseSource> =
            Arc::new(TestSharedLease(vec![0u8; 2 * 8 * 4096]));
        let mut state = WalWritebackState::new(source, 16 * 4096, 2, 8, 2).unwrap();

        state.mark_releasable(1, 0).unwrap();
        assert_eq!(state.payload_hwm(0).unwrap(), 0);
        assert_eq!(state.payload_hwm(1).unwrap(), 1);

        state.mark_releasable(0, 1).unwrap();
        assert_eq!(state.payload_hwm(0).unwrap(), 0);
        state.mark_releasable(0, 0).unwrap();
        assert_eq!(state.payload_hwm(0).unwrap(), 2);
        assert_eq!(state.payload_hwm(1).unwrap(), 1);
    }

    #[test]
    fn concurrent_dirty_cache_keeps_newer_cross_lane_write_visible() {
        let mut bytes = vec![0u8; 4 * 4096];
        bytes[..4096].fill(0x11);
        bytes[4096..8192].fill(0x22);
        let source: Arc<dyn ZcnblkFanWalSharedLeaseSource> = Arc::new(TestSharedLease(bytes));
        let cache = WalConcurrentDirtyCache::new(4, 2, 2).unwrap();

        cache.admit(0, 0, 1, 0, 1).unwrap();
        let mut out = vec![0u8; 4096];
        assert!(
            cache
                .copy_if_present(1, 4096, source.as_ref(), &mut out)
                .unwrap()
        );
        assert!(out.iter().all(|byte| *byte == 0x11));

        cache.admit(1, 0, 1, 4096, 2).unwrap();
        assert!(
            cache
                .copy_if_present(1, 4096, source.as_ref(), &mut out)
                .unwrap()
        );
        assert!(out.iter().all(|byte| *byte == 0x22));

        assert!(cache.is_evicted(0, 0, 1).unwrap());
        cache.retire(0, 0, 1, 1).unwrap();
        assert!(
            cache
                .copy_if_present(1, 4096, source.as_ref(), &mut out)
                .unwrap()
        );
        assert!(out.iter().all(|byte| *byte == 0x22));

        cache.retire(1, 0, 1, 2).unwrap();
        assert!(
            !cache
                .copy_if_present(1, 4096, source.as_ref(), &mut out)
                .unwrap()
        );
    }

    #[test]
    fn lane_release_tracker_advances_only_across_contiguous_slots() {
        let mut releases = WalLaneReleaseTracker::new(8);
        assert_eq!(releases.mark_releasable(2).unwrap(), 0);
        assert_eq!(releases.mark_releasable(0).unwrap(), 1);
        assert_eq!(releases.mark_releasable(1).unwrap(), 3);
    }

    #[test]
    fn wal_dirty_pressure_reserves_lane_window_and_sync_admission() {
        assert_eq!(
            wal_dirty_pressure_layout(131_072, 128, 8_192, 0).unwrap(),
            (8_320, 122_752)
        );
        assert_eq!(
            wal_dirty_pressure_layout(4_096, 128, 1_024, 0).unwrap(),
            (1_152, 2_944)
        );
        assert_eq!(
            wal_dirty_pressure_layout(4_096, 128, 1_024, 512).unwrap(),
            (512, 3_584)
        );
        assert!(wal_dirty_pressure_layout(1, 1, 1, 0).is_err());
    }

    #[test]
    fn shared_payload_layout_counts_contiguous_zero_copy_ranges() {
        let empty = shared_payload_plan([]).unwrap();
        assert_eq!(empty.source_iovecs, 0);
        assert!(empty.runs.is_empty());
        assert!(empty.rma_runs.is_empty());
        assert_eq!(empty.max_run_bytes, 0);
        let payloads = [
            (0, 0, 4096),
            (4096, 4096, 4096),
            (12_288, 20_480, 4096),
            (16_384, 24_576, 8192),
        ];
        let plan = shared_payload_plan(payloads).unwrap();
        assert_eq!(plan.source_iovecs, 4);
        assert_eq!(plan.runs, vec![(0, 8192), (12_288, 12_288)]);
        assert_eq!(
            plan.rma_runs,
            vec![
                RemoteWalRmaWriteRun {
                    source_offset: 0,
                    remote_offset: 0,
                    len: 8192,
                },
                RemoteWalRmaWriteRun {
                    source_offset: 12_288,
                    remote_offset: 20_480,
                    len: 12_288,
                },
            ]
        );
        assert_eq!(plan.max_run_bytes, 12_288);
        assert!(shared_payload_plan([(usize::MAX, 0, 2)]).is_err());
        assert!(shared_payload_plan([(0, u64::MAX, 2)]).is_err());
    }

    #[test]
    fn shared_payload_plan_preserves_wire_bytes() {
        let backing = (0u8..64).collect::<Vec<_>>();
        let payloads = [
            (0usize, 100u64, 4usize),
            (4, 104, 4),
            (12, 200, 4),
            (16, 204, 8),
            (32, 300, 4),
        ];
        let expected = payloads
            .iter()
            .flat_map(|&(offset, _, len)| backing[offset..offset + len].iter().copied())
            .collect::<Vec<_>>();
        let plan = shared_payload_plan(payloads).unwrap();
        let actual = plan
            .runs
            .iter()
            .flat_map(|&(offset, len)| backing[offset..offset + len].iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(plan.runs, vec![(0, 8), (12, 12), (32, 4)]);
        assert_eq!(actual, expected);
    }

    #[test]
    fn compact_write_batch_layout_preserves_offsets_lengths_and_mode() {
        let request =
            |offset: u64, submit_sequence: u64, payload_offset: usize| PendingRemoteRead {
                request: ZcnblkShmRequest {
                    offset,
                    submit_sequence,
                    len: 4096,
                    op: ZCNBLK_SHM_OP_WRITE,
                    ..ZcnblkShmRequest::default()
                },
                io_contract: ZcnblkWalIoContract::default(),
                request_sequence: submit_sequence - 10,
                payload_offset,
                dirty_ref: None,
            };
        let mut requests = [request(8192, 10, 0), request(24_576, 11, 4096)];
        requests[0].io_contract = ZcnblkWalIoContract {
            fua: true,
            polled_completion: true,
            registered_lease: true,
            atomic_write: true,
            ioprio: 0x4001,
            write_lifetime: ZCNBLK_WAL_WRITE_LIFE_EXTREME,
            lease_id: 10,
        };
        let (descriptors, payload_len, original_descriptor_len) =
            compact_write_batch_descriptors(3, &requests).unwrap();

        assert_eq!(payload_len, 8192);
        assert_eq!(original_descriptor_len, 2 * ZCNBLK_FAN_WAL_HEADER_LEN);
        assert_eq!(
            descriptors.len(),
            2 * ZCNBLK_FAN_WAL_COMPACT_WRITE_EXTENT_LEN
        );
        let first = ZcnblkFanWalCompactWriteExtent::decode(
            &descriptors[..ZCNBLK_FAN_WAL_COMPACT_WRITE_EXTENT_LEN],
        )
        .unwrap();
        let second = ZcnblkFanWalCompactWriteExtent::decode(
            &descriptors[ZCNBLK_FAN_WAL_COMPACT_WRITE_EXTENT_LEN..],
        )
        .unwrap();
        assert_eq!(first.leaf_offset, 8192);
        assert_eq!(first.logical_offset, 8192);
        assert_eq!(first.payload_len, 4096);
        assert_eq!(first.record_count, 1);
        assert_eq!(first.mode_selector, 10 ^ 3);
        assert_eq!(
            ZcnblkWalIoContract::decode(first.io_contract_word, first.lease_id).unwrap(),
            requests[0].io_contract
        );
        assert_eq!(second.leaf_offset, 24_576);
        assert_eq!(second.mode_selector, 11 ^ 3);

        let mut mixed = requests;
        mixed[1].request.op = ZCNBLK_SHM_OP_READ;
        assert!(compact_write_batch_descriptors(3, &mixed).is_err());
    }

    #[test]
    fn remote_wal_connection_remains_thread_portable() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RemoteWalLeaf>();
    }

    #[test]
    fn framed_wal_batch_requires_a_result_before_another_message_request() {
        let mut pending = VecDeque::new();
        assert!(!wal_lane_has_framed_batch(&pending));
        pending.push_back(WalLanePendingBatch {
            requests: Vec::new(),
            kind: WalLanePendingBatchKind::Rma(7),
        });
        assert!(!wal_lane_has_framed_batch(&pending));
        pending.push_back(WalLanePendingBatch {
            requests: Vec::new(),
            kind: WalLanePendingBatchKind::Framed,
        });
        assert!(wal_lane_has_framed_batch(&pending));
    }

    #[test]
    fn only_remote_wal_backend_can_ack_block_sync() {
        assert!(!BackendMode::Null.can_ack_block_sync());
        assert!(!BackendMode::Memory.can_ack_block_sync());
        assert!(!BackendMode::WalMemory.can_ack_block_sync());
        assert!(BackendMode::WalTcp.can_ack_block_sync());
    }

    #[test]
    fn remote_wal_tx_context_rejects_pending_shutdown() {
        let mut tx = RemoteWalTxContext {
            uring: None,
            pending_batches: VecDeque::new(),
        };
        assert!(tx.ensure_idle().is_ok());
        tx.pending_batches.push_back(RemoteWalPendingTx::SendZc(7));
        let error = tx.ensure_idle().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("1 zero-copy batches still pending")
        );
    }

    #[test]
    fn remote_wal_recv_policy_parser_is_strict() {
        assert_eq!(
            RemoteWalRecvPolicy::parse("fixed").unwrap(),
            RemoteWalRecvPolicy::Fixed
        );
        assert_eq!(
            RemoteWalRecvPolicy::parse("adaptive").unwrap(),
            RemoteWalRecvPolicy::Adaptive
        );
        assert!(RemoteWalRecvPolicy::parse("auto").is_err());
    }

    #[test]
    fn adaptive_recv_can_start_quiescent_and_ramp() {
        let mut state = ZcnblkFanWalAdaptiveRecvSpin::with_limits(0, 0, 4_096, 50_000, 10_000_000);
        assert_eq!(state.current, 0);
        state.grow();
        assert_eq!(state.current, 1);
        state.shrink();
        assert_eq!(state.current, 0);
    }

    #[test]
    fn lane_completion_tracker_releases_ready_write_ahead_of_remote_read() {
        let request = |request_sequence: u64, submit_sequence: u64| PendingRemoteRead {
            request: ZcnblkShmRequest {
                submit_sequence,
                ..ZcnblkShmRequest::default()
            },
            io_contract: ZcnblkWalIoContract::default(),
            request_sequence,
            payload_offset: request_sequence as usize * 4096,
            dirty_ref: None,
        };
        let mut completions = WalLaneCompletionTracker::new(8);
        completions.admit(request(0, 1), false).unwrap();
        completions.admit(request(1, 2), true).unwrap();
        assert_eq!(completions.len(), 2);
        assert_eq!(completions.pop_ready().unwrap().request_sequence, 1);
        assert_eq!(completions.len(), 1);
        assert!(!completions.is_empty());
        completions.mark_ready(0).unwrap();
        assert_eq!(completions.pop_ready().unwrap().request_sequence, 0);
        assert!(completions.is_empty());
    }

    #[test]
    fn lane_completion_tracker_withholds_flush_while_later_write_drains() {
        let pending = |request_sequence: u64, submit_sequence: u64, op: u16| PendingRemoteRead {
            request: ZcnblkShmRequest {
                submit_sequence,
                op,
                ..ZcnblkShmRequest::default()
            },
            io_contract: ZcnblkWalIoContract::default(),
            request_sequence,
            payload_offset: request_sequence as usize * 4096,
            dirty_ref: None,
        };
        let mut completions = WalLaneCompletionTracker::new(8);
        completions
            .admit(pending(0, 10, ZCNBLK_SHM_OP_SYNC), false)
            .unwrap();
        completions
            .admit(pending(1, 11, ZCNBLK_SHM_OP_WRITE), true)
            .unwrap();

        assert_eq!(
            completions.pop_ready().unwrap().request.op,
            ZCNBLK_SHM_OP_WRITE
        );
        completions.mark_ready(0).unwrap();
        assert_eq!(
            completions.pop_ready().unwrap().request.op,
            ZCNBLK_SHM_OP_SYNC
        );
        assert!(completions.is_empty());
    }

    #[test]
    fn dirty_read_ref_pins_transferred_slot_until_consumer_release() {
        let dirty = WalConcurrentDirtyCache::new(1, 2, 4).unwrap();
        dirty.admit(1, 2, 0, 24_576, 7).unwrap();
        let read_ref = dirty.acquire_ref_if_present(0).unwrap().unwrap();
        assert_eq!(read_ref.source_channel, 1);
        assert_eq!(read_ref.payload_slot, 2);
        assert!(!dirty.retire(1, 2, 0, 7).unwrap());
        dirty.release_ref(read_ref).unwrap();
        assert!(dirty.retire(1, 2, 0, 7).unwrap());
        assert!(dirty.acquire_ref_if_present(0).unwrap().is_none());
    }

    #[test]
    fn wal_completion_tracker_advances_across_out_of_order_completions() {
        let tracker = WalCompletionTracker::new(8).unwrap();

        assert_eq!(tracker.mark_complete(3).unwrap(), 0);
        assert!(tracker.is_complete(3));
        assert!(!tracker.is_complete(2));
        assert_eq!(tracker.mark_complete(1).unwrap(), 1);
        assert_eq!(tracker.mark_complete(2).unwrap(), 3);
        assert!(tracker.is_complete(1));
        assert!(tracker.is_complete(2));
        assert!(tracker.is_complete(3));
    }

    #[test]
    fn wal_completion_tracker_batches_one_hwm_advance() {
        let tracker = WalCompletionTracker::new(8).unwrap();

        assert_eq!(tracker.mark_complete_batch([3, 1, 2]).unwrap(), 3);
        assert!(tracker.is_complete(1));
        assert!(tracker.is_complete(2));
        assert!(tracker.is_complete(3));
    }

    #[test]
    fn wal_shared_lane_tracker_keeps_exact_out_of_order_visibility() {
        let tracker = WalSharedLaneTracker::new(8).unwrap();

        assert_eq!(tracker.mark_complete(3).unwrap(), 0);
        assert!(tracker.is_complete(3));
        assert!(!tracker.is_complete(2));
        assert_eq!(tracker.mark_complete_batch([1, 2]).unwrap(), 3);
        assert!(tracker.is_complete(1));
        assert!(tracker.is_complete(2));
        assert!(tracker.is_complete(3));
    }

    #[test]
    fn wal_shared_lane_tracker_reuses_wrapped_slots_after_hwm() {
        let tracker = WalSharedLaneTracker::new(2).unwrap();

        assert_eq!(tracker.mark_complete_batch([1, 2]).unwrap(), 2);
        assert!(tracker.can_track(4));
        assert_eq!(tracker.mark_complete_batch([4, 3]).unwrap(), 4);
    }

    #[test]
    fn wal_sync_coordinator_joins_contiguous_and_groups_announced_prefixes() {
        let syncs = WalSyncCoordinator::new(2, 20).unwrap();
        assert!(!syncs.try_join(1, 1));

        syncs.begin(10, 1, &[4, 9]).unwrap();
        let mut lane_zero_epoch = 0;
        let mut lane_one_epoch = 0;
        let mut remote_syncs = 0;
        assert!(
            syncs
                .service(0, &mut lane_zero_epoch, |_| {
                    remote_syncs += 1;
                    Ok(())
                })
                .unwrap()
        );
        assert!(
            syncs
                .service(1, &mut lane_one_epoch, |_| {
                    remote_syncs += 1;
                    Ok(())
                })
                .unwrap()
        );
        assert!(syncs.all_acknowledged(10).unwrap());
        syncs.finish(10).unwrap();

        assert_eq!(remote_syncs, 2);
        assert_eq!(syncs.committed_hwm(), 10);
        assert!(syncs.try_join(11, 1));
        assert_eq!(syncs.committed_hwm(), 11);
        assert!(!syncs.try_join(13, 2));

        syncs
            .announce(13, 2, vec![6, 12].into_boxed_slice(), 0)
            .unwrap();
        syncs
            .announce(15, 3, vec![7, 14].into_boxed_slice(), 0)
            .unwrap();
        assert_eq!(syncs.requested_epoch(), 13);
        assert_eq!(syncs.announcement_count(), 2);
        assert_eq!(syncs.try_begin_requested(0).unwrap(), None);
        syncs.observe_remote_lane_hwm(0, 7).unwrap();
        syncs.observe_remote_lane_hwm(1, 14).unwrap();
        assert_eq!(syncs.try_begin_requested(0).unwrap(), Some(15));
        assert!(!syncs.try_finish(10).unwrap());
        assert!(
            syncs
                .service(0, &mut lane_zero_epoch, |_| {
                    remote_syncs += 1;
                    Ok(())
                })
                .unwrap()
        );
        assert!(
            syncs
                .service(1, &mut lane_one_epoch, |_| {
                    remote_syncs += 1;
                    Ok(())
                })
                .unwrap()
        );
        syncs.finish(15).unwrap();
        syncs.retire_announcement().unwrap();
        syncs.retire_announcement().unwrap();

        assert_eq!(remote_syncs, 4);
        assert_eq!(syncs.committed_hwm(), 15);
        assert_eq!(syncs.requested_epoch(), 0);
        assert_eq!(syncs.announcement_count(), 0);
        assert_eq!(syncs.remote_epochs(), 2);
        assert_eq!(syncs.joined_syncs(), 1);

        syncs
            .announce(15, 3, vec![7, 14].into_boxed_slice(), 0)
            .unwrap();
        assert_eq!(syncs.try_begin_requested(0).unwrap(), None);
        assert_eq!(syncs.epoch(), 0);
        assert_eq!(syncs.requested_epoch(), 0);
        assert_eq!(syncs.remote_epochs(), 2);
        syncs.retire_announcement().unwrap();
    }

    #[test]
    fn wal_sync_announcement_wakes_every_lane() {
        let syncs = WalSyncCoordinator::new(2, 20).unwrap();
        syncs
            .announce(1, 1, vec![0, 0].into_boxed_slice(), 0)
            .unwrap();

        for lane in 0..2 {
            let mut pfd = libc::pollfd {
                fd: syncs.lane_wake_fd(lane).unwrap(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut pfd, 1, 0) }, 1);
            assert_ne!(pfd.revents & libc::POLLIN, 0);
            syncs.drain_lane_wake(lane).unwrap();
            pfd.revents = 0;
            assert_eq!(unsafe { libc::poll(&mut pfd, 1, 0) }, 0);
        }
    }

    #[test]
    fn wal_sync_vector_freezes_lane_tails_until_global_commit() {
        let syncs = WalSyncCoordinator::new(2, 0).unwrap();
        syncs.begin(10, 1, &[4, 9]).unwrap();

        assert_eq!(syncs.frozen_lane_tail(0).unwrap(), 4);
        assert_eq!(syncs.frozen_lane_tail(1).unwrap(), 9);
        assert!(syncs.lane_needs_service(0));
        assert!(syncs.lane_needs_service(1));

        let mut lane_zero_epoch = 0;
        syncs.service(0, &mut lane_zero_epoch, |_| Ok(())).unwrap();
        assert!(syncs.lane_needs_service(0));
        assert!(syncs.lane_needs_service(1));

        // Post-HWM traffic advances the live remote HWM without changing the cut.
        syncs.observe_remote_lane_hwm(0, 12).unwrap();
        assert_eq!(syncs.frozen_lane_tail(0).unwrap(), 4);

        let mut lane_one_epoch = 0;
        syncs.service(1, &mut lane_one_epoch, |_| Ok(())).unwrap();
        assert!(syncs.lane_needs_service(0));
        assert!(syncs.lane_needs_service(1));
        syncs.finish(10).unwrap();
        assert!(!syncs.lane_needs_service(0));
        assert!(!syncs.lane_needs_service(1));
        assert_eq!(syncs.committed_hwm(), 10);
    }

    #[test]
    fn wal_sync_scalar_fallback_waits_for_global_remote_hwm() {
        let syncs = WalSyncCoordinator::new(2, 0).unwrap();
        syncs
            .announce(10, 1, vec![0, 0].into_boxed_slice(), 9)
            .unwrap();
        assert_eq!(syncs.try_begin_requested(8).unwrap(), None);
        assert_eq!(syncs.try_begin_requested(9).unwrap(), Some(10));
    }

    #[test]
    fn wal_sync_vector_crosses_queued_sync_descriptors() {
        let syncs = WalSyncCoordinator::new(2, 0).unwrap();
        let mut lane_zero = WalLaneReleaseTracker::new(8);
        let mut lane_one = WalLaneReleaseTracker::new(8);

        lane_zero.mark_releasable(0).unwrap();
        lane_one.mark_releasable(0).unwrap();
        syncs.observe_remote_lane_hwm(0, lane_zero.hwm).unwrap();
        syncs.observe_remote_lane_hwm(1, lane_one.hwm).unwrap();
        syncs
            .announce(10, 1, vec![2, 2].into_boxed_slice(), 0)
            .unwrap();
        assert_eq!(syncs.try_begin_requested(0).unwrap(), None);

        // Lane-local sequence 1 is a zero-payload sync descriptor. It may
        // advance transport readiness before its block completion is durable.
        lane_zero.mark_releasable(1).unwrap();
        lane_one.mark_releasable(1).unwrap();
        syncs.observe_remote_lane_hwm(0, lane_zero.hwm).unwrap();
        syncs.observe_remote_lane_hwm(1, lane_one.hwm).unwrap();
        assert_eq!(syncs.try_begin_requested(0).unwrap(), Some(10));
    }

    #[test]
    fn shm_request_id_carries_ordering_epoch_without_growing_descriptor() {
        let request = ZcnblkShmRequest {
            request_id: (42 << ZCNBLK_SHM_REQUEST_ID_BITS) | 7,
            ..ZcnblkShmRequest::default()
        };
        assert_eq!(request.ordering_epoch(), 42);
        assert_eq!(request.client_request_id(), 7);
        assert_eq!(
            size_of::<ZcnblkShmRequest>(),
            ZCNBLK_SHM_DESC_BYTES as usize
        );
    }

    #[test]
    fn wal_completion_tracker_bounds_sequence_skew_before_ring_alias() {
        let tracker = WalCompletionTracker::new(4).unwrap();

        assert!(tracker.can_track(4));
        assert!(!tracker.can_track(5));
        tracker.mark_complete(1).unwrap();
        assert!(tracker.can_track(5));
        tracker.mark_complete(4).unwrap();
        assert!(!tracker.can_track(6));
        tracker.mark_complete(2).unwrap();
        assert!(tracker.can_track(6));
    }

    #[test]
    fn wal_completion_tracker_serializes_concurrent_hwm_scanners() {
        let tracker = Arc::new(WalCompletionTracker::new(2_048).unwrap());
        let mut workers = Vec::new();
        for lane in 0..4u64 {
            let tracker = Arc::clone(&tracker);
            workers.push(thread::spawn(move || {
                for sequence in (lane + 1..=1_024).step_by(4) {
                    tracker.mark_complete(sequence).unwrap();
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        assert_eq!(tracker.advance_hwm(), 1_024);
        assert!(tracker.is_complete(1_024));
    }

    #[test]
    fn wal_spsc_ring_wraps_without_reordering() {
        let ring = WalSpscRing::new(4).unwrap();
        for cycle in 0..64u64 {
            for index in 0..4u64 {
                ring.try_push(cycle * 4 + index).unwrap();
            }
            assert_eq!(ring.try_push(u64::MAX), Err(u64::MAX));
            for index in 0..4u64 {
                assert_eq!(ring.try_pop(), Some(cycle * 4 + index));
            }
            assert_eq!(ring.try_pop(), None);
        }
    }

    #[test]
    fn wal_spsc_ring_transfers_concurrently_in_order() {
        let ring = Arc::new(WalSpscRing::new(64).unwrap());
        let producer_ring = Arc::clone(&ring);
        let producer = thread::spawn(move || {
            for value in 0..100_000u64 {
                let mut pending = value;
                loop {
                    match producer_ring.try_push(pending) {
                        Ok(()) => break,
                        Err(returned) => pending = returned,
                    }
                    std::hint::spin_loop();
                }
            }
        });
        for expected in 0..100_000u64 {
            loop {
                if let Some(actual) = ring.try_pop() {
                    assert_eq!(actual, expected);
                    break;
                }
                std::hint::spin_loop();
            }
        }
        producer.join().unwrap();
    }

    #[test]
    fn adaptive_owner_wait_grows_while_busy_and_shrinks_after_quiescence() {
        let (sender, receiver) = sync_channel(1);
        let mut wait = AdaptiveChannelReceiver::new(true, 2, 8, 1_000);

        sender.send(1u32).unwrap();
        assert_eq!(wait.recv(&receiver).unwrap(), 1);
        assert_eq!(wait.current_spins, 4);
        assert_eq!(wait.spin_hits, 1);

        let delayed = thread::spawn(move || {
            thread::sleep(Duration::from_millis(2));
            sender.send(2u32).unwrap();
        });
        assert_eq!(wait.recv(&receiver).unwrap(), 2);
        delayed.join().unwrap();
        assert_eq!(wait.current_spins, 2);
        assert_eq!(wait.blocking_waits, 1);
        assert_eq!(wait.quick_blocking_waits, 0);
    }

    #[test]
    fn owner_result_publication_does_not_wait_for_ingress_drain() {
        let (sender, receiver) = channel();
        for sequence in 1..=4_096u64 {
            send_owner_result(&sender, WalOwnerIngressResult::Sync(sequence)).unwrap();
        }
        for expected in 1..=4_096u64 {
            match receiver.recv().unwrap() {
                WalOwnerIngressResult::Sync(actual) => assert_eq!(actual, expected),
                WalOwnerIngressResult::Batch(_) | WalOwnerIngressResult::Failed(_) => {
                    panic!("unexpected owner result")
                }
            }
        }
    }

    #[test]
    fn owner_fragment_deadline_tracks_the_oldest_pending_tail() {
        assert_eq!(WalLaneTransport::default_owner_fragment_records(1), 1);
        assert_eq!(WalLaneTransport::default_owner_fragment_records(2), 2);
        assert_eq!(WalLaneTransport::default_owner_fragment_records(32), 16);

        let oldest = Instant::now()
            .checked_sub(Duration::from_millis(2))
            .unwrap();
        let mut started = Some(oldest);
        WalLaneTransport::update_owner_fragment_deadline(&mut started, true);
        assert_eq!(started, Some(oldest));

        WalLaneTransport::update_owner_fragment_deadline(&mut started, false);
        assert!(started.is_none());
    }

    #[test]
    fn owner_write_results_are_background_only() {
        let pending = |op| PendingRemoteRead {
            request: ZcnblkShmRequest {
                op,
                ..ZcnblkShmRequest::default()
            },
            io_contract: ZcnblkWalIoContract::default(),
            request_sequence: 0,
            payload_offset: 0,
            dirty_ref: None,
        };

        assert!(!WalLaneTransport::batch_has_foreground(&[
            pending(ZCNBLK_SHM_OP_WRITE),
            pending(ZCNBLK_SHM_OP_WRITE),
        ]));
        assert!(WalLaneTransport::batch_has_foreground(&[
            pending(ZCNBLK_SHM_OP_WRITE),
            pending(ZCNBLK_SHM_OP_READ),
        ]));
    }

    #[test]
    fn owner_mixed_hysteresis_bypasses_write_fill_until_quiescent() {
        let pending = |op| PendingRemoteRead {
            request: ZcnblkShmRequest {
                op,
                ..ZcnblkShmRequest::default()
            },
            io_contract: ZcnblkWalIoContract::default(),
            request_sequence: 0,
            payload_offset: 0,
            dirty_ref: None,
        };

        let hysteresis = Duration::from_secs(1);
        let mut read_hot_until = None;
        assert!(!WalOwnerIngressWorker::mixed_dispatch_is_immediate(
            false,
            &[pending(ZCNBLK_SHM_OP_WRITE), pending(ZCNBLK_SHM_OP_WRITE)],
            &mut read_hot_until,
            hysteresis,
        ));
        assert!(read_hot_until.is_none());

        assert!(WalOwnerIngressWorker::mixed_dispatch_is_immediate(
            true,
            &[pending(ZCNBLK_SHM_OP_WRITE), pending(ZCNBLK_SHM_OP_READ)],
            &mut read_hot_until,
            hysteresis,
        ));
        assert!(read_hot_until.is_some());
        assert!(WalOwnerIngressWorker::mixed_dispatch_is_immediate(
            false,
            &[pending(ZCNBLK_SHM_OP_WRITE)],
            &mut read_hot_until,
            hysteresis,
        ));

        read_hot_until = Instant::now().checked_sub(Duration::from_nanos(1));
        assert!(!WalOwnerIngressWorker::mixed_dispatch_is_immediate(
            false,
            &[pending(ZCNBLK_SHM_OP_WRITE)],
            &mut read_hot_until,
            hysteresis,
        ));

        read_hot_until = None;
        assert!(!WalOwnerIngressWorker::mixed_dispatch_is_immediate(
            false,
            &[pending(ZCNBLK_SHM_OP_WRITE), pending(ZCNBLK_SHM_OP_READ)],
            &mut read_hot_until,
            hysteresis,
        ));
        assert!(read_hot_until.is_none());
    }

    #[test]
    fn wal_completion_tracker_rejects_live_ring_collisions() {
        let tracker = WalCompletionTracker::new(2).unwrap();

        tracker.mark_complete(2).unwrap();
        let error = tracker.mark_complete(4).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
