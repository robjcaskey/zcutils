//! Direct userspace block transport for vhost-user frontends.
//!
//! The steady-state data path is guest memory -> a pre-registered lane-local
//! arena -> libfabric RMA -> a remote userspace volume.  It never opens a
//! block device and performs no per-I/O system call.  The small TCP exchange
//! used by libfabric is setup-only.  This module owns transport lanes; mirror,
//! stripe, tier, and spill placement remain properties of the downstream
//! userspace volume service.

use super::{
    ZcOfiEndpoint, maybe_pin_current_thread, zcofi_client_exchange_peer, zcofi_control_port,
    zcofi_server_exchange_peer,
};
use std::collections::{BTreeSet, VecDeque};
use std::io;
use std::ptr::NonNull;
use std::slice;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering, fence},
};
use std::thread;

const META_MAGIC: &[u8; 8] = b"ZCVRMA01";
const CONTROL_MAGIC: &[u8; 8] = b"ZCVCTL01";
const VERSION: u16 = 1;
const META_BYTES: usize = 64;
const CONTROL_BYTES: usize = 64;
const CONTROL_BARRIER: u16 = 1;
const CONTROL_SHUTDOWN: u16 = 2;
const MAP_HUGE_2MB: libc::c_int = 21 << 26;

#[derive(Clone, Debug)]
pub struct VhostOfiClientConfig {
    pub provider: String,
    pub endpoint: String,
    pub address: String,
    pub domain: Option<String>,
    pub base_service: u16,
    pub lanes: usize,
    pub capacity_bytes: u64,
    pub require_hugetlb: bool,
}

/// Describe the per-I/O syscall contract of the selected libfabric provider.
///
/// The sockets provider is useful for local correctness tests but necessarily
/// enters the kernel network stack. EFA and verbs providers drive registered
/// NIC queues from userspace after setup, so this adapter adds no per-I/O
/// system calls on those data paths.
pub fn provider_data_path_syscalls(provider: &str) -> &'static str {
    let provider = provider.to_ascii_lowercase();
    if provider == "efa"
        || provider == "verbs"
        || provider.starts_with("verbs;")
        || provider.ends_with(";verbs")
    {
        "0"
    } else {
        "provider-dependent"
    }
}

#[derive(Clone, Debug)]
pub struct VhostOfiTargetConfig {
    pub provider: String,
    pub endpoint: String,
    pub bind: String,
    pub domain: Option<String>,
    pub base_service: u16,
    pub lanes: usize,
    pub capacity_bytes: u64,
    pub lane_cpus: Vec<usize>,
    pub require_hugetlb: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VhostOfiCompletionKind {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VhostOfiCompletion {
    pub slot: usize,
    pub user_data: u64,
    pub kind: VhostOfiCompletionKind,
}

#[derive(Clone, Copy, Debug)]
struct VolumeMeta {
    lane: u32,
    lanes: u32,
    capacity_bytes: u64,
    remote_addr: u64,
    remote_key: u64,
}

impl VolumeMeta {
    fn encode(self) -> [u8; META_BYTES] {
        let mut bytes = [0u8; META_BYTES];
        bytes[..8].copy_from_slice(META_MAGIC);
        bytes[8..10].copy_from_slice(&VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.lane.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.lanes.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.capacity_bytes.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.remote_addr.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.remote_key.to_le_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != META_BYTES || &bytes[..8] != META_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid direct-OFI volume metadata",
            ));
        }
        let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported direct-OFI metadata version {version}"),
            ));
        }
        Ok(Self {
            lane: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            lanes: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            capacity_bytes: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
            remote_addr: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            remote_key: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct ControlFrame {
    operation: u16,
    lane: u32,
    sequence: u64,
}

impl ControlFrame {
    fn encode(self) -> [u8; CONTROL_BYTES] {
        let mut bytes = [0u8; CONTROL_BYTES];
        bytes[..8].copy_from_slice(CONTROL_MAGIC);
        bytes[8..10].copy_from_slice(&VERSION.to_le_bytes());
        bytes[10..12].copy_from_slice(&self.operation.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.lane.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.sequence.to_le_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != CONTROL_BYTES || &bytes[..8] != CONTROL_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid direct-OFI control frame",
            ));
        }
        let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported direct-OFI control version {version}"),
            ));
        }
        Ok(Self {
            operation: u16::from_le_bytes(bytes[10..12].try_into().unwrap()),
            lane: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            sequence: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        })
    }
}

/// Page-backed memory whose address is stable for its complete lifetime.
struct RegisteredRegion {
    pointer: NonNull<u8>,
    len: usize,
    hugetlb: bool,
}

unsafe impl Send for RegisteredRegion {}
unsafe impl Sync for RegisteredRegion {}

impl RegisteredRegion {
    fn new(len: usize, require_hugetlb: bool) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "registered region must not be empty",
            ));
        }
        let ordinary = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE;
        let huge = ordinary | libc::MAP_HUGETLB | MAP_HUGE_2MB;
        let mut pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                huge,
                -1,
                0,
            )
        };
        let mut hugetlb = pointer != libc::MAP_FAILED;
        if pointer == libc::MAP_FAILED {
            if require_hugetlb {
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    format!(
                        "direct-OFI HugeTLB allocation of {len} bytes failed: {}",
                        io::Error::last_os_error()
                    ),
                ));
            }
            pointer = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    ordinary,
                    -1,
                    0,
                )
            };
            hugetlb = false;
        }
        if pointer == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        if !hugetlb {
            unsafe {
                libc::madvise(pointer, len, libc::MADV_HUGEPAGE);
            }
        }
        Ok(Self {
            pointer: NonNull::new(pointer.cast()).unwrap(),
            len,
            hugetlb,
        })
    }

    fn as_mut_ptr(&self) -> *mut u8 {
        self.pointer.as_ptr()
    }
    fn len(&self) -> usize {
        self.len
    }
    fn hugetlb(&self) -> bool {
        self.hugetlb
    }

    unsafe fn slice(&self, offset: usize, len: usize) -> io::Result<&[u8]> {
        let end = offset.checked_add(len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "registered slice overflow")
        })?;
        if end > self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "registered slice out of bounds",
            ));
        }
        Ok(unsafe { slice::from_raw_parts(self.pointer.as_ptr().add(offset), len) })
    }

    unsafe fn slice_mut(&self, offset: usize, len: usize) -> io::Result<&mut [u8]> {
        let end = offset.checked_add(len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "registered slice overflow")
        })?;
        if end > self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "registered slice out of bounds",
            ));
        }
        Ok(unsafe { slice::from_raw_parts_mut(self.pointer.as_ptr().add(offset), len) })
    }
}

impl Drop for RegisteredRegion {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.pointer.as_ptr().cast(), self.len);
        }
    }
}

struct ClientLane {
    endpoint: ZcOfiEndpoint,
    meta: VolumeMeta,
    depth: usize,
    read_slots: Vec<usize>,
    read_tokens: Vec<u64>,
    write_slots: Vec<usize>,
    write_tokens: Vec<u64>,
    write_sequence_by_slot: Vec<u64>,
    ready: VecDeque<VhostOfiCompletion>,
    completed_write_gaps: BTreeSet<u64>,
    admitted_writes: u64,
    completed_write_hwm: u64,
    outstanding_reads: usize,
    outstanding_writes: usize,
    barrier_sequence: u64,
    control_tx: Box<[u8; CONTROL_BYTES]>,
    control_rx: Box<[u8; CONTROL_BYTES]>,
    shutdown: bool,
}

impl ClientLane {
    fn record_write_completion(&mut self, slot: usize) -> io::Result<()> {
        let sequence = *self.write_sequence_by_slot.get(slot).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "direct-OFI write completion slot overflow",
            )
        })?;
        if sequence == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "direct-OFI write completion has no sequence",
            ));
        }
        self.write_sequence_by_slot[slot] = 0;
        if sequence == self.completed_write_hwm + 1 {
            self.completed_write_hwm = sequence;
            while self
                .completed_write_gaps
                .remove(&(self.completed_write_hwm + 1))
            {
                self.completed_write_hwm += 1;
            }
        } else if sequence > self.completed_write_hwm + 1 {
            self.completed_write_gaps.insert(sequence);
        }
        Ok(())
    }

    fn poll(&mut self, wait: bool) -> io::Result<usize> {
        let before = self.ready.len();
        if self.outstanding_reads != 0 {
            let count =
                self.endpoint
                    .rma_read_poll(&mut self.read_slots, &mut self.read_tokens, wait)?;
            for index in 0..count {
                self.ready.push_back(VhostOfiCompletion {
                    slot: self.read_slots[index],
                    user_data: self.read_tokens[index],
                    kind: VhostOfiCompletionKind::Read,
                });
            }
            self.outstanding_reads = self.outstanding_reads.saturating_sub(count);
        }
        if self.outstanding_writes != 0 {
            let wait_for_write = wait && self.ready.len() == before && self.outstanding_reads == 0;
            let count = self.endpoint.rma_write_poll(
                &mut self.write_slots,
                &mut self.write_tokens,
                wait_for_write,
            )?;
            for index in 0..count {
                let slot = self.write_slots[index];
                self.record_write_completion(slot)?;
                self.ready.push_back(VhostOfiCompletion {
                    slot,
                    user_data: self.write_tokens[index],
                    kind: VhostOfiCompletionKind::Write,
                });
            }
            self.outstanding_writes = self.outstanding_writes.saturating_sub(count);
        }
        Ok(self.ready.len() - before)
    }

    fn barrier(&mut self, target_hwm: u64) -> io::Result<()> {
        while self.completed_write_hwm < target_hwm {
            if self.outstanding_writes == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "direct-OFI write HWM cannot advance",
                ));
            }
            self.poll(true)?;
        }
        self.barrier_sequence = self.barrier_sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "direct-OFI barrier sequence overflow")
        })?;
        let frame = ControlFrame {
            operation: CONTROL_BARRIER,
            lane: self.meta.lane,
            sequence: self.barrier_sequence,
        };
        *self.control_tx = frame.encode();
        self.endpoint.send(self.control_tx.as_slice())?;
        let got = self.endpoint.recv(self.control_rx.as_mut_slice())?;
        let ack = ControlFrame::decode(&self.control_rx[..got])?;
        if ack.operation != CONTROL_BARRIER
            || ack.lane != self.meta.lane
            || ack.sequence != self.barrier_sequence
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "direct-OFI barrier acknowledgement mismatch",
            ));
        }
        Ok(())
    }

    fn shutdown(&mut self) -> io::Result<()> {
        if self.shutdown {
            return Ok(());
        }
        self.barrier_sequence = self.barrier_sequence.saturating_add(1);
        let frame = ControlFrame {
            operation: CONTROL_SHUTDOWN,
            lane: self.meta.lane,
            sequence: self.barrier_sequence,
        };
        *self.control_tx = frame.encode();
        self.endpoint.send(self.control_tx.as_slice())?;
        let got = self.endpoint.recv(self.control_rx.as_mut_slice())?;
        let ack = ControlFrame::decode(&self.control_rx[..got])?;
        if ack.operation != CONTROL_SHUTDOWN || ack.lane != self.meta.lane {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "direct-OFI shutdown acknowledgement mismatch",
            ));
        }
        self.shutdown = true;
        Ok(())
    }
}

pub struct VhostOfiClient {
    config: VhostOfiClientConfig,
    lanes: Mutex<Vec<Option<Weak<Mutex<ClientLane>>>>>,
}

impl VhostOfiClient {
    pub fn new(config: VhostOfiClientConfig) -> io::Result<Self> {
        validate_shape(config.base_service, config.lanes, config.capacity_bytes)?;
        Ok(Self {
            lanes: Mutex::new((0..config.lanes).map(|_| None).collect()),
            config,
        })
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.config.capacity_bytes
    }
    pub fn lanes(&self) -> usize {
        self.config.lanes
    }

    pub fn connect_queue(
        &self,
        queue: usize,
        depth: usize,
        slot_bytes: usize,
    ) -> io::Result<VhostOfiQueue> {
        if queue >= self.config.lanes
            || depth == 0
            || slot_bytes < 4096
            || !slot_bytes.is_power_of_two()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid direct-OFI queue shape",
            ));
        }
        let arena_bytes = depth.checked_mul(slot_bytes).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "direct-OFI arena size overflow",
            )
        })?;
        let arena = RegisteredRegion::new(arena_bytes, self.config.require_hugetlb)?;
        let service = service_for(self.config.base_service, queue)?.to_string();
        let mut endpoint = ZcOfiEndpoint::open_rma_sized_on_domain(
            &self.config.provider,
            &self.config.endpoint,
            &self.config.address,
            &service,
            false,
            self.config.domain.as_deref(),
            depth,
            depth,
        )?;
        let contract = contract(queue, self.config.lanes, self.config.capacity_bytes);
        zcofi_client_exchange_peer(
            &self.config.address,
            zcofi_control_port(&service)?,
            &mut endpoint,
            &contract,
        )?;
        let mut control_rx = Box::new([0u8; CONTROL_BYTES]);
        let control_tx = Box::new([0u8; CONTROL_BYTES]);
        endpoint.register_recv_buffer(control_rx.as_mut_slice())?;
        endpoint.register_send_buffer(control_tx.as_slice())?;
        let got = endpoint.recv(control_rx.as_mut_slice())?;
        let meta = VolumeMeta::decode(&control_rx[..got])?;
        if meta.lane as usize != queue
            || meta.lanes as usize != self.config.lanes
            || meta.capacity_bytes != self.config.capacity_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "direct-OFI target metadata does not match configured topology",
            ));
        }
        unsafe {
            endpoint.rma_register_read_buffer_raw(arena.as_mut_ptr(), arena.len())?;
            endpoint.rma_register_write_buffer_raw(arena.as_mut_ptr(), arena.len())?;
        }
        endpoint.rma_read_queue_init(depth)?;
        endpoint.rma_write_queue_init(depth)?;
        let lane = Arc::new(Mutex::new(ClientLane {
            endpoint,
            meta,
            depth,
            read_slots: vec![0; depth],
            read_tokens: vec![0; depth],
            write_slots: vec![0; depth],
            write_tokens: vec![0; depth],
            write_sequence_by_slot: vec![0; depth],
            ready: VecDeque::with_capacity(depth),
            completed_write_gaps: BTreeSet::new(),
            admitted_writes: 0,
            completed_write_hwm: 0,
            outstanding_reads: 0,
            outstanding_writes: 0,
            barrier_sequence: 0,
            control_tx,
            control_rx,
            shutdown: false,
        }));
        self.lanes
            .lock()
            .map_err(|_| io::Error::other("direct-OFI lane registry poisoned"))?[queue] =
            Some(Arc::downgrade(&lane));
        eprintln!(
            "zcvhost-direct-ofi-lane: lane={queue} service={service} provider={} endpoint={} domain={} depth={depth} slot_bytes={slot_bytes} registered_bytes={} hugetlb={} data_path_syscalls={} kernel_block_edge=no placement_owner=downstream-userspace-volume frontend_placement=no",
            self.config.provider,
            self.config.endpoint,
            self.config.domain.as_deref().unwrap_or("auto"),
            arena.len(),
            arena.hugetlb(),
            provider_data_path_syscalls(&self.config.provider),
        );
        Ok(VhostOfiQueue {
            lane,
            arena,
            slot_bytes,
            depth,
            free: (0..depth).rev().collect(),
        })
    }

    pub fn flush(&self) -> io::Result<()> {
        let lanes = self.live_lanes()?;
        let mut targets = Vec::with_capacity(lanes.len());
        for lane in &lanes {
            targets.push(
                lane.lock()
                    .map_err(|_| io::Error::other("direct-OFI lane poisoned"))?
                    .admitted_writes,
            );
        }
        for (lane, target) in lanes.iter().zip(targets) {
            lane.lock()
                .map_err(|_| io::Error::other("direct-OFI lane poisoned"))?
                .barrier(target)?;
        }
        Ok(())
    }

    pub fn shutdown(&self) -> io::Result<()> {
        for lane in self.live_lanes()? {
            lane.lock()
                .map_err(|_| io::Error::other("direct-OFI lane poisoned"))?
                .shutdown()?;
        }
        Ok(())
    }

    fn live_lanes(&self) -> io::Result<Vec<Arc<Mutex<ClientLane>>>> {
        let registry = self
            .lanes
            .lock()
            .map_err(|_| io::Error::other("direct-OFI lane registry poisoned"))?;
        if registry.iter().any(Option::is_none) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "not all direct-OFI lanes are connected",
            ));
        }
        registry
            .iter()
            .map(|lane| {
                lane.as_ref().unwrap().upgrade().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "direct-OFI lane closed")
                })
            })
            .collect()
    }
}

pub struct VhostOfiQueue {
    // Drop the endpoint/MR before unmapping its registered arena.
    lane: Arc<Mutex<ClientLane>>,
    arena: RegisteredRegion,
    slot_bytes: usize,
    depth: usize,
    free: Vec<usize>,
}

unsafe impl Send for VhostOfiQueue {}

impl VhostOfiQueue {
    pub fn slot_bytes(&self) -> usize {
        self.slot_bytes
    }
    pub fn depth(&self) -> usize {
        self.depth
    }
    pub fn has_capacity(&self) -> bool {
        !self.free.is_empty()
    }
    pub fn allocate(&mut self) -> Option<usize> {
        self.free.pop()
    }
    pub fn release(&mut self, slot: usize) -> io::Result<()> {
        if slot >= self.depth || self.free.contains(&slot) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid direct-OFI slot release",
            ));
        }
        self.free.push(slot);
        Ok(())
    }
    pub fn slot(&self, slot: usize, len: usize) -> io::Result<&[u8]> {
        if len > self.slot_bytes || slot >= self.depth {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "direct-OFI slot range invalid",
            ));
        }
        unsafe { self.arena.slice(slot * self.slot_bytes, len) }
    }
    pub fn slot_mut(&mut self, slot: usize, len: usize) -> io::Result<&mut [u8]> {
        if len > self.slot_bytes || slot >= self.depth {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "direct-OFI slot range invalid",
            ));
        }
        unsafe { self.arena.slice_mut(slot * self.slot_bytes, len) }
    }
    pub fn post_read(
        &mut self,
        slot: usize,
        offset: u64,
        len: usize,
        user_data: u64,
    ) -> io::Result<bool> {
        self.checked_remote(offset, len)?;
        let pointer = unsafe { self.arena.as_mut_ptr().add(slot * self.slot_bytes) };
        let mut lane = self
            .lane
            .lock()
            .map_err(|_| io::Error::other("direct-OFI lane poisoned"))?;
        let meta = lane.meta;
        let posted = unsafe {
            lane.endpoint.rma_read_post_raw(
                pointer,
                len,
                meta.remote_addr + offset,
                meta.remote_key,
                slot,
                user_data,
                true,
            )?
        };
        if posted {
            lane.outstanding_reads += 1;
        }
        Ok(posted)
    }
    pub fn post_write(
        &mut self,
        slot: usize,
        offset: u64,
        len: usize,
        user_data: u64,
        more: bool,
    ) -> io::Result<bool> {
        self.checked_remote(offset, len)?;
        let pointer = unsafe { self.arena.as_mut_ptr().add(slot * self.slot_bytes) };
        let mut lane = self
            .lane
            .lock()
            .map_err(|_| io::Error::other("direct-OFI lane poisoned"))?;
        let meta = lane.meta;
        let posted = unsafe {
            lane.endpoint.rma_write_post_more_raw(
                pointer,
                len,
                meta.remote_addr + offset,
                meta.remote_key,
                slot,
                user_data,
                more,
            )?
        };
        if posted {
            lane.admitted_writes = lane
                .admitted_writes
                .checked_add(1)
                .ok_or_else(|| io::Error::other("direct-OFI write sequence overflow"))?;
            let sequence = lane.admitted_writes;
            lane.write_sequence_by_slot[slot] = sequence;
            lane.outstanding_writes += 1;
        }
        Ok(posted)
    }
    pub fn poll(&mut self, output: &mut Vec<VhostOfiCompletion>, wait: bool) -> io::Result<usize> {
        let mut lane = self
            .lane
            .lock()
            .map_err(|_| io::Error::other("direct-OFI lane poisoned"))?;
        if lane.ready.is_empty() {
            lane.poll(wait)?;
        }
        let before = output.len();
        output.extend(lane.ready.drain(..));
        Ok(output.len() - before)
    }
    fn checked_remote(&self, offset: u64, len: usize) -> io::Result<()> {
        if len == 0
            || len > self.slot_bytes
            || offset.checked_add(len as u64).is_none_or(|end| {
                end > self
                    .lane
                    .lock()
                    .map(|lane| lane.meta.capacity_bytes)
                    .unwrap_or(0)
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "direct-OFI request range invalid",
            ));
        }
        Ok(())
    }
}

pub fn run_vhost_ofi_target(config: VhostOfiTargetConfig) -> io::Result<()> {
    validate_shape(config.base_service, config.lanes, config.capacity_bytes)?;
    if !config.lane_cpus.is_empty() && config.lane_cpus.len() != config.lanes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target lane CPU list must match lane count",
        ));
    }
    let region = Arc::new(RegisteredRegion::new(
        usize::try_from(config.capacity_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "capacity exceeds usize"))?,
        config.require_hugetlb,
    )?);
    let stopped = Arc::new(AtomicBool::new(false));
    eprintln!(
        "zcvhost-ofi-volume: bind={} provider={} endpoint={} domain={} lanes={} capacity_bytes={} hugetlb={} block_device=no data_path_syscalls={} placement_owner=userspace-volume terminal_media=registered-memory durability=volatile",
        config.bind,
        config.provider,
        config.endpoint,
        config.domain.as_deref().unwrap_or("auto"),
        config.lanes,
        config.capacity_bytes,
        region.hugetlb(),
        provider_data_path_syscalls(&config.provider),
    );
    let mut workers = Vec::with_capacity(config.lanes);
    for lane in 0..config.lanes {
        let config = config.clone();
        let region = Arc::clone(&region);
        let stopped = Arc::clone(&stopped);
        workers.push(thread::spawn(move || {
            target_lane(config, lane, region, stopped)
        }));
    }
    for worker in workers {
        worker
            .join()
            .map_err(|_| io::Error::other("direct-OFI target lane panicked"))??;
    }
    Ok(())
}

fn target_lane(
    config: VhostOfiTargetConfig,
    lane: usize,
    region: Arc<RegisteredRegion>,
    stopped: Arc<AtomicBool>,
) -> io::Result<()> {
    if let Some(cpu) = config.lane_cpus.get(lane) {
        let _ = maybe_pin_current_thread("zcvhost-ofi-volume", *cpu);
    }
    let service = service_for(config.base_service, lane)?.to_string();
    let mut endpoint = ZcOfiEndpoint::open_rma_on_domain(
        &config.provider,
        &config.endpoint,
        &config.bind,
        &service,
        true,
        config.domain.as_deref(),
    )?;
    let contract = contract(lane, config.lanes, config.capacity_bytes);
    zcofi_server_exchange_peer(
        &config.bind,
        zcofi_control_port(&service)?,
        &mut endpoint,
        &contract,
    )?;
    let (remote_addr, remote_key) =
        unsafe { endpoint.rma_register_target_raw(region.as_mut_ptr(), region.len())? };
    let mut tx = Box::new([0u8; CONTROL_BYTES]);
    let mut rx = Box::new([0u8; CONTROL_BYTES]);
    endpoint.register_send_buffer(tx.as_slice())?;
    endpoint.register_recv_buffer(rx.as_mut_slice())?;
    let meta = VolumeMeta {
        lane: lane as u32,
        lanes: config.lanes as u32,
        capacity_bytes: config.capacity_bytes,
        remote_addr,
        remote_key,
    };
    tx.copy_from_slice(&meta.encode());
    endpoint.send(tx.as_slice())?;
    let mut last_barrier = 0u64;
    while !stopped.load(Ordering::Acquire) {
        let got = endpoint.recv(rx.as_mut_slice())?;
        let frame = ControlFrame::decode(&rx[..got])?;
        if frame.lane as usize != lane || frame.sequence <= last_barrier {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "direct-OFI control sequence mismatch",
            ));
        }
        match frame.operation {
            CONTROL_BARRIER => {
                // RMA writes request FI_DELIVERY_COMPLETE. Receiving the
                // subsequent control record, followed by this CPU fence,
                // establishes the remote userspace HWM acknowledged here.
                fence(Ordering::SeqCst);
                last_barrier = frame.sequence;
                tx.copy_from_slice(&frame.encode());
                endpoint.send(tx.as_slice())?;
            }
            CONTROL_SHUTDOWN => {
                last_barrier = frame.sequence;
                tx.copy_from_slice(&frame.encode());
                endpoint.send(tx.as_slice())?;
                break;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown direct-OFI control operation",
                ));
            }
        }
    }
    eprintln!(
        "zcvhost-ofi-volume-lane-summary: lane={lane} last_barrier={last_barrier} status=closed"
    );
    Ok(())
}

fn service_for(base: u16, lane: usize) -> io::Result<u16> {
    usize::from(base)
        .checked_add(lane)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "direct-OFI service range overflow",
            )
        })
}

fn validate_shape(base: u16, lanes: usize, capacity: u64) -> io::Result<()> {
    if lanes == 0 || lanes > 64 || capacity == 0 || capacity % 4096 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "direct-OFI requires 1..=64 lanes and 4K-aligned positive capacity",
        ));
    }
    let last = service_for(base, lanes - 1)?;
    last.checked_add(1000).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "direct-OFI control service range overflow",
        )
    })?;
    Ok(())
}

fn contract(lane: usize, lanes: usize, capacity: u64) -> String {
    format!(
        "zcvhost-direct-rma-v1;lane={lane};lanes={lanes};capacity={capacity};placement=downstream-userspace-volume"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn metadata_and_control_round_trip() {
        let meta = VolumeMeta {
            lane: 3,
            lanes: 8,
            capacity_bytes: 1 << 30,
            remote_addr: 0x1234,
            remote_key: 0x5678,
        };
        let decoded = VolumeMeta::decode(&meta.encode()).unwrap();
        assert_eq!(decoded.lane, meta.lane);
        assert_eq!(decoded.remote_key, meta.remote_key);
        let control = ControlFrame {
            operation: CONTROL_BARRIER,
            lane: 2,
            sequence: 91,
        };
        let decoded = ControlFrame::decode(&control.encode()).unwrap();
        assert_eq!(decoded.operation, CONTROL_BARRIER);
        assert_eq!(decoded.sequence, 91);
    }

    #[test]
    fn service_range_rejects_control_overflow() {
        assert!(validate_shape(65_000, 1, 4096).is_err());
        assert!(validate_shape(40_000, 64, 4096).is_ok());
    }

    #[test]
    fn syscall_claim_is_conservative() {
        assert_eq!(provider_data_path_syscalls("efa"), "0");
        assert_eq!(provider_data_path_syscalls("verbs"), "0");
        assert_eq!(provider_data_path_syscalls("verbs;ofi_rxm"), "0");
        assert_eq!(provider_data_path_syscalls("sockets"), "provider-dependent");
    }

    #[cfg(zc_has_libfabric)]
    #[test]
    fn sockets_rma_round_trip_and_barrier() {
        let base_service = 42_000 + (std::process::id() % 500) as u16;
        let capacity_bytes = 1024 * 1024;
        let target = thread::spawn(move || {
            run_vhost_ofi_target(VhostOfiTargetConfig {
                provider: "sockets".to_string(),
                endpoint: "rdm".to_string(),
                bind: "127.0.0.1".to_string(),
                domain: None,
                base_service,
                lanes: 1,
                capacity_bytes,
                lane_cpus: Vec::new(),
                require_hugetlb: false,
            })
        });
        thread::sleep(Duration::from_millis(20));
        let client = VhostOfiClient::new(VhostOfiClientConfig {
            provider: "sockets".to_string(),
            endpoint: "rdm".to_string(),
            address: "127.0.0.1".to_string(),
            domain: None,
            base_service,
            lanes: 1,
            capacity_bytes,
            require_hugetlb: false,
        })
        .unwrap();
        let mut queue = client.connect_queue(0, 8, 4096).unwrap();
        let write_slot = queue.allocate().unwrap();
        queue.slot_mut(write_slot, 4096).unwrap().fill(0x5a);
        assert!(queue.post_write(write_slot, 8192, 4096, 7, false).unwrap());
        let mut completions = Vec::new();
        assert_eq!(queue.poll(&mut completions, true).unwrap(), 1);
        assert_eq!(completions[0].kind, VhostOfiCompletionKind::Write);
        queue.release(write_slot).unwrap();
        client.flush().unwrap();

        completions.clear();
        let read_slot = queue.allocate().unwrap();
        queue.slot_mut(read_slot, 4096).unwrap().fill(0);
        assert!(queue.post_read(read_slot, 8192, 4096, 9).unwrap());
        assert_eq!(queue.poll(&mut completions, true).unwrap(), 1);
        assert_eq!(completions[0].kind, VhostOfiCompletionKind::Read);
        assert!(
            queue
                .slot(read_slot, 4096)
                .unwrap()
                .iter()
                .all(|byte| *byte == 0x5a)
        );
        queue.release(read_slot).unwrap();
        client.shutdown().unwrap();
        drop(queue);
        target.join().unwrap().unwrap();
    }
}
