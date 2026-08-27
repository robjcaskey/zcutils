//! Online userspace WAL-volume migration primitives.
//!
//! The client block edge remains `/dev/zcnblk0`; this stage is downstream of
//! that edge and owns placement. A TCP base copy can splice payload bytes
//! source-socket -> pipe -> destination-socket without bringing them into a
//! userspace buffer. Concurrent foreground writes retain ownership of their
//! already-received WAL payload allocation and are replayed in lane order.

use crate::block::zcnblk::{ZCNBLK_OP_READ, ZCNBLK_OP_SYNC, ZCNBLK_OP_WRITE};
use crate::migration_cache::{RouteHwmMailbox, RouteHwmSnapshot};
use crate::volume_system_policy::{
    SystemTaskGrantMailbox, SystemTaskGrantSnapshot, monotonic_time_ns,
};
use crate::wal_contract::ZCNBLK_WAL_FEATURE_ALL;
use crate::wal_failover::{
    Endpoint, connect_leaf, is_write, read_frame, read_frame_header, write_frame,
};
use crate::{
    ZCNBLK_FAN_WAL_FLAG_DIRECT_MEMORY_WRITE_LAYOUT, ZCNBLK_FAN_WAL_FLAG_IO_CONTRACT_NEGOTIATION,
    ZCNBLK_FAN_WAL_FLAG_OFI_RMA_READ_WINDOW, ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_PAYLOAD,
    ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_WINDOW, ZCNBLK_FAN_WAL_FLAG_RESULT_RANGE_BATCH,
    ZCNBLK_FAN_WAL_HEADER_LEN, ZCNBLK_FAN_WAL_OP_EOF, ZCNBLK_FAN_WAL_OP_HELLO,
    ZCNBLK_FAN_WAL_OP_HELLO_ACK, ZCNBLK_FAN_WAL_OP_READ_DESC, ZCNBLK_FAN_WAL_OP_REQUEST_BATCH,
    ZCNBLK_FAN_WAL_OP_RESULT, ZCNBLK_FAN_WAL_OP_RESULT_BATCH, ZCNBLK_FAN_WAL_OP_RESULT_RANGE_BATCH,
    ZCNBLK_FAN_WAL_OP_SYNC, ZCNBLK_FAN_WAL_OP_WRITE_BATCH, ZCNBLK_FAN_WAL_OP_WRITE_DESC,
    ZCNBLK_FAN_WAL_OP_WRITE_EXTENT_BATCH, ZCNBLK_FAN_WAL_STATUS_OK, ZcOfiMessageStream,
    ZcnblkFanWalFrame,
};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::mem::MaybeUninit;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const IO_ALIGNMENT: u64 = 4096;
const CUTOVER_WAKE_SIGNAL: libc::c_int = libc::SIGUSR1;

extern "C" fn cutover_wake_handler(_signal: libc::c_int) {}

fn install_cutover_wake_handler() -> io::Result<()> {
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = cutover_wake_handler as *const () as usize;
    action.sa_flags = 0;
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0
        || unsafe { libc::sigaction(CUTOVER_WAKE_SIGNAL, &action, std::ptr::null_mut()) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn migration_env_enabled(name: &str) -> bool {
    env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn migration_cpu_list(name: &str) -> io::Result<Option<Vec<usize>>> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| crate::parse_cpu_list(&value))
        .transpose()
}

pub(crate) fn pin_migration_role(role: &str, lane: u32) -> io::Result<Option<usize>> {
    let name = match role {
        "proxy" => "ZCNBLK_WAL_MIGRATION_PROXY_CPU_LIST",
        "copy" => "ZCNBLK_WAL_MIGRATION_COPY_CPU_LIST",
        _ => return Err(invalid("unknown migration CPU role")),
    };
    let Some(cpus) = migration_cpu_list(name)? else {
        if migration_env_enabled("URING_PLAY_TOPOLOGY_STRICT")
            || migration_env_enabled("URING_PLAY_TOPOLOGY_FATAL")
        {
            return Err(io::Error::other(format!(
                "TOPOLOGY ERROR: {name} is required for a strict live-migration run"
            )));
        }
        return Ok(None);
    };
    let cpu = *cpus
        .get(lane as usize)
        .ok_or_else(|| invalid(format!("{name} has no CPU for lane {lane}")))?;
    crate::set_current_thread_affinity(cpu)?;
    eprintln!("zcnblk-wal-live-migration-affinity: role={role} lane={lane} cpu={cpu} applied=true");
    Ok(Some(cpu))
}

fn migration_topology_preflight(lanes: u32, transport: &LiveMigrationTransport) -> io::Result<()> {
    let proxy = migration_cpu_list("ZCNBLK_WAL_MIGRATION_PROXY_CPU_LIST")?;
    let copy = migration_cpu_list("ZCNBLK_WAL_MIGRATION_COPY_CPU_LIST")?;
    let strict = migration_env_enabled("URING_PLAY_TOPOLOGY_STRICT")
        || migration_env_enabled("URING_PLAY_TOPOLOGY_FATAL");
    for (role, cpus) in [("proxy", proxy.as_ref()), ("copy", copy.as_ref())] {
        if cpus.is_none_or(|cpus| cpus.len() < lanes as usize) {
            let message =
                format!("live migration {role} CPU map is missing or shorter than lanes={lanes}");
            if strict {
                return Err(io::Error::other(format!("TOPOLOGY ERROR: {message}")));
            }
            eprintln!("PERF WARNING: {message}; results are not representative");
        }
    }
    if let (Some(proxy), Some(copy)) = (&proxy, &copy) {
        let overlap = proxy
            .iter()
            .take(lanes as usize)
            .any(|cpu| copy.iter().take(lanes as usize).any(|other| other == cpu));
        if overlap {
            let message = "live migration proxy and copy CPU maps overlap";
            if strict {
                return Err(io::Error::other(format!("TOPOLOGY ERROR: {message}")));
            }
            eprintln!("PERF WARNING: {message}; foreground and system work may contend");
        }
    }
    if matches!(transport, LiveMigrationTransport::Ofi(_))
        && !migration_env_enabled("URING_PLAY_ZCNBLK_WAL_OFI_HUGETLB_CONFIRMED")
    {
        let message = "OFI migration registered-arena hugetlb/memlock topology is not confirmed";
        if strict {
            return Err(io::Error::other(format!("TOPOLOGY ERROR: {message}")));
        }
        eprintln!("PERF WARNING: {message}; results are not representative");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TcpBulkCopyMethod {
    /// Linux socket -> pipe -> socket splice. Payload bytes never enter a
    /// userspace buffer and there is no payload allocation in the copy loop.
    #[default]
    Splice,
    /// Compatibility path for kernels/transports which cannot splice.
    Buffered,
}

#[derive(Clone, Copy, Debug)]
pub struct WalBaseCopySpec {
    pub volume_bytes: u64,
    pub chunk_bytes: usize,
    pub lane_id: u32,
    pub lane_count: u32,
    pub method: TcpBulkCopyMethod,
}

impl WalBaseCopySpec {
    fn validate(self, hello: ZcnblkFanWalFrame) -> io::Result<()> {
        if self.volume_bytes == 0 || self.volume_bytes % IO_ALIGNMENT != 0 {
            return Err(invalid(
                "WAL migration volume size must be a non-zero 4096 multiple",
            ));
        }
        if self.chunk_bytes == 0
            || self.chunk_bytes as u64 % IO_ALIGNMENT != 0
            || self.chunk_bytes > u32::MAX as usize
        {
            return Err(invalid(
                "WAL migration chunk must be a non-zero 4096 multiple no larger than u32::MAX",
            ));
        }
        if self.lane_count == 0
            || self.lane_id >= self.lane_count
            || hello.lane_id != self.lane_id
            || hello.lane_count != self.lane_count
        {
            return Err(invalid("WAL migration lane topology does not match HELLO"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalBaseCopyStats {
    pub bytes_copied: u64,
    pub ranges_copied: u64,
    pub payload_userspace_buffers: u64,
    pub splice_payload_syscalls: u64,
    pub elapsed_ns: u64,
}

pub struct ManagedWalCopy<'a> {
    pub grants: &'a SystemTaskGrantMailbox,
    pub cancelled: &'a AtomicBool,
    pub idle_wait: Duration,
}

impl ManagedWalCopy<'_> {
    fn wait_for_grant(&self) -> io::Result<SystemTaskGrantSnapshot> {
        if self.idle_wait.is_zero() {
            return Err(invalid("managed WAL-copy idle wait must be non-zero"));
        }
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "managed WAL base copy cancelled at a range boundary",
                ));
            }
            let grant = self.grants.load(monotonic_time_ns()?);
            if grant.target_iops != 0 && grant.target_bytes_per_second != 0 {
                return Ok(grant);
            }
            thread::park_timeout(self.idle_wait);
        }
    }

    fn sleep_interruptible(&self, mut duration: Duration) -> io::Result<()> {
        while !duration.is_zero() {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "managed WAL base copy cancelled while pacing a range",
                ));
            }
            let slice = duration.min(self.idle_wait);
            thread::park_timeout(slice);
            duration = duration.saturating_sub(slice);
        }
        Ok(())
    }
}

/// Lane-local absolute-deadline pacer for system-copy chunks. It deliberately
/// lives outside foreground request admission: one grant read and, at most,
/// one sleep happen per migration chunk. The initial phase spreads sibling
/// lanes across one chunk interval instead of releasing a synchronized burst.
struct ManagedChunkPacer {
    lane_id: u32,
    lane_count: u32,
    generation: u64,
    epoch: Option<Instant>,
    scheduled_bytes: u128,
}

impl ManagedChunkPacer {
    fn new(lane_id: u32, lane_count: u32) -> Self {
        Self {
            lane_id,
            lane_count,
            generation: 0,
            epoch: None,
            scheduled_bytes: 0,
        }
    }

    fn before_range(&mut self, managed: &ManagedWalCopy<'_>, bytes: u64) -> io::Result<u64> {
        let grant = managed.wait_for_grant()?;
        if self.generation != grant.generation || self.epoch.is_none() {
            let range_period = paced_duration(bytes.into(), grant.target_bytes_per_second);
            let now_ns = monotonic_time_ns()?;
            let delay = initial_lane_pacing_delay(
                grant.effective_ns,
                now_ns,
                range_period,
                self.lane_id,
                self.lane_count,
            );
            managed.sleep_interruptible(delay)?;
            self.generation = grant.generation;
            self.epoch = Some(Instant::now());
            self.scheduled_bytes = 0;
        }
        Ok(grant.target_bytes_per_second)
    }

    fn after_range(
        &mut self,
        managed: &ManagedWalCopy<'_>,
        bytes: u64,
        bytes_per_second: u64,
    ) -> io::Result<()> {
        self.scheduled_bytes = self.scheduled_bytes.saturating_add(bytes.into());
        let deadline = paced_duration(self.scheduled_bytes, bytes_per_second);
        if let Some(delay) = deadline.checked_sub(
            self.epoch
                .expect("managed pacer epoch initialized before copy")
                .elapsed(),
        ) {
            managed.sleep_interruptible(delay)?;
        }
        Ok(())
    }
}

/// Copy this lane's disjoint range stripes between two terminal userspace WAL
/// leaves. The function creates dedicated migration sessions; it never changes
/// foreground routing and never asks a block device to mirror or stripe data.
pub(crate) fn tcp_base_copy_lane(
    source_endpoint: &Endpoint,
    destination_endpoint: &Endpoint,
    hello: ZcnblkFanWalFrame,
    spec: WalBaseCopySpec,
    managed: Option<&ManagedWalCopy<'_>>,
) -> io::Result<WalBaseCopyStats> {
    spec.validate(hello)?;
    let mut source = connect_leaf(source_endpoint, spec.lane_id)?;
    let mut destination = connect_leaf(destination_endpoint, spec.lane_id)?;
    tcp_base_copy_streams(&mut source, &mut destination, hello, spec, managed)
}

fn tcp_base_copy_streams(
    source: &mut TcpStream,
    destination: &mut TcpStream,
    hello: ZcnblkFanWalFrame,
    spec: WalBaseCopySpec,
    managed: Option<&ManagedWalCopy<'_>>,
) -> io::Result<WalBaseCopyStats> {
    spec.validate(hello)?;
    let source_ack = send_hello(source, hello)?;
    let destination_ack = send_hello(destination, hello)?;
    if source_ack.request_id != destination_ack.request_id
        || source_ack.placement_epoch != destination_ack.placement_epoch
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "migration source and destination admitted different placement plans",
        ));
    }

    let pipe = (spec.method == TcpBulkCopyMethod::Splice)
        .then(Pipe::new)
        .transpose()?;
    let started = Instant::now();
    let mut stats = WalBaseCopyStats::default();
    let stride = (spec.chunk_bytes as u64)
        .checked_mul(u64::from(spec.lane_count))
        .ok_or_else(|| invalid("WAL migration lane stride overflow"))?;
    let mut offset = (spec.chunk_bytes as u64)
        .checked_mul(u64::from(spec.lane_id))
        .ok_or_else(|| invalid("WAL migration lane offset overflow"))?;
    let mut range_sequence = 1u64;
    let mut pacer = managed.map(|_| ManagedChunkPacer::new(spec.lane_id, spec.lane_count));
    while offset < spec.volume_bytes {
        let len = spec.chunk_bytes.min((spec.volume_bytes - offset) as usize);
        let rate = match (managed, pacer.as_mut()) {
            (Some(control), Some(pacer)) => pacer.before_range(control, len as u64)?,
            _ => 0,
        };
        let read = migration_descriptor(
            hello,
            ZCNBLK_FAN_WAL_OP_READ_DESC,
            ZCNBLK_OP_READ,
            offset,
            len,
            range_sequence,
        )?;
        write_frame(source, read, &[])?;
        match spec.method {
            TcpBulkCopyMethod::Splice => {
                let result = read_frame_header(source)?;
                validate_copy_result(result, read, len)?;
                let write = migration_descriptor(
                    hello,
                    ZCNBLK_FAN_WAL_OP_WRITE_DESC,
                    ZCNBLK_OP_WRITE,
                    offset,
                    len,
                    range_sequence,
                )?;
                destination.write_all(&write.encode())?;
                stats.splice_payload_syscalls = stats.splice_payload_syscalls.saturating_add(
                    pipe.as_ref()
                        .expect("splice method has a pipe")
                        .splice_exact(&source, &destination, len)?,
                );
                let result = read_frame_header(destination)?;
                validate_copy_result(result, write, 0)?;
            }
            TcpBulkCopyMethod::Buffered => {
                let (result, payload) = read_frame(source)?;
                validate_copy_result(result, read, len)?;
                let write = migration_descriptor(
                    hello,
                    ZCNBLK_FAN_WAL_OP_WRITE_DESC,
                    ZCNBLK_OP_WRITE,
                    offset,
                    len,
                    range_sequence,
                )?;
                write_frame(destination, write, &payload)?;
                let (result, payload) = read_frame(destination)?;
                validate_copy_result(result, write, 0)?;
                if !payload.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "migration destination write returned an unexpected payload",
                    ));
                }
                stats.payload_userspace_buffers = stats.payload_userspace_buffers.saturating_add(1);
            }
        }
        stats.bytes_copied = stats.bytes_copied.saturating_add(len as u64);
        stats.ranges_copied = stats.ranges_copied.saturating_add(1);
        if let (Some(control), Some(pacer)) = (managed, pacer.as_mut()) {
            pacer.after_range(control, len as u64, rate)?;
        }
        offset = offset
            .checked_add(stride)
            .ok_or_else(|| invalid("WAL migration offset overflow"))?;
        range_sequence = range_sequence.saturating_add(1);
    }
    stats.elapsed_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    Ok(stats)
}

/// Preconnected off-path TCP copier used by the direct-route migration
/// controller. It owns no foreground socket. Every payload transfer is
/// socket -> pipe -> socket when `Splice` is selected, including final dirty
/// replay while the block edge is frozen.
pub(crate) struct TcpWalRangeCopier {
    source: TcpStream,
    destination: TcpStream,
    hello: ZcnblkFanWalFrame,
    method: TcpBulkCopyMethod,
    pipe: Option<Pipe>,
    next_sequence: u64,
}

impl TcpWalRangeCopier {
    pub(crate) fn connect(
        source_endpoint: &Endpoint,
        destination_endpoint: &Endpoint,
        hello: ZcnblkFanWalFrame,
        spec: WalBaseCopySpec,
    ) -> io::Result<Self> {
        spec.validate(hello)?;
        let mut source = connect_leaf(source_endpoint, spec.lane_id)?;
        let mut destination = connect_leaf(destination_endpoint, spec.lane_id)?;
        let source_ack = send_hello(&mut source, hello)?;
        let destination_ack = send_hello(&mut destination, hello)?;
        if source_ack.request_id != destination_ack.request_id
            || source_ack.placement_epoch != destination_ack.placement_epoch
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "direct migration source and destination admitted different placement plans",
            ));
        }
        Ok(Self {
            source,
            destination,
            hello,
            method: spec.method,
            pipe: (spec.method == TcpBulkCopyMethod::Splice)
                .then(Pipe::new)
                .transpose()?,
            next_sequence: 1,
        })
    }

    fn copy_one(&mut self, offset: u64, len: usize) -> io::Result<WalBaseCopyStats> {
        if len == 0
            || offset % IO_ALIGNMENT != 0
            || len as u64 % IO_ALIGNMENT != 0
            || len > u32::MAX as usize
        {
            return Err(invalid(
                "direct migration copy range must be a non-zero aligned u32-sized range",
            ));
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "direct migration range sequence overflow",
            )
        })?;
        let read = migration_descriptor(
            self.hello,
            ZCNBLK_FAN_WAL_OP_READ_DESC,
            ZCNBLK_OP_READ,
            offset,
            len,
            sequence,
        )?;
        write_frame(&mut self.source, read, &[])?;
        let mut stats = WalBaseCopyStats {
            bytes_copied: len as u64,
            ranges_copied: 1,
            ..WalBaseCopyStats::default()
        };
        match self.method {
            TcpBulkCopyMethod::Splice => {
                let result = read_frame_header(&mut self.source)?;
                validate_copy_result(result, read, len)?;
                let write = migration_descriptor(
                    self.hello,
                    ZCNBLK_FAN_WAL_OP_WRITE_DESC,
                    ZCNBLK_OP_WRITE,
                    offset,
                    len,
                    sequence,
                )?;
                self.destination.write_all(&write.encode())?;
                stats.splice_payload_syscalls = self
                    .pipe
                    .as_ref()
                    .expect("splice copier has a pipe")
                    .splice_exact(&self.source, &self.destination, len)?;
                let result = read_frame_header(&mut self.destination)?;
                validate_copy_result(result, write, 0)?;
            }
            TcpBulkCopyMethod::Buffered => {
                let (result, payload) = read_frame(&mut self.source)?;
                validate_copy_result(result, read, len)?;
                let write = migration_descriptor(
                    self.hello,
                    ZCNBLK_FAN_WAL_OP_WRITE_DESC,
                    ZCNBLK_OP_WRITE,
                    offset,
                    len,
                    sequence,
                )?;
                write_frame(&mut self.destination, write, &payload)?;
                let (result, payload) = read_frame(&mut self.destination)?;
                validate_copy_result(result, write, 0)?;
                if !payload.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "direct migration destination write returned payload",
                    ));
                }
                stats.payload_userspace_buffers = 1;
            }
        }
        Ok(stats)
    }

    pub(crate) fn copy_striped_base(
        &mut self,
        spec: WalBaseCopySpec,
        managed: Option<&ManagedWalCopy<'_>>,
    ) -> io::Result<WalBaseCopyStats> {
        spec.validate(self.hello)?;
        let started = Instant::now();
        let stride = (spec.chunk_bytes as u64)
            .checked_mul(u64::from(spec.lane_count))
            .ok_or_else(|| invalid("direct migration lane stride overflow"))?;
        let mut offset = (spec.chunk_bytes as u64)
            .checked_mul(u64::from(spec.lane_id))
            .ok_or_else(|| invalid("direct migration lane offset overflow"))?;
        let mut stats = WalBaseCopyStats::default();
        let mut pacer = managed.map(|_| ManagedChunkPacer::new(spec.lane_id, spec.lane_count));
        while offset < spec.volume_bytes {
            let len = spec.chunk_bytes.min((spec.volume_bytes - offset) as usize);
            let rate = match (managed, pacer.as_mut()) {
                (Some(control), Some(pacer)) => pacer.before_range(control, len as u64)?,
                _ => 0,
            };
            let copied = self.copy_one(offset, len)?;
            add_tcp_copy_stats(&mut stats, copied);
            if let (Some(control), Some(pacer)) = (managed, pacer.as_mut()) {
                pacer.after_range(control, len as u64, rate)?;
            }
            offset = offset
                .checked_add(stride)
                .ok_or_else(|| invalid("direct migration lane offset overflow"))?;
        }
        stats.elapsed_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        Ok(stats)
    }

    pub(crate) fn copy_ranges(
        &mut self,
        ranges: &[(u64, usize)],
        chunk_bytes: usize,
    ) -> io::Result<WalBaseCopyStats> {
        if chunk_bytes == 0 || chunk_bytes as u64 % IO_ALIGNMENT != 0 {
            return Err(invalid(
                "direct migration replay chunk must be a non-zero 4096 multiple",
            ));
        }
        let started = Instant::now();
        let mut stats = WalBaseCopyStats::default();
        for &(range_offset, range_len) in ranges {
            let mut offset = range_offset;
            let mut remaining = range_len;
            while remaining != 0 {
                let len = remaining.min(chunk_bytes).min(u32::MAX as usize);
                let copied = self.copy_one(offset, len)?;
                add_tcp_copy_stats(&mut stats, copied);
                offset = offset
                    .checked_add(len as u64)
                    .ok_or_else(|| invalid("direct migration replay offset overflow"))?;
                remaining -= len;
            }
        }
        stats.elapsed_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        Ok(stats)
    }

    /// Durability proof is obtained only after all lanes have finished their
    /// final range replay. The caller retains the returned value as the
    /// destination's per-owner cutover HWM proof.
    pub(crate) fn sync_destination(&mut self, hwm: u64) -> io::Result<u64> {
        let sync = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_SYNC,
            lane_id: self.hello.lane_id,
            lane_count: self.hello.lane_count,
            branch_id: self.hello.branch_id,
            branch_count: self.hello.branch_count,
            sequence: hwm,
            sync_epoch: hwm,
            request_id: self.hello.request_id,
            placement_epoch: self.hello.placement_epoch,
            ..ZcnblkFanWalFrame::default()
        };
        write_frame(&mut self.destination, sync, &[])?;
        let (result, payload) = read_frame(&mut self.destination)?;
        if result.op != ZCNBLK_FAN_WAL_OP_RESULT
            || result.status != ZCNBLK_FAN_WAL_STATUS_OK
            || result.sequence != hwm
            || !payload.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "direct migration destination sync did not prove the requested HWM",
            ));
        }
        Ok(hwm)
    }
}

fn add_tcp_copy_stats(total: &mut WalBaseCopyStats, next: WalBaseCopyStats) {
    total.bytes_copied = total.bytes_copied.saturating_add(next.bytes_copied);
    total.ranges_copied = total.ranges_copied.saturating_add(next.ranges_copied);
    total.payload_userspace_buffers = total
        .payload_userspace_buffers
        .saturating_add(next.payload_userspace_buffers);
    total.splice_payload_syscalls = total
        .splice_payload_syscalls
        .saturating_add(next.splice_payload_syscalls);
}

#[derive(Clone, Debug)]
pub struct OfiWalEndpoint {
    pub node: String,
    pub base_service: u16,
    pub domain: Option<String>,
}

#[derive(Clone, Debug)]
pub struct OfiBaseCopyConfig {
    pub provider: String,
    pub endpoint: String,
    pub source: OfiWalEndpoint,
    pub destination: OfiWalEndpoint,
    /// Connection zero is conventionally foreground; one is base copy and
    /// two is retained-tail replay.
    pub connection_index: u32,
}

#[derive(Clone, Debug)]
struct OfiLiveTransport {
    provider: String,
    endpoint: String,
    /// The client-facing stream is framed by default.  Terminal leaf
    /// sessions remain RMA capable independently so an intermediary never
    /// implies that its upstream peer can use a terminal leaf's MR key.
    ingress_rma_capable: bool,
    ingress_domain: Option<String>,
    source_domain: Option<String>,
    destination_domain: Option<String>,
}

#[derive(Clone, Debug)]
enum LiveMigrationTransport {
    Tcp,
    Ofi(OfiLiveTransport),
}

#[derive(Clone, Copy)]
enum LiveEndpointRole {
    Source,
    Destination,
}

enum LiveWalStream {
    Tcp(TcpStream),
    Ofi(ZcOfiMessageStream),
}

struct PreparedSystemSessions {
    base_source: LiveWalStream,
    base_destination: LiveWalStream,
    replay_destination: LiveWalStream,
}

impl LiveWalStream {
    fn set_low_latency(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.set_nodelay(true),
            Self::Ofi(_) => Ok(()),
        }
    }
}

impl From<TcpStream> for LiveWalStream {
    fn from(stream: TcpStream) -> Self {
        Self::Tcp(stream)
    }
}

impl Read for LiveWalStream {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(output),
            Self::Ofi(stream) => stream.read(output),
        }
    }
}

impl Write for LiveWalStream {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(input),
            Self::Ofi(stream) => stream.write(input),
        }
    }

    fn write_vectored(&mut self, input: &[std::io::IoSlice<'_>]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write_vectored(input),
            Self::Ofi(stream) => stream.write_vectored(input),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            Self::Ofi(stream) => stream.flush(),
        }
    }
}

impl LiveMigrationTransport {
    fn label(&self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Ofi(_) => "ofi",
        }
    }

    fn connect_leaf(
        &self,
        endpoint: &Endpoint,
        lane: u32,
        lanes: u32,
        connection_index: u32,
        role: LiveEndpointRole,
    ) -> io::Result<LiveWalStream> {
        match self {
            Self::Tcp => connect_leaf(endpoint, lane).map(LiveWalStream::Tcp),
            Self::Ofi(ofi) => {
                let service =
                    ofi_connection_service(endpoint.base_port, lane, lanes, connection_index)?;
                let domain = match role {
                    LiveEndpointRole::Source => ofi.source_domain.as_deref(),
                    LiveEndpointRole::Destination => ofi.destination_domain.as_deref(),
                };
                connect_ofi_leaf_retry(
                    &ofi.provider,
                    &ofi.endpoint,
                    &endpoint.host,
                    service,
                    false,
                    true,
                    domain,
                )
                .map(LiveWalStream::Ofi)
            }
        }
    }

    fn prepare_system_sessions(
        &self,
        source: &Endpoint,
        destination: &Endpoint,
        lane: u32,
        lanes: u32,
    ) -> io::Result<Option<PreparedSystemSessions>> {
        let (base_source, base_destination, replay_destination) = match self {
            Self::Tcp => (
                LiveWalStream::Tcp(connect_leaf(source, lane)?),
                LiveWalStream::Tcp(connect_leaf(destination, lane)?),
                LiveWalStream::Tcp(connect_leaf(destination, lane)?),
            ),
            Self::Ofi(ofi) => {
                let base_source_service = ofi_connection_service(source.base_port, lane, lanes, 1)?;
                let base_destination_service =
                    ofi_connection_service(destination.base_port, lane, lanes, 1)?;
                let replay_destination_service =
                    ofi_connection_service(destination.base_port, lane, lanes, 2)?;
                (
                    LiveWalStream::Ofi(connect_ofi_leaf_retry(
                        &ofi.provider,
                        &ofi.endpoint,
                        &source.host,
                        base_source_service,
                        false,
                        true,
                        ofi.source_domain.as_deref(),
                    )?),
                    LiveWalStream::Ofi(connect_ofi_leaf_retry(
                        &ofi.provider,
                        &ofi.endpoint,
                        &destination.host,
                        base_destination_service,
                        false,
                        true,
                        ofi.destination_domain.as_deref(),
                    )?),
                    LiveWalStream::Ofi(connect_ofi_leaf_retry(
                        &ofi.provider,
                        &ofi.endpoint,
                        &destination.host,
                        replay_destination_service,
                        false,
                        true,
                        ofi.destination_domain.as_deref(),
                    )?),
                )
            }
        };
        Ok(Some(PreparedSystemSessions {
            base_source,
            base_destination,
            replay_destination,
        }))
    }

    fn base_copy(
        &self,
        _source: &Endpoint,
        _destination: &Endpoint,
        hello: ZcnblkFanWalFrame,
        copy: WalBaseCopySpec,
        managed: &ManagedWalCopy<'_>,
        prepared: Option<&mut PreparedSystemSessions>,
    ) -> io::Result<u64> {
        match self {
            Self::Tcp => {
                let prepared = prepared.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "TCP migration system sessions were not admitted with the foreground lane",
                    )
                })?;
                let (LiveWalStream::Tcp(source), LiveWalStream::Tcp(destination)) =
                    (&mut prepared.base_source, &mut prepared.base_destination)
                else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "TCP migration received non-TCP prepared sessions",
                    ));
                };
                tcp_base_copy_streams(source, destination, hello, copy, Some(managed))
                    .map(|stats| stats.bytes_copied)
            }
            Self::Ofi(_) => {
                let prepared = prepared.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "OFI migration system sessions were not admitted with the foreground lane",
                    )
                })?;
                let (LiveWalStream::Ofi(source), LiveWalStream::Ofi(destination)) =
                    (&mut prepared.base_source, &mut prepared.base_destination)
                else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "OFI migration received non-OFI prepared sessions",
                    ));
                };
                ofi_rma_base_copy_streams(source, destination, hello, copy, Some(managed))
                    .map(|stats| stats.bytes_copied)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn connect_ofi_leaf_retry(
    provider: &str,
    endpoint: &str,
    node: &str,
    service: u16,
    server: bool,
    rma_capable: bool,
    domain: Option<&str>,
) -> io::Result<ZcOfiMessageStream> {
    let timeout = Duration::from_millis(
        env::var("ZCNBLK_WAL_MIGRATION_CONNECT_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(10_000),
    );
    let started = Instant::now();
    let mut retries = 0u64;
    loop {
        match ZcOfiMessageStream::connect_on_domain(
            provider,
            endpoint,
            node,
            service,
            server,
            rma_capable,
            domain,
        ) {
            Ok(stream) => {
                if retries != 0 {
                    eprintln!(
                        "zcnblk-wal-live-migration-ofi-connect: node={node} service={service} retries={retries} elapsed_ms={} status=connected",
                        started.elapsed().as_millis(),
                    );
                }
                return Ok(stream);
            }
            Err(error)
                if !server
                    && started.elapsed() < timeout
                    && matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::NotConnected
                            | io::ErrorKind::TimedOut
                    ) =>
            {
                retries = retries.saturating_add(1);
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OfiBaseCopyStats {
    pub bytes_copied: u64,
    pub ranges_copied: u64,
    pub registered_staging_arenas: u64,
    pub payload_cpu_copies: u64,
    pub rma_reads: u64,
    pub rma_writes: u64,
    pub remote_hwm_doorbells: u64,
    pub elapsed_ns: u64,
}

/// RDMA base copy using one registered, lane-local staging arena. Remote to
/// remote transfer necessarily lands somewhere between NICs; this is the
/// theoretical minimum-copy path: source NIC DMA into the arena, then
/// destination NIC DMA from the same bytes, with no CPU memcpy or allocation
/// in the range loop. A metadata doorbell obtains remote applied-HWM proof for
/// every destination write.
pub(crate) fn ofi_rma_base_copy_lane(
    config: &OfiBaseCopyConfig,
    hello: ZcnblkFanWalFrame,
    spec: WalBaseCopySpec,
    managed: Option<&ManagedWalCopy<'_>>,
) -> io::Result<OfiBaseCopyStats> {
    spec.validate(hello)?;
    let source_service = ofi_connection_service(
        config.source.base_service,
        spec.lane_id,
        spec.lane_count,
        config.connection_index,
    )?;
    let destination_service = ofi_connection_service(
        config.destination.base_service,
        spec.lane_id,
        spec.lane_count,
        config.connection_index,
    )?;
    let mut source = ZcOfiMessageStream::connect_on_domain(
        &config.provider,
        &config.endpoint,
        &config.source.node,
        source_service,
        false,
        true,
        config.source.domain.as_deref(),
    )?;
    let mut destination = ZcOfiMessageStream::connect_on_domain(
        &config.provider,
        &config.endpoint,
        &config.destination.node,
        destination_service,
        false,
        true,
        config.destination.domain.as_deref(),
    )?;
    ofi_rma_base_copy_streams(&mut source, &mut destination, hello, spec, managed)
}

fn ofi_rma_base_copy_streams(
    source: &mut ZcOfiMessageStream,
    destination: &mut ZcOfiMessageStream,
    hello: ZcnblkFanWalFrame,
    spec: WalBaseCopySpec,
    managed: Option<&ManagedWalCopy<'_>>,
) -> io::Result<OfiBaseCopyStats> {
    spec.validate(hello)?;
    let mut rma_hello = hello.with_hello_features(ZCNBLK_WAL_FEATURE_ALL)?;
    rma_hello.flags |= ZCNBLK_FAN_WAL_FLAG_IO_CONTRACT_NEGOTIATION
        | ZCNBLK_FAN_WAL_FLAG_RESULT_RANGE_BATCH
        | ZCNBLK_FAN_WAL_FLAG_OFI_RMA_READ_WINDOW
        | ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_WINDOW;
    let source_ack = send_stream_hello(source, rma_hello)?;
    let destination_ack = send_stream_hello(destination, rma_hello)?;
    if source_ack.flags & ZCNBLK_FAN_WAL_FLAG_OFI_RMA_READ_WINDOW == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "source leaf did not negotiate an OFI RMA read window",
        ));
    }
    if destination_ack.flags & ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_WINDOW == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "destination leaf did not negotiate an OFI RMA write window",
        ));
    }
    if source_ack.sync_epoch < spec.volume_bytes || destination_ack.sync_epoch < spec.volume_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "RMA migration window is smaller than the volume: source={} destination={} volume={}",
                source_ack.sync_epoch, destination_ack.sync_epoch, spec.volume_bytes
            ),
        ));
    }

    // This allocation is outside the range loop and is registered with both
    // endpoint domains. The pointer cannot move while either MR exists.
    let mut arena = vec![0u8; spec.chunk_bytes];
    source.register_rma_read_buffer(&mut arena)?;
    destination.register_rma_write_buffer(&arena)?;
    let started = Instant::now();
    let mut stats = OfiBaseCopyStats {
        registered_staging_arenas: 1,
        ..OfiBaseCopyStats::default()
    };
    let stride = (spec.chunk_bytes as u64)
        .checked_mul(u64::from(spec.lane_count))
        .ok_or_else(|| invalid("OFI migration lane stride overflow"))?;
    let mut offset = (spec.chunk_bytes as u64)
        .checked_mul(u64::from(spec.lane_id))
        .ok_or_else(|| invalid("OFI migration lane offset overflow"))?;
    let mut range_sequence = 1u64;
    let mut pacer = managed.map(|_| ManagedChunkPacer::new(spec.lane_id, spec.lane_count));
    while offset < spec.volume_bytes {
        let len = spec.chunk_bytes.min((spec.volume_bytes - offset) as usize);
        let rate = match (managed, pacer.as_mut()) {
            (Some(control), Some(pacer)) => pacer.before_range(control, len as u64)?,
            _ => 0,
        };
        let source_addr = source_ack
            .logical_offset
            .checked_add(offset)
            .ok_or_else(|| invalid("source RMA address overflow"))?;
        let destination_addr = destination_ack
            .logical_offset
            .checked_add(offset)
            .ok_or_else(|| invalid("destination RMA address overflow"))?;
        source.rma_read(&mut arena[..len], source_addr, source_ack.leaf_offset)?;
        stats.rma_reads = stats.rma_reads.saturating_add(1);
        destination.rma_write(&arena[..len], destination_addr, destination_ack.leaf_offset)?;
        stats.rma_writes = stats.rma_writes.saturating_add(1);

        let descriptor = migration_descriptor(
            rma_hello,
            ZCNBLK_FAN_WAL_OP_WRITE_DESC,
            ZCNBLK_OP_WRITE,
            offset,
            len,
            range_sequence,
        )?;
        let doorbell = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_REQUEST_BATCH,
            flags: ZCNBLK_FAN_WAL_FLAG_DIRECT_MEMORY_WRITE_LAYOUT
                | ZCNBLK_FAN_WAL_FLAG_RESULT_RANGE_BATCH
                | ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_PAYLOAD,
            lane_id: rma_hello.lane_id,
            lane_count: rma_hello.lane_count,
            branch_id: rma_hello.branch_id,
            branch_count: rma_hello.branch_count,
            segment_count: 1,
            payload_len: u32::try_from(ZCNBLK_FAN_WAL_HEADER_LEN + len)
                .map_err(|_| invalid("RMA doorbell payload length exceeds u32"))?,
            sequence: range_sequence,
            request_id: rma_hello.request_id,
            placement_epoch: rma_hello.placement_epoch,
            ..ZcnblkFanWalFrame::default()
        };
        crate::zcnblk_fan_wal_write_rma_payload_doorbell(
            &mut *destination,
            doorbell,
            &descriptor.encode(),
        )?;
        let (result, payload) = read_stream_frame(&mut *destination)?;
        if result.op != ZCNBLK_FAN_WAL_OP_RESULT_RANGE_BATCH
            || result.status != ZCNBLK_FAN_WAL_STATUS_OK
            || !payload.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "destination RMA write did not return a range-HWM acknowledgement",
            ));
        }
        stats.remote_hwm_doorbells = stats.remote_hwm_doorbells.saturating_add(1);
        stats.bytes_copied = stats.bytes_copied.saturating_add(len as u64);
        stats.ranges_copied = stats.ranges_copied.saturating_add(1);
        if let (Some(control), Some(pacer)) = (managed, pacer.as_mut()) {
            pacer.after_range(control, len as u64, rate)?;
        }
        offset = offset
            .checked_add(stride)
            .ok_or_else(|| invalid("OFI migration offset overflow"))?;
        range_sequence = range_sequence.saturating_add(1);
    }
    // FI_RDM has no TCP-style orderly shutdown for the leaf protocol. Release
    // the two preassigned migration connection slots explicitly rather than
    // leaving their workers blocked until the provider receive timeout.
    send_stream_eof(source, rma_hello)?;
    send_stream_eof(destination, rma_hello)?;
    stats.elapsed_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    Ok(stats)
}

fn ofi_connection_service(base: u16, lane: u32, lanes: u32, connection: u32) -> io::Result<u16> {
    let offset = connection
        .checked_mul(lanes)
        .and_then(|value| value.checked_add(lane))
        .ok_or_else(|| invalid("OFI migration service offset overflow"))?;
    base.checked_add(
        u16::try_from(offset).map_err(|_| invalid("OFI migration service exceeds u16"))?,
    )
    .ok_or_else(|| invalid("OFI migration service overflow"))
}

fn send_stream_hello(
    stream: &mut ZcOfiMessageStream,
    hello: ZcnblkFanWalFrame,
) -> io::Result<ZcnblkFanWalFrame> {
    crate::zcnblk_fan_wal_write_frame(stream, hello, &[])?;
    let (ack, payload) = read_stream_frame(stream)?;
    if ack.op != ZCNBLK_FAN_WAL_OP_HELLO_ACK || !payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "OFI migration leaf omitted HELLO_ACK",
        ));
    }
    Ok(ack)
}

fn write_stream_frame<W: Write + ?Sized>(
    stream: &mut W,
    frame: ZcnblkFanWalFrame,
    payload: &[u8],
) -> io::Result<()> {
    crate::zcnblk_fan_wal_write_frame(stream, frame, payload)
}

fn send_live_hello(
    stream: &mut LiveWalStream,
    hello: ZcnblkFanWalFrame,
) -> io::Result<ZcnblkFanWalFrame> {
    write_stream_frame(stream, hello, &[])?;
    let (ack, payload) = read_stream_frame(stream)?;
    if ack.op != ZCNBLK_FAN_WAL_OP_HELLO_ACK || !payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "live migration leaf omitted HELLO_ACK",
        ));
    }
    Ok(ack)
}

fn framed_forwarding_hello(
    mut hello: ZcnblkFanWalFrame,
    transport: &LiveMigrationTransport,
) -> ZcnblkFanWalFrame {
    if matches!(transport, LiveMigrationTransport::Ofi(_)) {
        // An intermediary must never advertise the terminal leaf's RMA key as
        // its own. Until the shared-arena placement stage owns an upstream RMA
        // window, foreground traffic uses framed OFI messages while the
        // dedicated base-copy sessions use RMA directly between leaves.
        hello.flags &=
            !(ZCNBLK_FAN_WAL_FLAG_OFI_RMA_READ_WINDOW | ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_WINDOW);
    }
    hello
}

fn send_stream_eof(stream: &mut ZcOfiMessageStream, hello: ZcnblkFanWalFrame) -> io::Result<()> {
    crate::zcnblk_fan_wal_write_frame(
        stream,
        ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_EOF,
            lane_id: hello.lane_id,
            lane_count: hello.lane_count,
            branch_id: hello.branch_id,
            branch_count: hello.branch_count,
            request_id: hello.request_id,
            placement_epoch: hello.placement_epoch,
            ..ZcnblkFanWalFrame::default()
        },
        &[],
    )
}

fn read_stream_frame<R: Read>(stream: &mut R) -> io::Result<(ZcnblkFanWalFrame, Vec<u8>)> {
    let mut header = [0u8; ZCNBLK_FAN_WAL_HEADER_LEN];
    stream.read_exact(&mut header)?;
    let frame = ZcnblkFanWalFrame::decode(&header)?;
    let payload_len = match frame.op {
        ZCNBLK_FAN_WAL_OP_WRITE_DESC
        | ZCNBLK_FAN_WAL_OP_RESULT
        | ZCNBLK_FAN_WAL_OP_WRITE_BATCH
        | ZCNBLK_FAN_WAL_OP_RESULT_BATCH
        | ZCNBLK_FAN_WAL_OP_REQUEST_BATCH
        | ZCNBLK_FAN_WAL_OP_WRITE_EXTENT_BATCH => frame.payload_len as usize,
        _ => 0,
    };
    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload)?;
    Ok((frame, payload))
}

/// Read an upstream frame while allowing a cutover wake signal to interrupt
/// an idle header read. Once any header byte has arrived, the complete frame
/// is retained before quiescing so a signal can never discard framing state.
fn read_upstream_frame_interruptible(
    stream: &mut LiveWalStream,
    coordinator: &MigrationCoordinator,
) -> io::Result<Option<(ZcnblkFanWalFrame, Vec<u8>)>> {
    let mut header = [0u8; ZCNBLK_FAN_WAL_HEADER_LEN];
    let mut filled = 0usize;
    while filled != header.len() {
        let bytes = match stream {
            LiveWalStream::Tcp(stream) => match stream.read(&mut header[filled..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "live migration upstream closed in a frame header",
                    ));
                }
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted && filled == 0 => {
                    return Ok(None);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            },
            LiveWalStream::Ofi(stream) => {
                let mut spins = 0u32;
                loop {
                    if let Some(bytes) = stream.try_read(&mut header[filled..])? {
                        if bytes == 0 {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "live migration upstream closed in a frame header",
                            ));
                        }
                        break bytes;
                    }
                    if coordinator.phase.load(Ordering::Acquire) == PHASE_CUTOVER_REQUESTED
                        && coordinator.all_base_ready()
                    {
                        return Ok(None);
                    }
                    if coordinator.phase.load(Ordering::Acquire) == PHASE_FAILED {
                        return Err(io::Error::other("migration coordinator failed"));
                    }
                    spins = spins.wrapping_add(1);
                    if spins & 4095 == 0 {
                        thread::yield_now();
                    } else {
                        std::hint::spin_loop();
                    }
                }
            }
        };
        filled += bytes;
    }
    let frame = ZcnblkFanWalFrame::decode(&header)?;
    let payload_len = match frame.op {
        ZCNBLK_FAN_WAL_OP_WRITE_DESC
        | ZCNBLK_FAN_WAL_OP_RESULT
        | ZCNBLK_FAN_WAL_OP_WRITE_BATCH
        | ZCNBLK_FAN_WAL_OP_RESULT_BATCH
        | ZCNBLK_FAN_WAL_OP_REQUEST_BATCH
        | ZCNBLK_FAN_WAL_OP_WRITE_EXTENT_BATCH => frame.payload_len as usize,
        _ => 0,
    };
    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload)?;
    Ok(Some((frame, payload)))
}

fn send_hello(stream: &mut TcpStream, hello: ZcnblkFanWalFrame) -> io::Result<ZcnblkFanWalFrame> {
    write_frame(stream, hello, &[])?;
    let (ack, payload) = read_frame(stream)?;
    if ack.op != ZCNBLK_FAN_WAL_OP_HELLO_ACK || !payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WAL migration leaf omitted HELLO_ACK",
        ));
    }
    Ok(ack)
}

fn migration_descriptor(
    hello: ZcnblkFanWalFrame,
    wal_op: u16,
    block_op: u16,
    offset: u64,
    len: usize,
    sequence: u64,
) -> io::Result<ZcnblkFanWalFrame> {
    let len = u32::try_from(len).map_err(|_| invalid("migration range exceeds u32"))?;
    Ok(ZcnblkFanWalFrame {
        op: wal_op,
        lane_id: hello.lane_id,
        lane_count: hello.lane_count,
        branch_id: hello.branch_id,
        branch_count: hello.branch_count,
        segment_index: 0,
        segment_count: 1,
        payload_len: len,
        sequence,
        request_id: hello.request_id,
        placement_epoch: hello.placement_epoch,
        logical_offset: offset,
        leaf_offset: offset,
        logical_len: len,
        zcnblk_op: block_op,
        zcnblk_shard: hello.zcnblk_shard,
        topology_preferred_worker: hello.topology_preferred_worker,
        topology_queue_id: hello.topology_queue_id,
        topology_tier_id: hello.topology_tier_id,
        topology_flags: hello.topology_flags,
        topology_preferred_cpu: hello.topology_preferred_cpu,
        topology_numa_node: hello.topology_numa_node,
        ..ZcnblkFanWalFrame::default()
    })
}

fn validate_copy_result(
    result: ZcnblkFanWalFrame,
    request: ZcnblkFanWalFrame,
    expected_payload: usize,
) -> io::Result<()> {
    if result.op != ZCNBLK_FAN_WAL_OP_RESULT
        || result.status != ZCNBLK_FAN_WAL_STATUS_OK
        || result.sequence != request.sequence
        || result.leaf_offset != request.leaf_offset
        || result.payload_len as usize != expected_payload
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "invalid WAL migration result op={} status={} sequence={} expected_sequence={} offset={} expected_offset={} payload={} expected_payload={expected_payload}",
                result.op,
                result.status,
                result.sequence,
                request.sequence,
                result.leaf_offset,
                request.leaf_offset,
                result.payload_len,
            ),
        ));
    }
    Ok(())
}

fn paced_duration(bytes: u128, bytes_per_second: u64) -> Duration {
    if bytes_per_second == 0 {
        return Duration::ZERO;
    }
    let nanos = bytes
        .saturating_mul(1_000_000_000)
        .div_ceil(u128::from(bytes_per_second))
        .min(u128::from(u64::MAX));
    Duration::from_nanos(nanos as u64)
}

fn scale_duration(duration: Duration, numerator: u32, denominator: u32) -> Duration {
    debug_assert!(denominator != 0);
    Duration::from_nanos(
        duration
            .as_nanos()
            .saturating_mul(u128::from(numerator))
            .checked_div(u128::from(denominator))
            .unwrap_or_default()
            .min(u128::from(u64::MAX)) as u64,
    )
}

fn initial_lane_pacing_delay(
    effective_ns: u64,
    now_ns: u64,
    period: Duration,
    lane_id: u32,
    lane_count: u32,
) -> Duration {
    let period_ns = period.as_nanos().min(u128::from(u64::MAX)) as u64;
    if period_ns == 0 {
        return Duration::ZERO;
    }
    let phase_ns = scale_duration(period, lane_id, lane_count)
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64;
    let first_slot = effective_ns.saturating_add(phase_ns);
    let due_ns = if now_ns <= first_slot {
        first_slot
    } else {
        let elapsed = now_ns - first_slot;
        let periods = elapsed.div_ceil(period_ns);
        first_slot.saturating_add(periods.saturating_mul(period_ns))
    };
    Duration::from_nanos(due_ns.saturating_sub(now_ns))
}

struct Pipe {
    read: OwnedFd,
    write: OwnedFd,
}

impl Pipe {
    fn new() -> io::Result<Self> {
        let mut fds = [-1; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            read: unsafe { OwnedFd::from_raw_fd(fds[0]) },
            write: unsafe { OwnedFd::from_raw_fd(fds[1]) },
        })
    }

    fn splice_exact(
        &self,
        source: &TcpStream,
        destination: &TcpStream,
        len: usize,
    ) -> io::Result<u64> {
        let mut remaining = len;
        let mut syscalls = 0u64;
        while remaining != 0 {
            let moved_in = splice_retry(
                source.as_raw_fd(),
                self.write.as_raw_fd(),
                remaining,
                libc::SPLICE_F_MOVE | libc::SPLICE_F_MORE,
            )?;
            syscalls = syscalls.saturating_add(1);
            if moved_in == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "WAL migration source closed in a spliced payload",
                ));
            }
            let mut pending = moved_in;
            while pending != 0 {
                /*
                 * Do not advertise MORE for the last pipe chunk.  Leaving
                 * SPLICE_F_MORE set on the final socket write lets TCP
                 * autocork the tail until its delayed-flush timer, which is
                 * disastrous when this is the final dirty range under the
                 * cutover fence.
                 */
                let output_flags = libc::SPLICE_F_MOVE
                    | if remaining > moved_in {
                        libc::SPLICE_F_MORE
                    } else {
                        0
                    };
                let moved_out = splice_retry(
                    self.read.as_raw_fd(),
                    destination.as_raw_fd(),
                    pending,
                    output_flags,
                )?;
                syscalls = syscalls.saturating_add(1);
                if moved_out == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "WAL migration destination accepted no spliced payload bytes",
                    ));
                }
                pending -= moved_out;
            }
            remaining -= moved_in;
        }
        Ok(syscalls)
    }
}

fn splice_retry(input: i32, output: i32, len: usize, flags: u32) -> io::Result<usize> {
    loop {
        let result = unsafe {
            libc::splice(
                input,
                std::ptr::null_mut(),
                output,
                std::ptr::null_mut(),
                len,
                flags,
            )
        };
        if result >= 0 {
            return Ok(result as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// An SPSC retention queue. The producer is the lane's foreground proxy and
/// the consumer is its migration replay worker. Slots are allocated once;
/// enqueue transfers ownership of the existing payload allocation and takes
/// no mutex. Full retention applies explicit lane backpressure.
pub struct RetainedWalQueue<T> {
    inner: Arc<RetainedWalQueueInner<T>>,
}

struct RetainedWalQueueInner<T> {
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    head: AtomicUsize,
    tail: AtomicUsize,
}

// Only the split producer writes a slot and only the split consumer reads it;
// Release/Acquire publication prevents concurrent access to initialized data.
unsafe impl<T: Send> Send for RetainedWalQueueInner<T> {}
unsafe impl<T: Send> Sync for RetainedWalQueueInner<T> {}

pub struct RetainedWalProducer<T> {
    inner: Arc<RetainedWalQueueInner<T>>,
    tail: usize,
}

pub struct RetainedWalConsumer<T> {
    inner: Arc<RetainedWalQueueInner<T>>,
    head: usize,
}

impl<T> RetainedWalQueue<T> {
    pub fn new(capacity: usize) -> io::Result<Self> {
        if capacity == 0 || !capacity.is_power_of_two() || capacity > isize::MAX as usize {
            return Err(invalid(
                "retained WAL queue capacity must be a non-zero power of two",
            ));
        }
        let slots = (0..capacity)
            .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Self {
            inner: Arc::new(RetainedWalQueueInner {
                slots,
                head: AtomicUsize::new(0),
                tail: AtomicUsize::new(0),
            }),
        })
    }

    pub fn split(self) -> (RetainedWalProducer<T>, RetainedWalConsumer<T>) {
        (
            RetainedWalProducer {
                tail: 0,
                inner: Arc::clone(&self.inner),
            },
            RetainedWalConsumer {
                head: 0,
                inner: self.inner,
            },
        )
    }
}

impl<T> RetainedWalProducer<T> {
    pub fn try_push(&mut self, value: T) -> Result<(), T> {
        let head = self.inner.head.load(Ordering::Acquire);
        if self.tail.wrapping_sub(head) == self.inner.slots.len() {
            return Err(value);
        }
        let slot = self.tail & (self.inner.slots.len() - 1);
        unsafe { (*self.inner.slots[slot].get()).write(value) };
        self.tail = self.tail.wrapping_add(1);
        self.inner.tail.store(self.tail, Ordering::Release);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.tail
            .wrapping_sub(self.inner.head.load(Ordering::Acquire))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> RetainedWalConsumer<T> {
    pub fn try_pop(&mut self) -> Option<T> {
        let tail = self.inner.tail.load(Ordering::Acquire);
        if self.head == tail {
            return None;
        }
        let slot = self.head & (self.inner.slots.len() - 1);
        let value = unsafe { (*self.inner.slots[slot].get()).assume_init_read() };
        self.head = self.head.wrapping_add(1);
        self.inner.head.store(self.head, Ordering::Release);
        Some(value)
    }

    pub fn len(&self) -> usize {
        self.inner
            .tail
            .load(Ordering::Acquire)
            .wrapping_sub(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Drop for RetainedWalQueueInner<T> {
    fn drop(&mut self) {
        let head = *self.head.get_mut();
        let tail = *self.tail.get_mut();
        for index in head..tail {
            let slot = index & (self.slots.len() - 1);
            unsafe { self.slots[slot].get_mut().assume_init_drop() };
        }
    }
}

const PHASE_IDLE: u8 = 0;
const PHASE_STARTING: u8 = 1;
const PHASE_COPYING: u8 = 2;
const PHASE_CUTOVER_REQUESTED: u8 = 3;
const PHASE_ACTIVE_SECONDARY: u8 = 4;
const PHASE_FAILED: u8 = 5;

fn phase_label(phase: u8) -> &'static str {
    match phase {
        PHASE_IDLE => "idle_primary",
        PHASE_STARTING => "starting_retention",
        PHASE_COPYING => "base_copy_and_replay",
        PHASE_CUTOVER_REQUESTED => "cutover_requested",
        PHASE_ACTIVE_SECONDARY => "active_secondary",
        PHASE_FAILED => "failed",
        _ => "invalid",
    }
}

struct MigrationLaneState {
    registered: AtomicBool,
    foreground_thread: AtomicUsize,
    retention_active: AtomicBool,
    base_ready: AtomicBool,
    producer_quiesced: AtomicBool,
    destination_ready: AtomicBool,
    source_hwm: AtomicU64,
    source_sync_epoch: AtomicU64,
    destination_hwm: AtomicU64,
    copy_bytes: AtomicU64,
    replay_records: AtomicU64,
    system_grant: Arc<SystemTaskGrantMailbox>,
    route_hwm: Arc<RouteHwmMailbox>,
    cancelled: AtomicBool,
}

struct MigrationCoordinator {
    phase: AtomicU8,
    generation: AtomicU64,
    placement_epoch: AtomicU64,
    rate_generation: AtomicU64,
    lanes: Box<[MigrationLaneState]>,
    commit: Mutex<()>,
    changed: (Mutex<()>, Condvar),
    failure: Mutex<Option<String>>,
}

impl MigrationCoordinator {
    fn new(lane_count: u32, bytes_per_second: u64) -> io::Result<Self> {
        install_cutover_wake_handler()?;
        if lane_count == 0 || bytes_per_second == 0 {
            return Err(invalid(
                "live migration requires non-zero lanes and system-task bytes/second",
            ));
        }
        let lane_rate = bytes_per_second.div_ceil(u64::from(lane_count));
        let lanes = (0..lane_count)
            .map(|_| {
                Ok(MigrationLaneState {
                    registered: AtomicBool::new(false),
                    foreground_thread: AtomicUsize::new(0),
                    retention_active: AtomicBool::new(false),
                    base_ready: AtomicBool::new(false),
                    producer_quiesced: AtomicBool::new(false),
                    destination_ready: AtomicBool::new(false),
                    source_hwm: AtomicU64::new(0),
                    source_sync_epoch: AtomicU64::new(0),
                    destination_hwm: AtomicU64::new(0),
                    copy_bytes: AtomicU64::new(0),
                    replay_records: AtomicU64::new(0),
                    system_grant: Arc::new(SystemTaskGrantMailbox::new(SystemTaskGrantSnapshot {
                        generation: 1,
                        target_iops: 1,
                        target_bytes_per_second: lane_rate,
                        effective_ns: 0,
                        valid_until_ns: 0,
                        fallback_iops: 0,
                        fallback_bytes_per_second: 0,
                    })),
                    route_hwm: Arc::new(RouteHwmMailbox::new(RouteHwmSnapshot {
                        generation: 1,
                        placement_epoch: 1,
                        applied_hwm: 0,
                        effective_ns: 0,
                    })?),
                    cancelled: AtomicBool::new(false),
                })
            })
            .collect::<io::Result<Vec<_>>>()?
            .into_boxed_slice();
        Ok(Self {
            phase: AtomicU8::new(PHASE_IDLE),
            generation: AtomicU64::new(1),
            placement_epoch: AtomicU64::new(1),
            rate_generation: AtomicU64::new(1),
            lanes,
            commit: Mutex::new(()),
            changed: (Mutex::new(()), Condvar::new()),
            failure: Mutex::new(None),
        })
    }

    fn lane(&self, lane: u32) -> io::Result<&MigrationLaneState> {
        self.lanes
            .get(lane as usize)
            .ok_or_else(|| invalid(format!("unknown migration lane {lane}")))
    }

    fn register_lane(&self, lane: u32) -> io::Result<LaneRegistration<'_>> {
        let state = self.lane(lane)?;
        if state.registered.swap(true, Ordering::AcqRel) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("migration lane {lane} already has a foreground session"),
            ));
        }
        state
            .foreground_thread
            .store(unsafe { libc::pthread_self() } as usize, Ordering::Release);
        self.changed.1.notify_all();
        Ok(LaneRegistration { lane: state })
    }

    fn start(&self) -> io::Result<String> {
        let _control = self
            .commit
            .lock()
            .map_err(|_| io::Error::other("migration control lock poisoned"))?;
        if self
            .lanes
            .iter()
            .any(|lane| !lane.registered.load(Ordering::Acquire))
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "all declared foreground lanes must be connected before migration starts",
            ));
        }
        let phase = self.phase.load(Ordering::Acquire);
        if phase != PHASE_IDLE {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("cannot start migration in phase {}", phase_label(phase)),
            ));
        }
        self.rephase_grants()?;
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.phase.store(PHASE_STARTING, Ordering::Release);
        self.changed.1.notify_all();
        Ok(format!("OK generation={generation} {}", self.status()))
    }

    fn request_cutover(self: &Arc<Self>) -> io::Result<String> {
        loop {
            let phase = self.phase.load(Ordering::Acquire);
            if phase == PHASE_CUTOVER_REQUESTED {
                return Ok(format!("OK already_requested=true {}", self.status()));
            }
            if !matches!(phase, PHASE_STARTING | PHASE_COPYING) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("cannot cut over in phase {}", phase_label(phase)),
                ));
            }
            if self
                .phase
                .compare_exchange(
                    phase,
                    PHASE_CUTOVER_REQUESTED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.changed.1.notify_all();
                let coordinator = Arc::clone(self);
                thread::Builder::new()
                    .name("zcwal-cutover-waker".into())
                    .spawn(move || coordinator.wake_idle_foreground_for_cutover())?;
                return Ok(format!("OK already_requested=false {}", self.status()));
            }
        }
    }

    fn wake_idle_foreground_for_cutover(&self) {
        while self.phase.load(Ordering::Acquire) == PHASE_CUTOVER_REQUESTED
            && !self.all_base_ready()
        {
            let Ok(guard) = self.changed.0.lock() else {
                return;
            };
            let _ = self
                .changed
                .1
                .wait_timeout(guard, Duration::from_millis(10));
        }
        while self.phase.load(Ordering::Acquire) == PHASE_CUTOVER_REQUESTED
            && self
                .lanes
                .iter()
                .any(|lane| !lane.producer_quiesced.load(Ordering::Acquire))
        {
            for lane in &self.lanes {
                if lane.producer_quiesced.load(Ordering::Acquire) {
                    continue;
                }
                let thread = lane.foreground_thread.load(Ordering::Acquire);
                if thread != 0 {
                    unsafe {
                        libc::pthread_kill(thread as libc::pthread_t, CUTOVER_WAKE_SIGNAL);
                    }
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn set_rate(&self, bytes_per_second: u64) -> io::Result<String> {
        if bytes_per_second == 0 {
            return Err(invalid(
                "use pause to publish a zero grant; rate must be non-zero",
            ));
        }
        self.publish_grant(bytes_per_second, false)?;
        Ok(format!(
            "OK system_bytes_per_second={bytes_per_second} {}",
            self.status()
        ))
    }

    fn publish_grant(&self, bytes_per_second: u64, paused: bool) -> io::Result<()> {
        let generation = self.rate_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let effective_ns = monotonic_time_ns()?;
        let lane_rate = if paused {
            0
        } else {
            bytes_per_second.div_ceil(self.lanes.len() as u64)
        };
        for lane in &self.lanes {
            lane.system_grant.publish(SystemTaskGrantSnapshot {
                generation,
                target_iops: u64::from(!paused),
                target_bytes_per_second: lane_rate,
                effective_ns,
                valid_until_ns: 0,
                fallback_iops: 0,
                fallback_bytes_per_second: 0,
            });
        }
        Ok(())
    }

    fn rephase_grants(&self) -> io::Result<()> {
        let now_ns = monotonic_time_ns()?;
        let generation = self.rate_generation.fetch_add(1, Ordering::AcqRel) + 1;
        for lane in &self.lanes {
            let mut grant = lane.system_grant.load(now_ns);
            grant.generation = generation;
            grant.effective_ns = now_ns;
            lane.system_grant.publish(grant);
        }
        Ok(())
    }

    fn fail(&self, message: impl Into<String>) {
        let message = message.into();
        if let Ok(mut failure) = self.failure.lock() {
            *failure = Some(message);
        }
        for lane in &self.lanes {
            lane.cancelled.store(true, Ordering::Release);
        }
        self.phase.store(PHASE_FAILED, Ordering::Release);
        self.changed.1.notify_all();
    }

    fn maybe_commit_cutover(&self) -> io::Result<bool> {
        let _commit = self
            .commit
            .lock()
            .map_err(|_| io::Error::other("migration commit lock poisoned"))?;
        if self.phase.load(Ordering::Acquire) != PHASE_CUTOVER_REQUESTED
            || self
                .lanes
                .iter()
                .any(|lane| !lane.destination_ready.load(Ordering::Acquire))
        {
            return Ok(false);
        }
        let placement_epoch = self.placement_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let effective_ns = monotonic_time_ns()?;
        for lane in &self.lanes {
            lane.route_hwm.publish_clean_cutover(RouteHwmSnapshot {
                generation,
                placement_epoch,
                applied_hwm: lane.destination_hwm.load(Ordering::Acquire),
                effective_ns,
            })?;
        }
        self.phase.store(PHASE_ACTIVE_SECONDARY, Ordering::Release);
        self.changed.1.notify_all();
        Ok(true)
    }

    fn wait_for_active_secondary(&self) -> io::Result<()> {
        let mut guard = self
            .changed
            .0
            .lock()
            .map_err(|_| io::Error::other("migration condition lock poisoned"))?;
        loop {
            match self.phase.load(Ordering::Acquire) {
                PHASE_ACTIVE_SECONDARY => return Ok(()),
                PHASE_FAILED => {
                    let failure = self
                        .failure
                        .lock()
                        .ok()
                        .and_then(|value| value.clone())
                        .unwrap_or_else(|| "migration failed".to_string());
                    return Err(io::Error::other(failure));
                }
                _ => {
                    let (next, _) = self
                        .changed
                        .1
                        .wait_timeout(guard, Duration::from_millis(100))
                        .map_err(|_| io::Error::other("migration condition lock poisoned"))?;
                    guard = next;
                }
            }
        }
    }

    fn all_base_ready(&self) -> bool {
        self.lanes
            .iter()
            .all(|lane| lane.base_ready.load(Ordering::Acquire))
    }

    fn status(&self) -> String {
        let lanes = self
            .lanes
            .iter()
            .enumerate()
            .map(|(index, lane)| {
                format!(
                    "{index}:connected={},retain={},base={},quiesced={},ready={},source_hwm={},destination_hwm={},copy_bytes={},replay_records={}",
                    lane.registered.load(Ordering::Acquire),
                    lane.retention_active.load(Ordering::Acquire),
                    lane.base_ready.load(Ordering::Acquire),
                    lane.producer_quiesced.load(Ordering::Acquire),
                    lane.destination_ready.load(Ordering::Acquire),
                    lane.source_hwm.load(Ordering::Acquire),
                    lane.destination_hwm.load(Ordering::Acquire),
                    lane.copy_bytes.load(Ordering::Acquire),
                    lane.replay_records.load(Ordering::Acquire),
                )
            })
            .collect::<Vec<_>>()
            .join(";");
        let failure = self
            .failure
            .lock()
            .ok()
            .and_then(|value| value.clone())
            .unwrap_or_else(|| "none".to_string());
        format!(
            "phase={} generation={} placement_epoch={} lanes=[{}] failure={failure:?}",
            phase_label(self.phase.load(Ordering::Acquire)),
            self.generation.load(Ordering::Acquire),
            self.placement_epoch.load(Ordering::Acquire),
            lanes,
        )
    }
}

struct LaneRegistration<'a> {
    lane: &'a MigrationLaneState,
}

impl Drop for LaneRegistration<'_> {
    fn drop(&mut self) {
        self.lane.foreground_thread.store(0, Ordering::Release);
        self.lane.registered.store(false, Ordering::Release);
    }
}

struct RetainedRequest {
    frame: ZcnblkFanWalFrame,
    payload: Vec<u8>,
    applied_hwm: u64,
}

fn request_applied_hwm(frame: ZcnblkFanWalFrame, payload: &[u8]) -> io::Result<u64> {
    if matches!(
        frame.op,
        ZCNBLK_FAN_WAL_OP_REQUEST_BATCH | ZCNBLK_FAN_WAL_OP_WRITE_BATCH
    ) {
        let descriptor_bytes = (frame.segment_count as usize)
            .checked_mul(ZCNBLK_FAN_WAL_HEADER_LEN)
            .ok_or_else(|| invalid("retained WAL descriptor size overflow"))?;
        if payload.len() < descriptor_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "retained WAL batch is shorter than its descriptor table",
            ));
        }
        let mut hwm = 0u64;
        for descriptor in payload[..descriptor_bytes].chunks_exact(ZCNBLK_FAN_WAL_HEADER_LEN) {
            let header: &[u8; ZCNBLK_FAN_WAL_HEADER_LEN] =
                descriptor.try_into().expect("complete descriptor");
            let member = ZcnblkFanWalFrame::decode(header)?;
            if member.op == ZCNBLK_FAN_WAL_OP_WRITE_DESC {
                hwm = hwm.max(member.sequence.saturating_add(1));
            }
        }
        return Ok(hwm);
    }
    let width = if frame.op == ZCNBLK_FAN_WAL_OP_WRITE_EXTENT_BATCH {
        u64::from(frame.segment_count.max(1))
    } else {
        1
    };
    Ok(frame.sequence.saturating_add(width))
}

struct LaneWorkerConfig {
    transport: LiveMigrationTransport,
    prepared_system: Option<PreparedSystemSessions>,
    source: Endpoint,
    destination: Endpoint,
    hello: ZcnblkFanWalFrame,
    copy: WalBaseCopySpec,
    replay_poll: Duration,
    replay_window: usize,
}

fn migration_lane_worker(
    coordinator: Arc<MigrationCoordinator>,
    lane_id: u32,
    mut retained: RetainedWalConsumer<RetainedRequest>,
    mut config: LaneWorkerConfig,
) -> io::Result<()> {
    let _copy_cpu = pin_migration_role("copy", lane_id)?;
    let lane = coordinator.lane(lane_id)?;
    while coordinator
        .lanes
        .iter()
        .any(|candidate| !candidate.retention_active.load(Ordering::Acquire))
    {
        if coordinator.phase.load(Ordering::Acquire) == PHASE_FAILED {
            return Err(io::Error::other("migration failed before base copy"));
        }
        thread::sleep(config.replay_poll);
    }
    let _ = coordinator.phase.compare_exchange(
        PHASE_STARTING,
        PHASE_COPYING,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    let managed = ManagedWalCopy {
        grants: &lane.system_grant,
        cancelled: &lane.cancelled,
        idle_wait: config.replay_poll,
    };
    let copy_bytes = config.transport.base_copy(
        &config.source,
        &config.destination,
        config.hello,
        config.copy,
        &managed,
        config.prepared_system.as_mut(),
    )?;
    lane.copy_bytes.store(copy_bytes, Ordering::Release);
    lane.base_ready.store(true, Ordering::Release);
    coordinator.changed.1.notify_all();

    let mut destination = if let Some(prepared) = config.prepared_system.take() {
        prepared.replay_destination
    } else {
        config.transport.connect_leaf(
            &config.destination,
            lane_id,
            config.hello.lane_count,
            2,
            LiveEndpointRole::Destination,
        )?
    };
    send_live_hello(&mut destination, config.hello)?;
    let mut replay_in_flight = VecDeque::<u64>::with_capacity(config.replay_window);
    loop {
        let mut progressed = false;
        while replay_in_flight.len() < config.replay_window
            && let Some(request) = retained.try_pop()
        {
            write_stream_frame(&mut destination, request.frame, &request.payload)?;
            replay_in_flight.push_back(request.applied_hwm);
            progressed = true;
        }
        if let Some(applied_hwm) = replay_in_flight.pop_front() {
            let (result, _payload) = read_stream_frame(&mut destination)?;
            if result.status != ZCNBLK_FAN_WAL_STATUS_OK {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("destination replay failed with status {}", result.status),
                ));
            }
            lane.destination_hwm
                .fetch_max(applied_hwm, Ordering::AcqRel);
            lane.replay_records.fetch_add(1, Ordering::Relaxed);
            progressed = true;
        }
        if coordinator.phase.load(Ordering::Acquire) == PHASE_CUTOVER_REQUESTED
            && lane.producer_quiesced.load(Ordering::Acquire)
            && retained.is_empty()
            && replay_in_flight.is_empty()
        {
            let source_hwm = lane.source_hwm.load(Ordering::Acquire);
            let source_sync_epoch = lane.source_sync_epoch.load(Ordering::Acquire);
            let sync = ZcnblkFanWalFrame {
                op: ZCNBLK_FAN_WAL_OP_SYNC,
                lane_id: config.hello.lane_id,
                lane_count: config.hello.lane_count,
                branch_id: config.hello.branch_id,
                branch_count: config.hello.branch_count,
                sequence: source_hwm,
                request_id: config.hello.request_id,
                sync_epoch: source_sync_epoch.max(coordinator.generation.load(Ordering::Acquire)),
                placement_epoch: config.hello.placement_epoch,
                zcnblk_op: ZCNBLK_OP_SYNC,
                ..ZcnblkFanWalFrame::default()
            };
            write_stream_frame(&mut destination, sync, &[])?;
            let (result, payload) = read_stream_frame(&mut destination)?;
            if result.op != ZCNBLK_FAN_WAL_OP_RESULT
                || result.status != ZCNBLK_FAN_WAL_STATUS_OK
                || !payload.is_empty()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "destination omitted the final migration sync acknowledgement",
                ));
            }
            lane.destination_hwm.store(source_hwm, Ordering::Release);
            lane.destination_ready.store(true, Ordering::Release);
            coordinator.maybe_commit_cutover()?;
            coordinator.wait_for_active_secondary()?;
            if matches!(config.transport, LiveMigrationTransport::Ofi(_)) {
                send_stream_eof(
                    match &mut destination {
                        LiveWalStream::Ofi(stream) => stream,
                        LiveWalStream::Tcp(_) => unreachable!("transport and stream disagree"),
                    },
                    config.hello,
                )?;
            }
            return Ok(());
        }
        if coordinator.phase.load(Ordering::Acquire) == PHASE_FAILED {
            return Err(io::Error::other("migration coordinator failed"));
        }
        if !progressed {
            thread::sleep(config.replay_poll);
        }
    }
}

fn retain_with_backpressure(
    producer: &mut RetainedWalProducer<RetainedRequest>,
    mut request: RetainedRequest,
    coordinator: &MigrationCoordinator,
) -> io::Result<()> {
    loop {
        match producer.try_push(request) {
            Ok(()) => return Ok(()),
            Err(returned) => request = returned,
        }
        if coordinator.phase.load(Ordering::Acquire) == PHASE_FAILED {
            return Err(io::Error::other("migration retention failed"));
        }
        thread::yield_now();
    }
}

#[allow(clippy::too_many_arguments)]
fn proxy_foreground_session(
    mut upstream: LiveWalStream,
    transport: LiveMigrationTransport,
    source_endpoint: Endpoint,
    destination_endpoint: Endpoint,
    coordinator: Arc<MigrationCoordinator>,
    listener_lane: u32,
    volume_bytes: u64,
    chunk_bytes: usize,
    method: TcpBulkCopyMethod,
    retention_records: usize,
    replay_poll: Duration,
    replay_window: usize,
) -> io::Result<()> {
    let _proxy_cpu = pin_migration_role("proxy", listener_lane)?;
    upstream.set_low_latency()?;
    let mut source = transport.connect_leaf(
        &source_endpoint,
        listener_lane,
        coordinator.lanes.len() as u32,
        0,
        LiveEndpointRole::Source,
    )?;
    let mut destination = transport.connect_leaf(
        &destination_endpoint,
        listener_lane,
        coordinator.lanes.len() as u32,
        0,
        LiveEndpointRole::Destination,
    )?;
    let mut prepared_system_sessions = transport.prepare_system_sessions(
        &source_endpoint,
        &destination_endpoint,
        listener_lane,
        coordinator.lanes.len() as u32,
    )?;
    let (upstream_hello, hello_payload) = read_stream_frame(&mut upstream)?;
    let hello = framed_forwarding_hello(upstream_hello, &transport);
    if hello.op != ZCNBLK_FAN_WAL_OP_HELLO
        || !hello_payload.is_empty()
        || hello.lane_id != listener_lane
        || hello.lane_count != coordinator.lanes.len() as u32
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "live-migration ingress received an invalid lane HELLO",
        ));
    }
    let source_ack = send_live_hello(&mut source, hello)?;
    let destination_ack = send_live_hello(&mut destination, hello)?;
    if destination_ack.request_id != source_ack.request_id
        || destination_ack.placement_epoch != source_ack.placement_epoch
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source and destination admitted different route plans",
        ));
    }
    let _registration = coordinator.register_lane(listener_lane)?;
    let mut upstream_ack = source_ack;
    upstream_ack.placement_epoch = coordinator.placement_epoch.load(Ordering::Acquire);
    write_stream_frame(&mut upstream, upstream_ack, &[])?;

    let mut local_generation = 0u64;
    let mut producer = None::<RetainedWalProducer<RetainedRequest>>;
    let mut active_secondary = false;
    let mut worker = None::<thread::JoinHandle<io::Result<()>>>;
    loop {
        let phase = coordinator.phase.load(Ordering::Acquire);
        let generation = coordinator.generation.load(Ordering::Acquire);
        if local_generation == 0
            && matches!(
                phase,
                PHASE_STARTING | PHASE_COPYING | PHASE_CUTOVER_REQUESTED
            )
        {
            let (next_producer, consumer) = RetainedWalQueue::new(retention_records)?.split();
            producer = Some(next_producer);
            local_generation = generation;
            let lane = coordinator.lane(listener_lane)?;
            lane.retention_active.store(true, Ordering::Release);
            coordinator.changed.1.notify_all();
            let worker_coordinator = Arc::clone(&coordinator);
            let config = LaneWorkerConfig {
                transport: transport.clone(),
                prepared_system: prepared_system_sessions.take(),
                source: source_endpoint.clone(),
                destination: destination_endpoint.clone(),
                hello,
                copy: WalBaseCopySpec {
                    volume_bytes,
                    chunk_bytes,
                    lane_id: listener_lane,
                    lane_count: hello.lane_count,
                    method,
                },
                replay_poll,
                replay_window,
            };
            worker = Some(
                thread::Builder::new()
                    .name(format!("zcwal-migration-copy-{listener_lane}"))
                    .spawn(move || {
                        let result = migration_lane_worker(
                            Arc::clone(&worker_coordinator),
                            listener_lane,
                            consumer,
                            config,
                        );
                        if let Err(error) = &result {
                            worker_coordinator.fail(format!(
                                "lane {listener_lane} migration worker failed: {error}"
                            ));
                        }
                        result
                    })?,
            );
        }
        if !active_secondary
            && producer.is_some()
            && coordinator.phase.load(Ordering::Acquire) == PHASE_CUTOVER_REQUESTED
            && coordinator.all_base_ready()
        {
            coordinator
                .lane(listener_lane)?
                .producer_quiesced
                .store(true, Ordering::Release);
            coordinator.changed.1.notify_all();
            coordinator.wait_for_active_secondary()?;
            active_secondary = true;
            producer.take();
            if let Some(worker) = worker.take() {
                worker
                    .join()
                    .map_err(|_| io::Error::other("migration lane worker panicked"))??;
            }
            eprintln!(
                "zcnblk-wal-live-migration-cutover: lane={listener_lane} generation={} placement_epoch={} destination_hwm={} client_reconnect=false cache_fence=epoch+hwm",
                coordinator.generation.load(Ordering::Acquire),
                coordinator.placement_epoch.load(Ordering::Acquire),
                coordinator
                    .lane(listener_lane)?
                    .destination_hwm
                    .load(Ordering::Acquire),
            );
        }

        let (frame, payload) = match read_upstream_frame_interruptible(&mut upstream, &coordinator)
        {
            Ok(Some(request)) => request,
            Ok(None) => continue,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        if frame.op == ZCNBLK_FAN_WAL_OP_EOF {
            write_stream_frame(&mut source, frame, &payload)?;
            write_stream_frame(&mut destination, frame, &payload)?;
            return Ok(());
        }

        let (mut result, result_payload) = if active_secondary {
            write_stream_frame(&mut destination, frame, &payload)?;
            read_stream_frame(&mut destination)?
        } else {
            write_stream_frame(&mut source, frame, &payload)?;
            read_stream_frame(&mut source)?
        };
        // The migration stage owns placement, so the result returned to the
        // edge carries its current route epoch rather than the terminal
        // leaf's private connection epoch.  The edge observes this at its
        // existing result/batch boundary and fences sequence-keyed look-aside
        // state without a new per-I/O synchronization primitive.
        result.placement_epoch = coordinator.placement_epoch.load(Ordering::Acquire);

        // The control request may have arrived while this lane was blocked in
        // its foreground read. Start retention before acknowledging the frame
        // so this exact write cannot fall between the retention and base-copy
        // watermarks. The next loop iteration starts the worker.
        let phase_after = coordinator.phase.load(Ordering::Acquire);
        if local_generation == 0
            && matches!(
                phase_after,
                PHASE_STARTING | PHASE_COPYING | PHASE_CUTOVER_REQUESTED
            )
        {
            let (next_producer, consumer) = RetainedWalQueue::new(retention_records)?.split();
            producer = Some(next_producer);
            local_generation = coordinator.generation.load(Ordering::Acquire);
            coordinator
                .lane(listener_lane)?
                .retention_active
                .store(true, Ordering::Release);
            let worker_coordinator = Arc::clone(&coordinator);
            let config = LaneWorkerConfig {
                transport: transport.clone(),
                prepared_system: prepared_system_sessions.take(),
                source: source_endpoint.clone(),
                destination: destination_endpoint.clone(),
                hello,
                copy: WalBaseCopySpec {
                    volume_bytes,
                    chunk_bytes,
                    lane_id: listener_lane,
                    lane_count: hello.lane_count,
                    method,
                },
                replay_poll,
                replay_window,
            };
            worker = Some(
                thread::Builder::new()
                    .name(format!("zcwal-migration-copy-{listener_lane}"))
                    .spawn(move || {
                        let result = migration_lane_worker(
                            Arc::clone(&worker_coordinator),
                            listener_lane,
                            consumer,
                            config,
                        );
                        if let Err(error) = &result {
                            worker_coordinator.fail(format!(
                                "lane {listener_lane} migration worker failed: {error}"
                            ));
                        }
                        result
                    })?,
            );
            coordinator.changed.1.notify_all();
        }

        if !active_secondary
            && producer.is_some()
            && (is_write(frame, &payload)? || frame.op == ZCNBLK_FAN_WAL_OP_SYNC)
        {
            let applied_hwm = if frame.op == ZCNBLK_FAN_WAL_OP_SYNC {
                coordinator
                    .lane(listener_lane)?
                    .source_sync_epoch
                    .store(frame.sync_epoch, Ordering::Release);
                coordinator
                    .lane(listener_lane)?
                    .source_hwm
                    .load(Ordering::Acquire)
            } else {
                request_applied_hwm(frame, &payload)?
            };
            coordinator
                .lane(listener_lane)?
                .source_hwm
                .fetch_max(applied_hwm, Ordering::AcqRel);
            retain_with_backpressure(
                producer.as_mut().expect("checked retained producer"),
                RetainedRequest {
                    frame,
                    payload,
                    applied_hwm,
                },
                &coordinator,
            )?;
        }
        write_stream_frame(&mut upstream, result, &result_payload)?;
    }
}

fn control_session(
    mut stream: TcpStream,
    coordinator: &Arc<MigrationCoordinator>,
) -> io::Result<()> {
    let mut command = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut command)?;
    let command = command.trim();
    let response = match command {
        "status" => format!("OK {}", coordinator.status()),
        "start" => coordinator
            .start()
            .unwrap_or_else(|error| format!("ERR kind={:?} message={error}", error.kind())),
        "cutover" => coordinator
            .request_cutover()
            .unwrap_or_else(|error| format!("ERR kind={:?} message={error}", error.kind())),
        "pause" => coordinator
            .publish_grant(0, true)
            .map(|()| format!("OK system_task_paused=true {}", coordinator.status()))
            .unwrap_or_else(|error| format!("ERR kind={:?} message={error}", error.kind())),
        value if value.starts_with("rate ") => match value[5..].parse::<u64>() {
            Ok(rate) => coordinator
                .set_rate(rate)
                .unwrap_or_else(|error| format!("ERR kind={:?} message={error}", error.kind())),
            Err(error) => format!("ERR kind=InvalidInput message={error}"),
        },
        other => format!(
            "ERR kind=InvalidInput message=unknown command {other:?}; use status, start, cutover, pause, or rate BYTES_PER_SECOND"
        ),
    };
    writeln!(stream, "{response}")
}

fn run_control(listener: TcpListener, coordinator: Arc<MigrationCoordinator>) -> io::Result<()> {
    for accepted in listener.incoming() {
        if let Err(error) = control_session(accepted?, &coordinator) {
            eprintln!("zcnblk-wal-live-migration-control-error: {error}");
        }
    }
    Ok(())
}

fn resolve_one(value: &str) -> io::Result<SocketAddr> {
    value
        .to_socket_addrs()
        .map_err(|error| invalid(format!("invalid socket address {value:?}: {error}")))?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, value.to_string()))
}

fn usage() -> io::Error {
    invalid(
        "usage: zcnblk-wal-live-migrate LISTEN_BASE SOURCE_BASE DESTINATION_BASE CONTROL_ADDR VOLUME_BYTES LANES CHUNK_BYTES SYSTEM_BYTES_PER_SECOND",
    )
}

pub fn main_entry() -> io::Result<()> {
    install_cutover_wake_handler()?;
    let mut args = env::args().skip(1);
    let listen = Endpoint::parse(&args.next().ok_or_else(usage)?)?;
    let source = Endpoint::parse(&args.next().ok_or_else(usage)?)?;
    let destination = Endpoint::parse(&args.next().ok_or_else(usage)?)?;
    let control_addr = resolve_one(&args.next().ok_or_else(usage)?)?;
    let volume_bytes = args
        .next()
        .ok_or_else(usage)?
        .parse::<u64>()
        .map_err(|error| invalid(error.to_string()))?;
    let lanes = args
        .next()
        .ok_or_else(usage)?
        .parse::<u32>()
        .map_err(|error| invalid(error.to_string()))?;
    let chunk_bytes = args
        .next()
        .ok_or_else(usage)?
        .parse::<usize>()
        .map_err(|error| invalid(error.to_string()))?;
    let system_bytes_per_second = args
        .next()
        .ok_or_else(usage)?
        .parse::<u64>()
        .map_err(|error| invalid(error.to_string()))?;
    if args.next().is_some() {
        return Err(usage());
    }
    let nonempty_env = |name: &str| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let transport = match env::var("ZCNBLK_WAL_MIGRATION_TRANSPORT")
        .unwrap_or_else(|_| "tcp".to_string())
        .as_str()
    {
        "tcp" => LiveMigrationTransport::Tcp,
        "ofi" | "rdm" | "efa" => LiveMigrationTransport::Ofi(OfiLiveTransport {
            provider: env::var("ZCNBLK_WAL_MIGRATION_OFI_PROVIDER")
                .unwrap_or_else(|_| "efa".to_string()),
            endpoint: env::var("ZCNBLK_WAL_MIGRATION_OFI_ENDPOINT")
                .unwrap_or_else(|_| "rdm".to_string()),
            ingress_rma_capable: match env::var("ZCNBLK_WAL_MIGRATION_INGRESS_OFI_RMA_CAPABLE")
                .unwrap_or_else(|_| "0".to_string())
                .as_str()
            {
                "0" | "false" | "no" | "off" => false,
                "1" | "true" | "yes" | "on" => true,
                value => {
                    return Err(invalid(format!(
                        "invalid ZCNBLK_WAL_MIGRATION_INGRESS_OFI_RMA_CAPABLE={value:?}"
                    )));
                }
            },
            ingress_domain: nonempty_env("ZCNBLK_WAL_MIGRATION_INGRESS_OFI_DOMAIN"),
            source_domain: nonempty_env("ZCNBLK_WAL_MIGRATION_SOURCE_OFI_DOMAIN"),
            destination_domain: nonempty_env("ZCNBLK_WAL_MIGRATION_DESTINATION_OFI_DOMAIN"),
        }),
        value => {
            return Err(invalid(format!(
                "unknown migration transport {value:?}; use tcp or ofi"
            )));
        }
    };
    let ingress_transport =
        env::var("ZCNBLK_WAL_MIGRATION_INGRESS_TRANSPORT").unwrap_or_else(|_| "tcp".to_string());
    match ingress_transport.as_str() {
        "tcp" => {}
        "ofi" | "rdm" | "efa" if matches!(transport, LiveMigrationTransport::Ofi(_)) => {}
        "ofi" | "rdm" | "efa" => {
            return Err(invalid(
                "OFI migration ingress requires an OFI migration leaf transport",
            ));
        }
        value => {
            return Err(invalid(format!(
                "unknown migration ingress transport {value:?}; use tcp or ofi"
            )));
        }
    }
    let method = match env::var("ZCNBLK_WAL_MIGRATION_TCP_COPY")
        .unwrap_or_else(|_| "splice".to_string())
        .as_str()
    {
        "splice" | "zero-copy" => TcpBulkCopyMethod::Splice,
        "buffered" | "copy" => TcpBulkCopyMethod::Buffered,
        value => return Err(invalid(format!("unknown TCP copy method {value:?}"))),
    };
    let retention_records = env::var("ZCNBLK_WAL_MIGRATION_RETENTION_RECORDS")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|error| invalid(error.to_string()))?
        .unwrap_or(1 << 16);
    if retention_records == 0 || !retention_records.is_power_of_two() {
        return Err(invalid(
            "ZCNBLK_WAL_MIGRATION_RETENTION_RECORDS must be a non-zero power of two",
        ));
    }
    let replay_poll = Duration::from_micros(
        env::var("ZCNBLK_WAL_MIGRATION_REPLAY_POLL_US")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()
            .map_err(|error| invalid(error.to_string()))?
            .unwrap_or(50)
            .max(1),
    );
    let replay_window = env::var("ZCNBLK_WAL_MIGRATION_REPLAY_WINDOW")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|error| invalid(error.to_string()))?
        .unwrap_or(64);
    if replay_window == 0 {
        return Err(invalid(
            "ZCNBLK_WAL_MIGRATION_REPLAY_WINDOW must be non-zero",
        ));
    }
    migration_topology_preflight(lanes, &transport)?;
    let coordinator = Arc::new(MigrationCoordinator::new(lanes, system_bytes_per_second)?);
    let control = TcpListener::bind(control_addr)?;
    let control_coordinator = Arc::clone(&coordinator);
    thread::Builder::new()
        .name("zcwal-migration-control".into())
        .spawn(move || {
            if let Err(error) = run_control(control, control_coordinator) {
                eprintln!("zcnblk-wal-live-migration-control-fatal: {error}");
            }
        })?;
    println!(
        "zcnblk-wal-live-migration: listen={}:{} source={}:{} destination={}:{} control={control_addr} volume_bytes={volume_bytes} lanes={lanes} chunk_bytes={chunk_bytes} system_bytes_per_second={system_bytes_per_second} replay_window={replay_window} replay_completion=ordered-pipelined-result-hwm ingress_transport={ingress_transport} leaf_transport={} topology=client-block-edge->userspace-live-migration->userspace-terminal-leaf placement_owner=userspace kernel_placement=no base_payload={} retained_tail_payload=owned-original-buffer cutover=lane-frame-boundary+destination-sync-hwm+placement-epoch client_reconnect=false",
        listen.host,
        listen.base_port,
        source.host,
        source.base_port,
        destination.host,
        destination.base_port,
        transport.label(),
        match &transport {
            LiveMigrationTransport::Tcp => match method {
                TcpBulkCopyMethod::Splice => "socket-pipe-socket-splice-zero-userspace-buffer",
                TcpBulkCopyMethod::Buffered => "buffered-compatibility",
            },
            LiveMigrationTransport::Ofi(_) => {
                "source-rma-read->one-registered-arena->destination-rma-write-zero-cpu-copy"
            }
        },
    );

    let mut handles = Vec::with_capacity(lanes as usize);
    for lane in 0..lanes {
        let source = source.clone();
        let destination = destination.clone();
        let coordinator = Arc::clone(&coordinator);
        let lane_transport = transport.clone();
        let lane_ingress_transport = ingress_transport.clone();
        let listen = listen.clone();
        handles.push(thread::Builder::new().name(format!(
            "zcwal-migration-listen-{lane}"
        )).spawn(move || -> io::Result<()> {
            match lane_ingress_transport.as_str() {
                "tcp" => {
                    let listener = TcpListener::bind(listen.lane_addr(lane)?)?;
                    for accepted in listener.incoming() {
                        let upstream = LiveWalStream::Tcp(accepted?);
                        let source = source.clone();
                        let destination = destination.clone();
                        let coordinator = Arc::clone(&coordinator);
                        let session_transport = lane_transport.clone();
                        thread::Builder::new()
                            .name(format!("zcwal-migration-session-{lane}"))
                            .spawn(move || {
                                if let Err(error) = proxy_foreground_session(
                                    upstream,
                                    session_transport,
                                    source,
                                    destination,
                                    coordinator,
                                    lane,
                                    volume_bytes,
                                    chunk_bytes,
                                    method,
                                    retention_records,
                                    replay_poll,
                                    replay_window,
                                ) {
                                    eprintln!(
                                        "zcnblk-wal-live-migration-session-error: lane={lane} error={error}"
                                    );
                                }
                            })?;
                    }
                    Ok(())
                }
                "ofi" | "rdm" | "efa" => {
                    let LiveMigrationTransport::Ofi(ofi) = &lane_transport else {
                        return Err(invalid(
                            "OFI migration ingress requires an OFI migration leaf transport",
                        ));
                    };
                    let service = ofi_connection_service(listen.base_port, lane, lanes, 0)?;
                    let upstream = ZcOfiMessageStream::connect_on_domain(
                        &ofi.provider,
                        &ofi.endpoint,
                        &listen.host,
                        service,
                        true,
                        ofi.ingress_rma_capable,
                        ofi.ingress_domain.as_deref(),
                    )?;
                    proxy_foreground_session(
                        LiveWalStream::Ofi(upstream),
                        lane_transport,
                        source,
                        destination,
                        coordinator,
                        lane,
                        volume_bytes,
                        chunk_bytes,
                        method,
                        retention_records,
                        replay_poll,
                        replay_window,
                    )
                }
                _ => unreachable!("migration ingress transport validated before lane startup"),
            }
        })?);
    }
    for handle in handles {
        handle
            .join()
            .map_err(|_| io::Error::other("live migration listener panicked"))??;
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ZCNBLK_FAN_WAL_OP_EOF, ZCNBLK_FAN_WAL_OP_HELLO, ZCNBLK_FAN_WAL_OP_SYNC};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::{Mutex, OnceLock};

    static OFI_MIGRATION_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn free_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    fn free_ofi_base() -> u16 {
        for base in 20_000u16..28_000u16 {
            let ports = [base, base + 1, base + 1000, base + 1001];
            let listeners = ports
                .into_iter()
                .map(|port| TcpListener::bind(("127.0.0.1", port)))
                .collect::<Result<Vec<_>, _>>();
            if let Ok(listeners) = listeners {
                drop(listeners);
                return base;
            }
        }
        panic!("could not reserve two OFI data/control services");
    }

    fn free_ofi_live_bases() -> (u16, u16, u16) {
        for base in (20_000u16..26_000u16).step_by(32) {
            let source = base;
            let destination = base + 8;
            let gateway = base + 16;
            let services = [
                source,
                source + 1,
                destination,
                destination + 1,
                destination + 2,
                gateway,
            ];
            let ports = services
                .into_iter()
                .flat_map(|service| [service, service + 1000]);
            let listeners = ports
                .map(|port| TcpListener::bind(("127.0.0.1", port)))
                .collect::<Result<Vec<_>, _>>();
            if let Ok(listeners) = listeners {
                drop(listeners);
                return (source, destination, gateway);
            }
        }
        panic!("could not reserve OFI live-migration services");
    }

    fn fake_leaf(address: SocketAddr, storage: Arc<Mutex<Vec<u8>>>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let listener = TcpListener::bind(address).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            loop {
                let (frame, payload) = match read_frame(&mut stream) {
                    Ok(frame) => frame,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return,
                    Err(error) => panic!("fake leaf read failed: {error}"),
                };
                match frame.op {
                    ZCNBLK_FAN_WAL_OP_HELLO => {
                        write_frame(
                            &mut stream,
                            ZcnblkFanWalFrame {
                                op: ZCNBLK_FAN_WAL_OP_HELLO_ACK,
                                ..frame
                            },
                            &[],
                        )
                        .unwrap();
                    }
                    ZCNBLK_FAN_WAL_OP_READ_DESC => {
                        assert!(payload.is_empty());
                        let start = frame.leaf_offset as usize;
                        let end = start + frame.payload_len as usize;
                        let result_payload = storage.lock().unwrap()[start..end].to_vec();
                        write_frame(
                            &mut stream,
                            ZcnblkFanWalFrame {
                                op: ZCNBLK_FAN_WAL_OP_RESULT,
                                status: ZCNBLK_FAN_WAL_STATUS_OK,
                                ..frame
                            },
                            &result_payload,
                        )
                        .unwrap();
                    }
                    ZCNBLK_FAN_WAL_OP_WRITE_DESC => {
                        let start = frame.leaf_offset as usize;
                        let end = start + payload.len();
                        storage.lock().unwrap()[start..end].copy_from_slice(&payload);
                        write_frame(
                            &mut stream,
                            ZcnblkFanWalFrame {
                                op: ZCNBLK_FAN_WAL_OP_RESULT,
                                status: ZCNBLK_FAN_WAL_STATUS_OK,
                                payload_len: 0,
                                ..frame
                            },
                            &[],
                        )
                        .unwrap();
                    }
                    ZCNBLK_FAN_WAL_OP_SYNC => {
                        write_frame(
                            &mut stream,
                            ZcnblkFanWalFrame {
                                op: ZCNBLK_FAN_WAL_OP_RESULT,
                                status: ZCNBLK_FAN_WAL_STATUS_OK,
                                ..frame
                            },
                            &[],
                        )
                        .unwrap();
                    }
                    ZCNBLK_FAN_WAL_OP_EOF => return,
                    other => panic!("unexpected fake-leaf operation {other}"),
                }
            }
        })
    }

    fn fake_leaf_multi(
        address: SocketAddr,
        storage: Arc<Mutex<Vec<u8>>>,
        connections: usize,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let listener = TcpListener::bind(address).unwrap();
            let mut sessions = Vec::new();
            for _ in 0..connections {
                let (mut stream, _) = listener.accept().unwrap();
                let storage = Arc::clone(&storage);
                sessions.push(thread::spawn(move || {
                    loop {
                        let (frame, payload) = match read_frame(&mut stream) {
                            Ok(frame) => frame,
                            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return,
                            Err(error) => panic!("fake leaf read failed: {error}"),
                        };
                        match frame.op {
                            ZCNBLK_FAN_WAL_OP_HELLO => write_frame(
                                &mut stream,
                                ZcnblkFanWalFrame {
                                    op: ZCNBLK_FAN_WAL_OP_HELLO_ACK,
                                    ..frame
                                },
                                &[],
                            )
                            .unwrap(),
                            ZCNBLK_FAN_WAL_OP_READ_DESC => {
                                let start = frame.leaf_offset as usize;
                                let end = start + frame.payload_len as usize;
                                let result_payload = storage.lock().unwrap()[start..end].to_vec();
                                write_frame(
                                    &mut stream,
                                    ZcnblkFanWalFrame {
                                        op: ZCNBLK_FAN_WAL_OP_RESULT,
                                        status: ZCNBLK_FAN_WAL_STATUS_OK,
                                        ..frame
                                    },
                                    &result_payload,
                                )
                                .unwrap();
                            }
                            ZCNBLK_FAN_WAL_OP_WRITE_DESC => {
                                let start = frame.leaf_offset as usize;
                                let end = start + payload.len();
                                storage.lock().unwrap()[start..end].copy_from_slice(&payload);
                                write_frame(
                                    &mut stream,
                                    ZcnblkFanWalFrame {
                                        op: ZCNBLK_FAN_WAL_OP_RESULT,
                                        status: ZCNBLK_FAN_WAL_STATUS_OK,
                                        payload_len: 0,
                                        ..frame
                                    },
                                    &[],
                                )
                                .unwrap();
                            }
                            ZCNBLK_FAN_WAL_OP_SYNC => write_frame(
                                &mut stream,
                                ZcnblkFanWalFrame {
                                    op: ZCNBLK_FAN_WAL_OP_RESULT,
                                    status: ZCNBLK_FAN_WAL_STATUS_OK,
                                    payload_len: 0,
                                    ..frame
                                },
                                &[],
                            )
                            .unwrap(),
                            ZCNBLK_FAN_WAL_OP_EOF => return,
                            other => panic!("unexpected fake-leaf operation {other}"),
                        }
                    }
                }));
            }
            for session in sessions {
                session.join().unwrap();
            }
        })
    }

    fn client_write<S: Read + Write>(
        stream: &mut S,
        hello: ZcnblkFanWalFrame,
        sequence: u64,
        offset: u64,
        value: u8,
    ) {
        let payload = vec![value; IO_ALIGNMENT as usize];
        let request = migration_descriptor(
            hello,
            ZCNBLK_FAN_WAL_OP_WRITE_DESC,
            ZCNBLK_OP_WRITE,
            offset,
            payload.len(),
            sequence,
        )
        .unwrap();
        write_stream_frame(stream, request, &payload).unwrap();
        let (result, result_payload) = read_stream_frame(stream).unwrap();
        assert_eq!(result.op, ZCNBLK_FAN_WAL_OP_RESULT);
        assert!(result_payload.is_empty());
    }

    fn client_read<S: Read + Write>(
        stream: &mut S,
        hello: ZcnblkFanWalFrame,
        sequence: u64,
        offset: u64,
    ) -> Vec<u8> {
        let request = migration_descriptor(
            hello,
            ZCNBLK_FAN_WAL_OP_READ_DESC,
            ZCNBLK_OP_READ,
            offset,
            IO_ALIGNMENT as usize,
            sequence,
        )
        .unwrap();
        write_stream_frame(stream, request, &[]).unwrap();
        let (result, payload) = read_stream_frame(stream).unwrap();
        assert_eq!(result.op, ZCNBLK_FAN_WAL_OP_RESULT);
        assert_eq!(payload.len(), IO_ALIGNMENT as usize);
        payload
    }

    fn client_write_payload<S: Read + Write>(
        stream: &mut S,
        hello: ZcnblkFanWalFrame,
        sequence: u64,
        offset: u64,
        payload: &[u8],
    ) {
        let request = migration_descriptor(
            hello,
            ZCNBLK_FAN_WAL_OP_WRITE_DESC,
            ZCNBLK_OP_WRITE,
            offset,
            payload.len(),
            sequence,
        )
        .unwrap();
        write_stream_frame(stream, request, payload).unwrap();
        let (result, result_payload) = read_stream_frame(stream).unwrap();
        assert_eq!(result.op, ZCNBLK_FAN_WAL_OP_RESULT);
        assert_eq!(result.status, ZCNBLK_FAN_WAL_STATUS_OK);
        assert!(result_payload.is_empty());
    }

    fn client_read_payload<S: Read + Write>(
        stream: &mut S,
        hello: ZcnblkFanWalFrame,
        sequence: u64,
        offset: u64,
        len: usize,
    ) -> Vec<u8> {
        let request = migration_descriptor(
            hello,
            ZCNBLK_FAN_WAL_OP_READ_DESC,
            ZCNBLK_OP_READ,
            offset,
            len,
            sequence,
        )
        .unwrap();
        write_stream_frame(stream, request, &[]).unwrap();
        let (result, payload) = read_stream_frame(stream).unwrap();
        assert_eq!(result.op, ZCNBLK_FAN_WAL_OP_RESULT);
        assert_eq!(result.status, ZCNBLK_FAN_WAL_STATUS_OK);
        assert_eq!(payload.len(), len);
        payload
    }

    #[test]
    fn retained_queue_moves_payload_ownership_without_a_mutex() {
        let queue = RetainedWalQueue::new(4).unwrap();
        let (mut producer, mut consumer) = queue.split();
        let payload = vec![0xa5; 4096];
        let allocation = payload.as_ptr();
        producer.try_push(payload).unwrap();
        let received = consumer.try_pop().unwrap();
        assert_eq!(received.as_ptr(), allocation);
        assert_eq!(received, vec![0xa5; 4096]);
        assert!(consumer.is_empty());
    }

    #[test]
    fn retained_queue_applies_bounded_backpressure() {
        let queue = RetainedWalQueue::new(2).unwrap();
        let (mut producer, mut consumer) = queue.split();
        producer.try_push(1).unwrap();
        producer.try_push(2).unwrap();
        assert_eq!(producer.try_push(3), Err(3));
        assert_eq!(consumer.try_pop(), Some(1));
        producer.try_push(3).unwrap();
        assert_eq!(consumer.try_pop(), Some(2));
        assert_eq!(consumer.try_pop(), Some(3));
    }

    #[test]
    fn system_chunk_pacing_uses_exact_cumulative_deadlines_and_lane_phases() {
        let chunk_period = paced_duration(16 * 1024 * 1024, 256 * 1024 * 1024);
        assert_eq!(chunk_period, Duration::from_micros(62_500));
        assert_eq!(
            paced_duration(64 * 1024 * 1024, 256 * 1024 * 1024),
            Duration::from_millis(250)
        );
        assert_eq!(scale_duration(chunk_period, 0, 16), Duration::ZERO);
        assert_eq!(
            scale_duration(chunk_period, 15, 16),
            Duration::from_nanos(58_593_750)
        );
        let period = Duration::from_nanos(1_000);
        assert_eq!(
            initial_lane_pacing_delay(1_000_000, 1_000_100, period, 1, 4),
            Duration::from_nanos(150)
        );
        assert_eq!(
            initial_lane_pacing_delay(1_000_000, 1_000_300, period, 1, 4),
            Duration::from_nanos(950)
        );
        assert_eq!(
            initial_lane_pacing_delay(1_000_000, 1_000_250, period, 1, 4),
            Duration::ZERO
        );
    }

    #[test]
    fn tcp_splice_base_copy_moves_no_payload_through_a_userspace_copy_buffer() {
        let bytes = 8 * 1024 * 1024usize;
        let source_data = (0..bytes)
            .map(|index| (index.wrapping_mul(131) as u8).wrapping_add(17))
            .collect::<Vec<_>>();
        let source_storage = Arc::new(Mutex::new(source_data.clone()));
        let destination_storage = Arc::new(Mutex::new(vec![0u8; bytes]));
        let source_addr = free_addr();
        let destination_addr = free_addr();
        let source_leaf = fake_leaf(source_addr, Arc::clone(&source_storage));
        let destination_leaf = fake_leaf(destination_addr, Arc::clone(&destination_storage));
        thread::sleep(Duration::from_millis(20));

        let source = Endpoint::parse(&source_addr.to_string()).unwrap();
        let destination = Endpoint::parse(&destination_addr.to_string()).unwrap();
        let hello = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_HELLO,
            lane_id: 0,
            lane_count: 1,
            branch_count: 1,
            request_id: 0x91a2,
            placement_epoch: 7,
            ..ZcnblkFanWalFrame::default()
        };
        let stats = tcp_base_copy_lane(
            &source,
            &destination,
            hello,
            WalBaseCopySpec {
                volume_bytes: bytes as u64,
                chunk_bytes: 1024 * 1024,
                lane_id: 0,
                lane_count: 1,
                method: TcpBulkCopyMethod::Splice,
            },
            None,
        )
        .unwrap();
        source_leaf.join().unwrap();
        destination_leaf.join().unwrap();
        assert_eq!(*destination_storage.lock().unwrap(), source_data);
        assert_eq!(stats.bytes_copied, bytes as u64);
        assert_eq!(stats.ranges_copied, 8);
        assert_eq!(stats.payload_userspace_buffers, 0);
        assert!(stats.splice_payload_syscalls >= 16);
    }

    #[test]
    fn retained_wal_cutover_keeps_one_client_session_and_moves_the_hwm() {
        let bytes = 8 * 1024 * 1024usize;
        let initial = (0..bytes)
            .map(|index| (index.wrapping_mul(29) as u8).wrapping_add(3))
            .collect::<Vec<_>>();
        let source_storage = Arc::new(Mutex::new(initial));
        let destination_storage = Arc::new(Mutex::new(vec![0u8; bytes]));
        let source_addr = free_addr();
        let destination_addr = free_addr();
        let gateway_addr = free_addr();
        let source_leaf = fake_leaf_multi(source_addr, Arc::clone(&source_storage), 2);
        let destination_leaf =
            fake_leaf_multi(destination_addr, Arc::clone(&destination_storage), 3);
        thread::sleep(Duration::from_millis(20));

        let source = Endpoint::parse(&source_addr.to_string()).unwrap();
        let destination = Endpoint::parse(&destination_addr.to_string()).unwrap();
        let coordinator = Arc::new(MigrationCoordinator::new(1, 64 * 1024 * 1024).unwrap());
        let gateway_coordinator = Arc::clone(&coordinator);
        let gateway_source = source.clone();
        let gateway_destination = destination.clone();
        let gateway = thread::spawn(move || {
            let listener = TcpListener::bind(gateway_addr).unwrap();
            let (upstream, _) = listener.accept().unwrap();
            proxy_foreground_session(
                upstream.into(),
                LiveMigrationTransport::Tcp,
                gateway_source,
                gateway_destination,
                gateway_coordinator,
                0,
                bytes as u64,
                64 * 1024,
                TcpBulkCopyMethod::Splice,
                1024,
                Duration::from_micros(50),
                64,
            )
            .unwrap();
        });
        thread::sleep(Duration::from_millis(20));

        let mut client = TcpStream::connect(gateway_addr).unwrap();
        client.set_nodelay(true).unwrap();
        let client_identity = client.local_addr().unwrap();
        let hello = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_HELLO,
            lane_id: 0,
            lane_count: 1,
            branch_count: 1,
            request_id: 0x7788,
            placement_epoch: 11,
            ..ZcnblkFanWalFrame::default()
        };
        write_frame(&mut client, hello, &[]).unwrap();
        assert_eq!(
            read_frame(&mut client).unwrap().0.op,
            ZCNBLK_FAN_WAL_OP_HELLO_ACK
        );
        client_write(&mut client, hello, 1, 0, 0x31);
        coordinator.start().unwrap();

        // These writes race the striped base copy and must be replayed from
        // the retained original payload allocation before ownership moves.
        for sequence in 2..66u64 {
            let page = (sequence * 7919) % (bytes as u64 / IO_ALIGNMENT);
            client_write(
                &mut client,
                hello,
                sequence,
                page * IO_ALIGNMENT,
                sequence as u8,
            );
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !coordinator.lanes[0].base_ready.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "base copy did not complete");
            thread::sleep(Duration::from_millis(1));
        }
        coordinator.request_cutover().unwrap();
        let cutover_deadline = Instant::now() + Duration::from_secs(5);
        while coordinator.phase.load(Ordering::Acquire) != PHASE_ACTIVE_SECONDARY {
            assert!(
                Instant::now() < cutover_deadline,
                "lane cutover did not complete: {}",
                coordinator.status()
            );
            thread::sleep(Duration::from_millis(1));
        }
        // No client frame drives this transition: the controller must wake an
        // idle lane and publish the destination route itself.
        let cutover_read = client_read(&mut client, hello, 66, 0);
        assert_eq!(cutover_read, vec![0x31; IO_ALIGNMENT as usize]);
        assert_eq!(client.local_addr().unwrap(), client_identity);
        assert_eq!(
            coordinator.phase.load(Ordering::Acquire),
            PHASE_ACTIVE_SECONDARY
        );
        assert_eq!(
            coordinator.lanes[0].destination_hwm.load(Ordering::Acquire),
            coordinator.lanes[0].source_hwm.load(Ordering::Acquire)
        );
        let route = coordinator.lanes[0]
            .route_hwm
            .load_effective(monotonic_time_ns().unwrap())
            .unwrap();
        assert_eq!(route.placement_epoch, 2);
        assert_eq!(route.applied_hwm, 66);

        // Prove the same established client now writes and reads the new leaf.
        client_write(&mut client, hello, 67, IO_ALIGNMENT, 0xe7);
        assert_eq!(
            client_read(&mut client, hello, 68, IO_ALIGNMENT),
            vec![0xe7; IO_ALIGNMENT as usize]
        );
        let eof = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_EOF,
            lane_id: 0,
            lane_count: 1,
            request_id: hello.request_id,
            placement_epoch: hello.placement_epoch,
            ..ZcnblkFanWalFrame::default()
        };
        write_frame(&mut client, eof, &[]).unwrap();
        drop(client);
        gateway.join().unwrap();
        source_leaf.join().unwrap();
        destination_leaf.join().unwrap();

        let mut expected = source_storage.lock().unwrap().clone();
        expected[IO_ALIGNMENT as usize..2 * IO_ALIGNMENT as usize].fill(0xe7);
        assert_eq!(*destination_storage.lock().unwrap(), expected);
    }

    #[test]
    #[ignore = "requires a local libfabric sockets provider with FI_RMA"]
    fn ofi_live_cutover_keeps_one_framed_client_session_and_exact_destination() {
        let _guard = OFI_MIGRATION_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        unsafe {
            env::set_var("URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT", "ofi");
            env::set_var("URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER", "sockets");
            env::set_var("URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT", "rdm");
            env::set_var("URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS", "1");
            env::set_var("URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_WRITES", "1");
            env::set_var("URING_PLAY_ZCNBLK_WAL_RESULT_RANGES", "1");
            env::set_var("URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC", "1");
            env::set_var("URING_PLAY_OFI_CQ_SLEEP_NS", "0");
            env::set_var("URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES", "1048576");
        }

        let bytes = 8 * 1024 * 1024usize;
        let (source_base, destination_base, gateway_base) = free_ofi_live_bases();
        let source_leaf = thread::spawn(move || {
            crate::zcnblk_wal_leaf(
                &format!("zcmem:{bytes}"),
                "127.0.0.1",
                source_base,
                1,
                2,
                4096,
                1,
                false,
                crate::ZcnblkWalLeafIoMode::Blocking,
            )
        });
        let destination_leaf = thread::spawn(move || {
            crate::zcnblk_wal_leaf(
                &format!("zcmem:{bytes}"),
                "127.0.0.1",
                destination_base,
                1,
                3,
                4096,
                1,
                false,
                crate::ZcnblkWalLeafIoMode::Blocking,
            )
        });
        thread::sleep(Duration::from_millis(25));

        let coordinator = Arc::new(MigrationCoordinator::new(1, u64::MAX).unwrap());
        let transport = LiveMigrationTransport::Ofi(OfiLiveTransport {
            provider: "sockets".into(),
            endpoint: "rdm".into(),
            ingress_rma_capable: true,
            ingress_domain: None,
            source_domain: None,
            destination_domain: None,
        });
        let gateway_coordinator = Arc::clone(&coordinator);
        let gateway_transport = transport.clone();
        let gateway = thread::spawn(move || {
            let upstream = ZcOfiMessageStream::connect(
                "sockets",
                "rdm",
                "127.0.0.1",
                gateway_base,
                true,
                true,
            )
            .unwrap();
            proxy_foreground_session(
                LiveWalStream::Ofi(upstream),
                gateway_transport,
                Endpoint {
                    host: "127.0.0.1".into(),
                    base_port: source_base,
                },
                Endpoint {
                    host: "127.0.0.1".into(),
                    base_port: destination_base,
                },
                gateway_coordinator,
                0,
                bytes as u64,
                1024 * 1024,
                TcpBulkCopyMethod::Splice,
                1024,
                Duration::from_micros(50),
                64,
            )
            .unwrap();
        });
        let mut client =
            ZcOfiMessageStream::connect("sockets", "rdm", "127.0.0.1", gateway_base, false, true)
                .unwrap();
        let mut hello = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_HELLO,
            lane_id: 0,
            lane_count: 1,
            branch_count: 1,
            request_id: 0x0f1c_0f1c,
            placement_epoch: 1,
            ..ZcnblkFanWalFrame::default()
        }
        .with_hello_features(ZCNBLK_WAL_FEATURE_ALL)
        .unwrap();
        hello.flags |= ZCNBLK_FAN_WAL_FLAG_RESULT_RANGE_BATCH
            | ZCNBLK_FAN_WAL_FLAG_OFI_RMA_READ_WINDOW
            | ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_WINDOW;
        write_stream_frame(&mut client, hello, &[]).unwrap();
        let (ack, payload) = read_stream_frame(&mut client).unwrap();
        assert_eq!(ack.op, ZCNBLK_FAN_WAL_OP_HELLO_ACK);
        assert!(payload.is_empty());
        assert_eq!(
            ack.flags
                & (ZCNBLK_FAN_WAL_FLAG_OFI_RMA_READ_WINDOW
                    | ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_WINDOW),
            0,
            "an intermediary must not re-advertise a terminal leaf RMA key",
        );

        let mut expected = (0..bytes)
            .map(|index| (index.wrapping_mul(31) as u8).wrapping_add(7))
            .collect::<Vec<_>>();
        let one_mib = 1024 * 1024;
        for (range, payload) in expected.chunks(one_mib).enumerate() {
            client_write_payload(
                &mut client,
                hello,
                range as u64 + 1,
                (range * one_mib) as u64,
                payload,
            );
        }
        coordinator.start().unwrap();
        for index in 0..64usize {
            let offset = ((index * 17) % (bytes / IO_ALIGNMENT as usize)) * IO_ALIGNMENT as usize;
            let value = 0x40u8.wrapping_add(index as u8);
            client_write(&mut client, hello, 9 + index as u64, offset as u64, value);
            expected[offset..offset + IO_ALIGNMENT as usize].fill(value);
        }
        coordinator.request_cutover().unwrap();
        let final_offset = bytes - IO_ALIGNMENT as usize;
        client_write(&mut client, hello, 100, final_offset as u64, 0xe3);
        expected[final_offset..].fill(0xe3);
        // A cutover request arms the route change; it must not quiesce the
        // foreground lane until base copy is complete.  Keep the framed
        // session busy long enough for the gateway to observe readiness and
        // cross a real request boundary without reconnecting the client.
        for request_id in 101..357u64 {
            if coordinator.phase.load(Ordering::Acquire) == PHASE_ACTIVE_SECONDARY {
                break;
            }
            let offset = ((request_id as usize * 29) % (bytes / IO_ALIGNMENT as usize))
                * IO_ALIGNMENT as usize;
            let value = request_id as u8;
            client_write(&mut client, hello, request_id, offset as u64, value);
            expected[offset..offset + IO_ALIGNMENT as usize].fill(value);
        }
        assert_eq!(
            coordinator.phase.load(Ordering::Acquire),
            PHASE_ACTIVE_SECONDARY
        );
        assert_eq!(
            coordinator.lanes[0].source_hwm.load(Ordering::Acquire),
            coordinator.lanes[0].destination_hwm.load(Ordering::Acquire)
        );
        assert_eq!(
            coordinator.lanes[0].copy_bytes.load(Ordering::Acquire),
            bytes as u64
        );
        assert!(coordinator.lanes[0].replay_records.load(Ordering::Acquire) > 0);

        for range in 0..(bytes / one_mib) {
            let observed = client_read_payload(
                &mut client,
                hello,
                200 + range as u64,
                (range * one_mib) as u64,
                one_mib,
            );
            assert_eq!(observed, expected[range * one_mib..(range + 1) * one_mib]);
        }
        write_stream_frame(
            &mut client,
            ZcnblkFanWalFrame {
                op: ZCNBLK_FAN_WAL_OP_EOF,
                lane_id: 0,
                lane_count: 1,
                branch_count: 1,
                request_id: hello.request_id,
                placement_epoch: hello.placement_epoch,
                ..ZcnblkFanWalFrame::default()
            },
            &[],
        )
        .unwrap();
        drop(client);
        gateway.join().unwrap();
        source_leaf.join().unwrap().unwrap();
        destination_leaf.join().unwrap().unwrap();
    }

    #[test]
    #[ignore = "requires a local libfabric sockets provider with FI_RMA"]
    fn ofi_rma_base_copy_uses_one_registered_arena_and_no_cpu_payload_copy() {
        let _guard = OFI_MIGRATION_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        unsafe {
            env::set_var("URING_PLAY_ZCNBLK_WAL_LEAF_TRANSPORT", "ofi");
            env::set_var("URING_PLAY_ZCNBLK_WAL_LEAF_OFI_PROVIDER", "sockets");
            env::set_var("URING_PLAY_ZCNBLK_WAL_LEAF_OFI_ENDPOINT", "rdm");
            env::set_var("URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_READS", "1");
            env::set_var("URING_PLAY_ZCNBLK_WAL_LEAF_OFI_RMA_WRITES", "1");
            env::set_var("URING_PLAY_ZCNBLK_WAL_RESULT_RANGES", "1");
            env::set_var("URING_PLAY_OFI_CQ_SLEEP_NS", "0");
            env::set_var("URING_PLAY_ZCNBLK_WAL_OFI_MESSAGE_BYTES", "1048576");
        }
        let bytes = 8 * 1024 * 1024usize;
        let source_base = free_ofi_base();
        let destination_base = source_base + 10;
        let source_leaf = thread::spawn(move || {
            crate::zcnblk_wal_leaf(
                &format!("zcmem:{bytes}"),
                "127.0.0.1",
                source_base,
                1,
                2,
                4096,
                1,
                false,
                crate::ZcnblkWalLeafIoMode::Blocking,
            )
        });
        let destination_leaf = thread::spawn(move || {
            crate::zcnblk_wal_leaf(
                &format!("zcmem:{bytes}"),
                "127.0.0.1",
                destination_base,
                1,
                2,
                4096,
                1,
                false,
                crate::ZcnblkWalLeafIoMode::Blocking,
            )
        });
        thread::sleep(Duration::from_millis(25));

        let mut source_foreground =
            ZcOfiMessageStream::connect("sockets", "rdm", "127.0.0.1", source_base, false, true)
                .unwrap();
        let mut destination_foreground = ZcOfiMessageStream::connect(
            "sockets",
            "rdm",
            "127.0.0.1",
            destination_base,
            false,
            true,
        )
        .unwrap();
        let hello = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_HELLO,
            lane_id: 0,
            lane_count: 1,
            branch_count: 1,
            request_id: 0xa771,
            placement_epoch: 4,
            ..ZcnblkFanWalFrame::default()
        };
        let mailbox = Arc::new(SystemTaskGrantMailbox::new(SystemTaskGrantSnapshot {
            generation: 1,
            target_iops: 0,
            target_bytes_per_second: 0,
            effective_ns: 0,
            valid_until_ns: 0,
            fallback_iops: 0,
            fallback_bytes_per_second: 0,
        }));
        let cancelled = Arc::new(AtomicBool::new(false));
        let copy_mailbox = Arc::clone(&mailbox);
        let copy_cancelled = Arc::clone(&cancelled);
        let copier = thread::spawn(move || {
            let managed = ManagedWalCopy {
                grants: &copy_mailbox,
                cancelled: &copy_cancelled,
                idle_wait: Duration::from_millis(1),
            };
            ofi_rma_base_copy_lane(
                &OfiBaseCopyConfig {
                    provider: "sockets".into(),
                    endpoint: "rdm".into(),
                    source: OfiWalEndpoint {
                        node: "127.0.0.1".into(),
                        base_service: source_base,
                        domain: None,
                    },
                    destination: OfiWalEndpoint {
                        node: "127.0.0.1".into(),
                        base_service: destination_base,
                        domain: None,
                    },
                    connection_index: 1,
                },
                hello,
                WalBaseCopySpec {
                    volume_bytes: bytes as u64,
                    chunk_bytes: 1024 * 1024,
                    lane_id: 0,
                    lane_count: 1,
                    method: TcpBulkCopyMethod::Splice,
                },
                Some(&managed),
            )
        });

        let mut rma_hello = hello.with_hello_features(ZCNBLK_WAL_FEATURE_ALL).unwrap();
        rma_hello.flags |= ZCNBLK_FAN_WAL_FLAG_RESULT_RANGE_BATCH
            | ZCNBLK_FAN_WAL_FLAG_OFI_RMA_READ_WINDOW
            | ZCNBLK_FAN_WAL_FLAG_OFI_RMA_WRITE_WINDOW;
        let _source_ack = send_stream_hello(&mut source_foreground, rma_hello).unwrap();
        let destination_ack = send_stream_hello(&mut destination_foreground, rma_hello).unwrap();
        let source_data = (0..bytes)
            .map(|index| (index.wrapping_mul(73) as u8).wrapping_add(9))
            .collect::<Vec<_>>();
        for (range, payload) in source_data.chunks(1024 * 1024).enumerate() {
            let request = migration_descriptor(
                rma_hello,
                ZCNBLK_FAN_WAL_OP_WRITE_DESC,
                ZCNBLK_OP_WRITE,
                (range * 1024 * 1024) as u64,
                payload.len(),
                range as u64 + 1,
            )
            .unwrap();
            crate::zcnblk_fan_wal_write_frame(&mut source_foreground, request, payload).unwrap();
            let (result, response) = read_stream_frame(&mut source_foreground).unwrap();
            assert_eq!(result.op, ZCNBLK_FAN_WAL_OP_RESULT);
            assert!(response.is_empty());
        }
        mailbox.publish(SystemTaskGrantSnapshot {
            generation: 2,
            target_iops: 1,
            target_bytes_per_second: u64::MAX,
            effective_ns: 0,
            valid_until_ns: 0,
            fallback_iops: 0,
            fallback_bytes_per_second: 0,
        });
        let stats = copier.join().unwrap().unwrap();
        assert_eq!(stats.bytes_copied, bytes as u64);
        assert_eq!(stats.registered_staging_arenas, 1);
        assert_eq!(stats.payload_cpu_copies, 0);
        assert_eq!(stats.rma_reads, 8);
        assert_eq!(stats.rma_writes, 8);
        assert_eq!(stats.remote_hwm_doorbells, 8);

        let mut observed = vec![0u8; bytes];
        destination_foreground
            .register_rma_read_buffer(&mut observed)
            .unwrap();
        destination_foreground
            .rma_read(
                &mut observed,
                destination_ack.logical_offset,
                destination_ack.leaf_offset,
            )
            .unwrap();
        assert_eq!(observed, source_data);
        send_stream_eof(&mut source_foreground, rma_hello).unwrap();
        send_stream_eof(&mut destination_foreground, rma_hello).unwrap();
        drop(source_foreground);
        drop(destination_foreground);
        source_leaf.join().unwrap().unwrap();
        destination_leaf.join().unwrap().unwrap();
    }
}
