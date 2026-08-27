//! A placement-free vhost-user-blk edge for stock QEMU.
//!
//! This process translates virtio-blk requests into a `BlockStage`.  It must
//! never choose mirrors, stripes, tiers, lanes, or spill destinations.  Those
//! decisions belong to a separate userspace RAID stage.  `FileLeaf` is kept as
//! a terminal-media implementation for protocol tests and local QEMU smoke
//! tests; a production topology supplies a userspace volume stage instead.

use std::env;
use std::fs::{File, OpenOptions};
use std::io;
use std::ops::Deref;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering, fence};
use std::sync::{Arc, Mutex, RwLock, RwLockWriteGuard};
use std::time::{Duration, Instant};

use vhost::vhost_user::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};
use vhost_user_backend::{VhostUserBackend, VhostUserDaemon, VringRwLock, VringState, VringT};
use virtio_bindings::bindings::virtio_blk::{
    VIRTIO_BLK_F_BLK_SIZE, VIRTIO_BLK_F_CONFIG_WCE, VIRTIO_BLK_F_FLUSH, VIRTIO_BLK_F_MQ,
    VIRTIO_BLK_F_RO, VIRTIO_BLK_F_SEG_MAX, VIRTIO_BLK_S_IOERR, VIRTIO_BLK_S_OK,
    VIRTIO_BLK_S_UNSUPP, VIRTIO_BLK_T_FLUSH, VIRTIO_BLK_T_GET_ID, VIRTIO_BLK_T_IN,
    VIRTIO_BLK_T_OUT,
};
use virtio_bindings::bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_bindings::bindings::virtio_ring::{
    VIRTIO_RING_F_EVENT_IDX, VRING_DESC_F_INDIRECT, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE,
};
use virtio_queue::{DescriptorChain, QueueT};
use vm_memory::{
    Bytes, GuestAddress, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryBackend,
    GuestMemoryLoadGuard, GuestMemoryRegion, MemoryRegionAddress, ReadVolatile, VolatileSlice,
    WriteVolatile,
};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::event::{EventConsumer, EventNotifier};
use vmm_sys_util::eventfd::EventFd;
use zcutils::vhost_ofi::{
    VhostOfiClient, VhostOfiClientConfig, VhostOfiCompletion, VhostOfiCompletionKind, VhostOfiQueue,
};
use zcutils::zcnblk_app_arena::{
    ZcnblkAppArena, ZcnblkAppArenaBuffer, ZcnblkAppArenaIoCompletion, ZcnblkAppArenaIoRing,
    pin_current_thread,
};

// This backend does not advertise VHOST_USER_PROTOCOL_F_LOG_SHMFD, so using
// BitmapMmapRegion would only take its shared dirty-bitmap lock for every
// guest-memory write without providing migration logging.  The unit bitmap is
// the protocol-correct no-logging fast path.  If live-migration logging is
// added later it must be negotiated explicitly and kept out of normal I/O.
type GuestMemoryMmap = vm_memory::GuestMemoryMmap<()>;
type AtomicGuestMemory = GuestMemoryAtomic<GuestMemoryMmap>;
type LoadedDescriptorChain = DescriptorChain<GuestMemoryLoadGuard<GuestMemoryMmap>>;

const SECTOR_SIZE: u64 = 512;
const BLOCK_SIZE: u32 = 512;
const CONFIG_LEN: usize = 60;
const CONFIG_WCE_OFFSET: usize = 32;
const SERIAL_LEN: usize = 20;
const MAX_QUEUES: usize = 64;
const MAX_DATA_DESCRIPTORS: usize = 1024;
// vhost-user-backend reserves event ids through num_queues (inclusive). Keep
// completion events above the largest queue count so every lane can register
// the same stable id before QEMU negotiates its active queue count.
const ARENA_CQ_EVENT_BASE: u16 = MAX_QUEUES as u16 + 1;
// A vDSO clock read on every empty virtqueue probe dominated bounded polling
// on high-IOPS runs.  The poll budget is a liveness/latency bound rather than
// an I/O accounting clock, so sample it once per power-of-two probe group.
const POLL_DEADLINE_SAMPLE_MASK: u32 = 255;
const ARENA_WAIT_BATCH_MIN_OUTSTANDING: usize = 32;
const ARENA_WAIT_BATCH_COMPLETIONS: usize = 16;
const VIRTIO_BLK_T_BARRIER: u32 = 0x8000_0000;
const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;

/// The only contract the virtio adapter is allowed to use.
///
/// Placement and durability policy are properties of the supplied stage, not
/// of the vhost adapter.  A later adapter can therefore wrap an existing
/// userspace mirrored-volume stage without moving policy into QEMU or virtio.
trait BlockStage: Send + Sync {
    fn capacity_bytes(&self) -> u64;
    fn read_at(&self, queue: usize, offset: u64, destination: &mut [u8]) -> io::Result<()>;
    fn write_at(&self, queue: usize, offset: u64, source: &[u8]) -> io::Result<()>;
    fn read_at_guest(
        &self,
        queue: usize,
        offset: u64,
        destination: &mut VolatileSlice<'_, ()>,
    ) -> io::Result<()> {
        let mut scratch = vec![0u8; destination.len()];
        self.read_at(queue, offset, &mut scratch)?;
        (&scratch[..])
            .read_exact_volatile(destination)
            .map_err(io::Error::other)
    }
    fn write_at_guest(
        &self,
        queue: usize,
        offset: u64,
        source: &VolatileSlice<'_, ()>,
    ) -> io::Result<()> {
        let mut scratch = vec![0u8; source.len()];
        (&mut scratch[..])
            .write_all_volatile(source)
            .map_err(io::Error::other)?;
        self.write_at(queue, offset, &scratch)
    }
    fn flush(&self) -> io::Result<()>;

    fn shutdown(&self) -> io::Result<()> {
        Ok(())
    }

    fn open_arena_queue(&self, _queue: usize, _depth: u32) -> io::Result<Option<ArenaQueue>> {
        Ok(None)
    }

    fn open_direct_queue(&self, _queue: usize, _depth: u32) -> io::Result<Option<DirectDataQueue>> {
        Ok(None)
    }
}

/// Terminal leaf media used by the local integration test.
struct FileLeaf {
    file: File,
    capacity: u64,
    read_only: bool,
}

impl FileLeaf {
    fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let capacity = file.metadata()?.len();
        if capacity == 0 || capacity % SECTOR_SIZE != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "terminal leaf size must be a non-zero multiple of {SECTOR_SIZE}, got {capacity}"
                ),
            ));
        }
        Ok(Self {
            file,
            capacity,
            read_only,
        })
    }

    fn checked_end(&self, offset: u64, len: usize) -> io::Result<u64> {
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "I/O range overflow"))?;
        if end > self.capacity {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "I/O range {offset}..{end} exceeds leaf capacity {}",
                    self.capacity
                ),
            ));
        }
        Ok(end)
    }
}

impl BlockStage for FileLeaf {
    fn capacity_bytes(&self) -> u64 {
        self.capacity
    }

    fn read_at(&self, _queue: usize, offset: u64, destination: &mut [u8]) -> io::Result<()> {
        self.checked_end(offset, destination.len())?;
        self.file.read_exact_at(destination, offset)
    }

    fn write_at(&self, _queue: usize, offset: u64, source: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "terminal leaf is read-only",
            ));
        }
        self.checked_end(offset, source.len())?;
        self.file.write_all_at(source, offset)
    }

    fn read_at_guest(
        &self,
        _queue: usize,
        offset: u64,
        destination: &mut VolatileSlice<'_, ()>,
    ) -> io::Result<()> {
        self.checked_end(offset, destination.len())?;
        let guard = destination.ptr_guard_mut();
        let mut completed = 0usize;
        while completed < destination.len() {
            // SAFETY: the volatile pointer guard covers destination.len()
            // writable bytes, and checked_end bounded the file offset.
            let result = unsafe {
                libc::pread(
                    self.file.as_raw_fd(),
                    guard.as_ptr().add(completed).cast(),
                    destination.len() - completed,
                    (offset + completed as u64) as libc::off_t,
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            if result == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            completed += result as usize;
        }
        Ok(())
    }

    fn write_at_guest(
        &self,
        _queue: usize,
        offset: u64,
        source: &VolatileSlice<'_, ()>,
    ) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "terminal leaf is read-only",
            ));
        }
        self.checked_end(offset, source.len())?;
        let guard = source.ptr_guard();
        let mut completed = 0usize;
        while completed < source.len() {
            // SAFETY: the volatile pointer guard covers source.len() readable
            // bytes, and checked_end bounded the file offset.
            let result = unsafe {
                libc::pwrite(
                    self.file.as_raw_fd(),
                    guard.as_ptr().add(completed).cast(),
                    source.len() - completed,
                    (offset + completed as u64) as libc::off_t,
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            if result == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            completed += result as usize;
        }
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}

/// Shared-arena adapter for an existing zcnblk userspace stage.
///
/// Each virtqueue maps deterministically to one existing lane. The only
/// payload copy is between QEMU guest RAM and the HugeTLB slot; ownership of
/// that slot is then handed through `/dev/zcnblk0` without a second copy.
/// This adapter does not inspect or choose downstream placement.
struct ArenaStage {
    arena: ZcnblkAppArena,
    device: File,
    capacity: u64,
    read_only: bool,
    // Arena exhaustion is exceptional. Coordinate the resulting downstream
    // barrier here so lane workers do not independently issue overlapping
    // global syncs. This mutex is never acquired by the ordinary I/O path.
    pressure_barrier: Arc<Mutex<()>>,
}

impl ArenaStage {
    fn open(socket: &Path, device: &Path, read_only: bool) -> io::Result<Self> {
        let arena = ZcnblkAppArena::connect(socket)?;
        let device = OpenOptions::new()
            .read(true)
            .write(!read_only)
            // The block edge is not a meaningful access-time source. Avoid
            // dirtying its inode metadata on every otherwise read-only run.
            .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC | libc::O_NOATIME)
            .open(device)?;
        let mut capacity = 0u64;
        if unsafe { libc::ioctl(device.as_raw_fd(), BLKGETSIZE64, &mut capacity) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let slot_bytes = arena.slot_bytes();
        if slot_bytes == 0 || !slot_bytes.is_power_of_two() || capacity < slot_bytes as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid application-arena slot size or zcnblk capacity",
            ));
        }
        Ok(Self {
            arena,
            device,
            capacity,
            read_only,
            pressure_barrier: Arc::new(Mutex::new(())),
        })
    }

    fn checked_end(&self, offset: u64, len: usize) -> io::Result<u64> {
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "I/O range overflow"))?;
        if end > self.capacity {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "I/O range exceeds zcnblk volume capacity",
            ));
        }
        Ok(end)
    }

    fn lane(&self, queue: usize) -> u32 {
        queue as u32 % self.arena.channels()
    }
}

struct ArenaPending {
    // Retain the exact RCU guest-memory snapshot until the asynchronous
    // completion is published. The generic fallback keeps its descriptor
    // chain; direct split-ring admission keeps the guard and head index only.
    chain: ArenaChain,
    data: DescriptorMeta,
    // Resolve the guest data mapping once at admission. The descriptor
    // chain's RCU memory guard above keeps this mapping alive until the CQE
    // is retired, even if QEMU replaces its vhost memory table meanwhile.
    // Store the address as an integer so this lane-local request remains
    // movable without claiming that an unbounded raw pointer is Send.
    data_host_address: usize,
    status_host_address: usize,
    buffer: ZcnblkAppArenaBuffer,
    kind: RequestKind,
}

#[derive(Clone, Copy, Default)]
struct ArenaCompletionStats {
    submit_calls: u64,
    submitted_requests: u64,
    submit_batch_max: u64,
    outstanding_max: u64,
    wait_calls: u64,
    wait_cqes: u64,
    wait_batch_max: u64,
    wait_cq_head_publications: u64,
    ready_batch_publications: u64,
    ready_batched_cqes: u64,
    ready_batch_max: u64,
    direct_used_entries: u64,
    direct_avail_entries: u64,
    completion_event_wakes: u64,
    completion_event_cqes: u64,
    completion_event_arms: u64,
    completion_event_disarms: u64,
    completion_event_task_runs: u64,
    forced_idle_notifications: u64,
}

enum ArenaChain {
    Loaded(LoadedDescriptorChain),
    // Direct admissions retain no per-request memory guard. QueueWorker keeps
    // the complete mapping generation alive and drains all arena requests
    // before replacing it, avoiding one shared reference-count pair per I/O.
    Fast { head_index: u16 },
}

impl ArenaChain {
    fn head_index(&self) -> u16 {
        match self {
            Self::Loaded(chain) => chain.head_index(),
            Self::Fast { head_index, .. } => *head_index,
        }
    }
}

#[derive(Clone, Copy)]
struct ArenaDataPlan {
    data: DescriptorMeta,
    data_host_address: usize,
    status_host_address: usize,
    offset: u64,
    kind: RequestKind,
}

struct ArenaPrepared {
    chain: ArenaChain,
    data: DescriptorMeta,
    data_host_address: usize,
    status_host_address: usize,
    offset: u64,
    kind: RequestKind,
}

enum ArenaPrepareDecision {
    Prepared(ArenaPrepared),
    FallbackLoaded(LoadedDescriptorChain),
    Invalid(LoadedDescriptorChain),
}

enum FastArenaPrepareDecision {
    Empty,
    Prepared(ArenaPrepared),
    Fallback,
}

enum ArenaQueueDecision {
    Queued,
    Pressure(ArenaPrepared),
}

struct ArenaQueue {
    arena: ZcnblkAppArena,
    device: File,
    pressure_barrier: Arc<Mutex<()>>,
    lane: u32,
    slot_bytes: usize,
    read_only: bool,
    // Created lazily by the pinned vhost queue worker. RawRing enables
    // IORING_SETUP_SINGLE_ISSUER, so constructing it on the backend setup
    // thread and later submitting here violates the kernel's issuer contract.
    ring: Option<ZcnblkAppArenaIoRing>,
    completion_event: EventFd,
    completion_event_armed: bool,
    depth: u32,
    pending: Vec<Option<ArenaPending>>,
    free: Vec<usize>,
    // Reacquired buffers remain reserved by this lane and are reused without
    // another shared owner/free-count RMW pair. A target-retained write is not
    // placed here; its existing orphan/pressure path remains authoritative.
    recycled: Vec<ZcnblkAppArenaBuffer>,
    // Ready CQEs are pulled from the mmap ring in contiguous batches so the
    // CQ head is not written once per I/O. The cache is lane-local and bounded
    // independently of an unusually large advertised virtqueue.
    completions: Vec<ZcnblkAppArenaIoCompletion>,
    completion_next: usize,
    completion_end: usize,
    completion_stats: ArenaCompletionStats,
    queued: bool,
    queued_count: usize,
    outstanding: usize,
    next_slot: u32,
}

impl ArenaQueue {
    fn new(stage: &ArenaStage, lane: u32, requested_depth: u32) -> io::Result<Self> {
        let slots_per_lane = stage.arena.slots_per_lane();
        // A full local SQ is retired before another arena allocation. It is
        // therefore safe to expose every lane slot to io_uring; actual
        // downstream lease retention is handled by the pressure barrier.
        let depth = requested_depth.min(slots_per_lane).max(1);
        Ok(Self {
            arena: stage.arena.clone(),
            device: stage.device.try_clone()?,
            pressure_barrier: Arc::clone(&stage.pressure_barrier),
            lane,
            slot_bytes: stage.arena.slot_bytes(),
            read_only: stage.read_only,
            ring: None,
            completion_event: EventFd::new(libc::EFD_CLOEXEC | libc::EFD_NONBLOCK)?,
            completion_event_armed: false,
            depth,
            pending: (0..depth).map(|_| None).collect(),
            free: (0..depth as usize).rev().collect(),
            recycled: Vec::with_capacity(depth as usize),
            completions: vec![
                ZcnblkAppArenaIoCompletion::default();
                (depth as usize).min(256).max(1)
            ],
            completion_next: 0,
            completion_end: 0,
            completion_stats: ArenaCompletionStats::default(),
            queued: false,
            queued_count: 0,
            outstanding: 0,
            next_slot: 0,
        })
    }

    fn has_capacity(&self) -> bool {
        !self.free.is_empty()
    }

    fn start(&mut self, completion_events: bool) -> io::Result<()> {
        if self.ring.is_none() {
            let ring = if completion_events {
                ZcnblkAppArenaIoRing::new_event_driven(self.depth)?
            } else {
                ZcnblkAppArenaIoRing::new(self.depth)?
            };
            if completion_events {
                ring.register_completion_eventfd(self.completion_event.as_raw_fd())?;
                // Keep eventfd signaling off while the lane is actively
                // draining. The vhost worker arms it only at the idle edge.
                ring.set_completion_eventfd_enabled(false);
            }
            self.ring = Some(ring);
        }
        Ok(())
    }

    fn completion_event_fd(&self) -> i32 {
        self.completion_event.as_raw_fd()
    }

    fn consume_completion_event(&mut self) -> io::Result<()> {
        match self.completion_event.read() {
            Ok(cqes) => {
                self.completion_stats.completion_event_wakes += 1;
                self.completion_stats.completion_event_cqes += cqes;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn set_completion_event_armed(&mut self, armed: bool) {
        if self.completion_event_armed == armed {
            return;
        }
        self.ring().set_completion_eventfd_enabled(armed);
        self.completion_event_armed = armed;
        if armed {
            self.completion_stats.completion_event_arms += 1;
        } else {
            self.completion_stats.completion_event_disarms += 1;
        }
    }

    fn run_task_work(&mut self) -> io::Result<()> {
        self.ring().run_task_work()?;
        self.completion_stats.completion_event_task_runs += 1;
        Ok(())
    }

    fn ring(&mut self) -> &mut ZcnblkAppArenaIoRing {
        self.ring
            .as_mut()
            .expect("arena ring is started by its pinned queue worker")
    }

    fn outstanding(&self) -> usize {
        self.outstanding
    }

    fn record_forced_idle_notification(&mut self) {
        self.completion_stats.forced_idle_notifications += 1;
    }

    fn allocate(&mut self) -> io::Result<ZcnblkAppArenaBuffer> {
        if let Some(buffer) = self.recycled.pop() {
            debug_assert!(buffer.is_application_owned());
            return Ok(buffer);
        }
        let buffer = self.arena.allocate_from(self.lane, self.next_slot)?;
        let slot = buffer.slot();
        // Reuse the SQ-sized hot window while it can satisfy allocations so
        // a large writeback arena does not turn ordinary memory-backed I/O
        // into a lane-wide cache/TLB walk. If retained leases fill that
        // window, allocate_from spills into the remaining arena and the
        // cursor follows the spill until it wraps.
        self.next_slot = if slot < self.depth {
            if slot + 1 == self.depth { 0 } else { slot + 1 }
        } else if slot + 1 == self.arena.slots_per_lane() {
            0
        } else {
            slot + 1
        };
        Ok(buffer)
    }

    fn recycle(&mut self, buffer: ZcnblkAppArenaBuffer) {
        debug_assert!(buffer.is_application_owned());
        self.recycled.push(buffer);
    }

    fn pressure_sync(&self) -> io::Result<()> {
        let _barrier = self
            .pressure_barrier
            .lock()
            .map_err(|_| io::Error::other("zcnblk arena pressure barrier mutex was poisoned"))?;

        // A preceding lane may already have completed the global barrier.
        // Probe after taking the mutex to avoid another unnecessary sync.
        match self.arena.allocate(self.lane) {
            Ok(buffer) => {
                drop(buffer);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => self.device.sync_all(),
            Err(error) => Err(error),
        }
    }

    fn queue(&mut self, offset: u64, mut request: ArenaPending) -> io::Result<()> {
        let slot = self
            .free
            .pop()
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))?;
        let device = &self.device;
        let result = match request.kind {
            RequestKind::Read => self
                .ring
                .as_mut()
                .expect("arena ring is started")
                .queue_read(device, &mut request.buffer, offset, slot as u64),
            RequestKind::Write => self
                .ring
                .as_mut()
                .expect("arena ring is started")
                .queue_write(device, &mut request.buffer, offset, slot as u64),
            RequestKind::Flush | RequestKind::GetId | RequestKind::Unsupported => {
                unreachable!("only data requests use the arena ring")
            }
        };
        if let Err(error) = result {
            self.free.push(slot);
            return Err(io::Error::new(
                error.kind(),
                format!("arena lane {} queue SQE: {error}", self.lane),
            ));
        }
        self.pending[slot] = Some(request);
        self.outstanding += 1;
        self.completion_stats.outstanding_max = self
            .completion_stats
            .outstanding_max
            .max(self.outstanding as u64);
        self.queued = true;
        self.queued_count += 1;
        Ok(())
    }

    fn submit(&mut self) -> io::Result<()> {
        if self.queued {
            let lane = self.lane;
            self.ring().submit().map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("arena lane {lane} submit SQEs: {error}"),
                )
            })?;
            self.completion_stats.submit_calls += 1;
            self.completion_stats.submitted_requests += self.queued_count as u64;
            self.completion_stats.submit_batch_max = self
                .completion_stats
                .submit_batch_max
                .max(self.queued_count as u64);
            self.queued = false;
            self.queued_count = 0;
        }
        Ok(())
    }

    fn take_completion(&mut self, wait: bool) -> io::Result<Option<(ArenaPending, i32)>> {
        let lane = self.lane;
        if self.completion_next == self.completion_end {
            self.completion_next = 0;
            self.completion_end = 0;
            if wait {
                let minimum = if self.outstanding >= ARENA_WAIT_BATCH_MIN_OUTSTANDING {
                    ARENA_WAIT_BATCH_COMPLETIONS
                        .min(self.outstanding)
                        .min(self.completions.len())
                } else {
                    1
                };
                let added = {
                    let (ring, completions) = (
                        self.ring.as_mut().expect("arena ring is started"),
                        &mut self.completions,
                    );
                    ring.wait_completions(minimum, completions)
                }
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("arena lane {lane} wait CQEs: {error}"),
                    )
                })?;
                self.completion_end = added;
                self.completion_stats.wait_calls += 1;
                self.completion_stats.wait_cqes += added as u64;
                self.completion_stats.wait_batch_max =
                    self.completion_stats.wait_batch_max.max(added as u64);
                self.completion_stats.wait_cq_head_publications += if added > 1 { 2 } else { 1 };
            } else {
                let added = {
                    let (ring, completions) = (
                        self.ring.as_mut().expect("arena ring is started"),
                        &mut self.completions,
                    );
                    ring.try_completions(completions)
                };
                self.completion_end = added;
                if added != 0 {
                    self.completion_stats.ready_batch_publications += 1;
                    self.completion_stats.ready_batched_cqes += added as u64;
                    self.completion_stats.ready_batch_max =
                        self.completion_stats.ready_batch_max.max(added as u64);
                }
            }
            if self.completion_end == 0 {
                return Ok(None);
            }
        }
        let ZcnblkAppArenaIoCompletion { user_data, result } =
            self.completions[self.completion_next];
        self.completion_next += 1;
        let slot = usize::try_from(user_data).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "arena completion id exceeds usize",
            )
        })?;
        let request = self
            .pending
            .get_mut(slot)
            .and_then(Option::take)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "unknown arena completion id")
            })?;
        self.free.push(slot);
        self.outstanding = self.outstanding.saturating_sub(1);
        Ok(Some((request, result)))
    }
}

struct DirectPending {
    chain: ArenaChain,
    data: DescriptorMeta,
    data_host_address: usize,
    status_host_address: usize,
    slot: usize,
    kind: RequestKind,
}

#[derive(Clone, Copy, Default)]
struct DirectCompletionStats {
    posts: u64,
    post_eagain: u64,
    cq_polls: u64,
    cq_completions: u64,
    cq_batch_max: u64,
    outstanding_max: u64,
    direct_avail_entries: u64,
    direct_used_entries: u64,
}

struct DirectDataQueue {
    transport: VhostOfiQueue,
    pending: Vec<Option<DirectPending>>,
    completions: Vec<VhostOfiCompletion>,
    completion_next: usize,
    outstanding: usize,
    read_only: bool,
    stats: DirectCompletionStats,
}

impl DirectDataQueue {
    fn new(transport: VhostOfiQueue, read_only: bool) -> Self {
        let depth = transport.depth();
        Self {
            transport,
            pending: (0..depth).map(|_| None).collect(),
            completions: Vec::with_capacity(depth.min(256)),
            completion_next: 0,
            outstanding: 0,
            read_only,
            stats: DirectCompletionStats::default(),
        }
    }

    fn slot_bytes(&self) -> usize {
        self.transport.slot_bytes()
    }

    fn has_capacity(&self) -> bool {
        self.transport.has_capacity()
    }

    fn outstanding(&self) -> usize {
        self.outstanding
    }

    fn queue(&mut self, prepared: ArenaPrepared) -> io::Result<ArenaQueueDecision> {
        let Some(slot) = self.transport.allocate() else {
            return Ok(ArenaQueueDecision::Pressure(prepared));
        };
        if prepared.kind == RequestKind::Write {
            // SAFETY: the cached vhost region spans the complete descriptor
            // and the retained chain/memory generation remains alive until
            // this request is retired.
            let data_slice = unsafe {
                VolatileSlice::new(
                    prepared.data_host_address as *mut u8,
                    prepared.data.length as usize,
                )
            };
            self.transport
                .slot_mut(slot, prepared.data.length as usize)?
                .write_all_volatile(&data_slice)
                .map_err(io::Error::other)?;
        }
        let posted = match prepared.kind {
            RequestKind::Read => self.transport.post_read(
                slot,
                prepared.offset,
                prepared.data.length as usize,
                slot as u64,
            )?,
            RequestKind::Write => self.transport.post_write(
                slot,
                prepared.offset,
                prepared.data.length as usize,
                slot as u64,
                false,
            )?,
            RequestKind::Flush | RequestKind::GetId | RequestKind::Unsupported => {
                unreachable!("only data requests use a direct OFI queue")
            }
        };
        if !posted {
            self.transport.release(slot)?;
            self.stats.post_eagain += 1;
            return Ok(ArenaQueueDecision::Pressure(prepared));
        }
        self.pending[slot] = Some(DirectPending {
            chain: prepared.chain,
            data: prepared.data,
            data_host_address: prepared.data_host_address,
            status_host_address: prepared.status_host_address,
            slot,
            kind: prepared.kind,
        });
        self.outstanding += 1;
        self.stats.posts += 1;
        self.stats.outstanding_max = self.stats.outstanding_max.max(self.outstanding as u64);
        Ok(ArenaQueueDecision::Queued)
    }

    fn take_completion(&mut self, wait: bool) -> io::Result<Option<DirectPending>> {
        if self.completion_next == self.completions.len() {
            self.completions.clear();
            self.completion_next = 0;
            self.stats.cq_polls += 1;
            self.transport.poll(&mut self.completions, wait)?;
            self.stats.cq_completions += self.completions.len() as u64;
            self.stats.cq_batch_max = self.stats.cq_batch_max.max(self.completions.len() as u64);
        }
        let Some(completion) = self.completions.get(self.completion_next).copied() else {
            return Ok(None);
        };
        self.completion_next += 1;
        let request = self
            .pending
            .get_mut(completion.slot)
            .and_then(Option::take)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown direct-OFI completion slot",
                )
            })?;
        let expected = match request.kind {
            RequestKind::Read => VhostOfiCompletionKind::Read,
            RequestKind::Write => VhostOfiCompletionKind::Write,
            _ => unreachable!(),
        };
        if completion.kind != expected || completion.user_data != completion.slot as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "direct-OFI completion token or operation mismatch",
            ));
        }
        self.outstanding = self.outstanding.saturating_sub(1);
        Ok(Some(request))
    }

    fn copy_read_to_guest(&mut self, request: &DirectPending) -> io::Result<()> {
        let mut source = self
            .transport
            .slot(request.slot, request.data.length as usize)?;
        // SAFETY: admission range-checked the guest descriptor and its memory
        // generation is retained by request.chain.
        let mut destination = unsafe {
            VolatileSlice::new(
                request.data_host_address as *mut u8,
                request.data.length as usize,
            )
        };
        source
            .read_exact_volatile(&mut destination)
            .map_err(io::Error::other)
    }

    fn release(&mut self, slot: usize) -> io::Result<()> {
        self.transport.release(slot)
    }
}

struct DirectOfiStage {
    client: Arc<VhostOfiClient>,
    slot_bytes: usize,
    read_only: bool,
}

impl DirectOfiStage {
    fn new(config: VhostOfiClientConfig, slot_bytes: usize, read_only: bool) -> io::Result<Self> {
        if !(4096..=1024 * 1024).contains(&slot_bytes) || !slot_bytes.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "direct-OFI slot bytes must be a power of two in 4096..=1048576",
            ));
        }
        Ok(Self {
            client: Arc::new(VhostOfiClient::new(config)?),
            slot_bytes,
            read_only,
        })
    }
}

impl BlockStage for DirectOfiStage {
    fn capacity_bytes(&self) -> u64 {
        self.client.capacity_bytes()
    }

    fn read_at(&self, _queue: usize, _offset: u64, _destination: &mut [u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "direct-OFI requires the asynchronous contiguous-descriptor path",
        ))
    }

    fn write_at(&self, _queue: usize, _offset: u64, _source: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "direct-OFI requires the asynchronous contiguous-descriptor path",
        ))
    }

    fn flush(&self) -> io::Result<()> {
        self.client.flush()
    }

    fn shutdown(&self) -> io::Result<()> {
        self.client.shutdown()
    }

    fn open_direct_queue(&self, queue: usize, depth: u32) -> io::Result<Option<DirectDataQueue>> {
        let transport = self
            .client
            .connect_queue(queue, depth as usize, self.slot_bytes)?;
        Ok(Some(DirectDataQueue::new(transport, self.read_only)))
    }
}

impl BlockStage for ArenaStage {
    fn capacity_bytes(&self) -> u64 {
        self.capacity
    }

    fn read_at(&self, queue: usize, mut offset: u64, mut destination: &mut [u8]) -> io::Result<()> {
        self.checked_end(offset, destination.len())?;
        let slot_bytes = self.arena.slot_bytes();
        while !destination.is_empty() {
            let slot_offset = offset % slot_bytes as u64;
            let block_offset = offset - slot_offset;
            let count = destination.len().min(slot_bytes - slot_offset as usize);
            let mut buffer = self.arena.allocate(self.lane(queue))?;
            buffer.read_at(&self.device, block_offset)?;
            destination[..count].copy_from_slice(
                &buffer.as_slice()?[slot_offset as usize..slot_offset as usize + count],
            );
            destination = &mut destination[count..];
            offset += count as u64;
        }
        Ok(())
    }

    fn write_at(&self, queue: usize, mut offset: u64, mut source: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "zcnblk userspace stage is read-only",
            ));
        }
        self.checked_end(offset, source.len())?;
        let slot_bytes = self.arena.slot_bytes();
        while !source.is_empty() {
            let slot_offset = offset % slot_bytes as u64;
            let block_offset = offset - slot_offset;
            let count = source.len().min(slot_bytes - slot_offset as usize);
            let mut buffer = self.arena.allocate(self.lane(queue))?;
            if slot_offset != 0 || count != slot_bytes {
                buffer.read_at(&self.device, block_offset)?;
            }
            buffer.as_mut_slice()?[slot_offset as usize..slot_offset as usize + count]
                .copy_from_slice(&source[..count]);
            buffer.write_at(&self.device, block_offset)?;
            // Ordinary writes are acknowledged when the downstream userspace
            // stage admits the shared-slot lease.  The stage may retain that
            // slot as dirty state until a later FLUSH; dropping this handle
            // deliberately leaves the sequence token owned downstream.  A
            // fresh request allocates another free slot, so the vhost queue
            // does not turn every write into an implicit sync barrier.
            source = &source[count..];
            offset += count as u64;
        }
        Ok(())
    }

    fn read_at_guest(
        &self,
        queue: usize,
        mut offset: u64,
        destination: &mut VolatileSlice<'_, ()>,
    ) -> io::Result<()> {
        self.checked_end(offset, destination.len())?;
        let slot_bytes = self.arena.slot_bytes();
        let mut completed = 0usize;
        while completed < destination.len() {
            let slot_offset = offset % slot_bytes as u64;
            let block_offset = offset - slot_offset;
            let count = (destination.len() - completed).min(slot_bytes - slot_offset as usize);
            let mut buffer = self.arena.allocate(self.lane(queue))?;
            buffer.read_at(&self.device, block_offset)?;
            let mut target = destination
                .subslice(completed, count)
                .map_err(io::Error::other)?;
            (&buffer.as_slice()?[slot_offset as usize..slot_offset as usize + count])
                .read_exact_volatile(&mut target)
                .map_err(io::Error::other)?;
            completed += count;
            offset += count as u64;
        }
        Ok(())
    }

    fn write_at_guest(
        &self,
        queue: usize,
        mut offset: u64,
        source: &VolatileSlice<'_, ()>,
    ) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "zcnblk userspace stage is read-only",
            ));
        }
        self.checked_end(offset, source.len())?;
        let slot_bytes = self.arena.slot_bytes();
        let mut completed = 0usize;
        while completed < source.len() {
            let slot_offset = offset % slot_bytes as u64;
            let block_offset = offset - slot_offset;
            let count = (source.len() - completed).min(slot_bytes - slot_offset as usize);
            let mut buffer = self.arena.allocate(self.lane(queue))?;
            if slot_offset != 0 || count != slot_bytes {
                buffer.read_at(&self.device, block_offset)?;
            }
            let source_part = source
                .subslice(completed, count)
                .map_err(io::Error::other)?;
            (&mut buffer.as_mut_slice()?[slot_offset as usize..slot_offset as usize + count])
                .write_all_volatile(&source_part)
                .map_err(io::Error::other)?;
            buffer.write_at(&self.device, block_offset)?;
            completed += count;
            offset += count as u64;
        }
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        self.device.sync_all()
    }

    fn open_arena_queue(&self, queue: usize, depth: u32) -> io::Result<Option<ArenaQueue>> {
        let lane = u32::try_from(queue)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "queue exceeds u32"))?;
        if lane >= self.arena.channels() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "vhost queue count exceeds zcnblk arena lane count",
            ));
        }
        ArenaQueue::new(self, lane, depth).map(Some)
    }
}

#[derive(Clone, Copy, Default)]
struct Stats {
    reads: u64,
    writes: u64,
    flushes: u64,
    get_ids: u64,
    unsupported: u64,
    io_errors: u64,
    read_bytes: u64,
    write_bytes: u64,
}

#[derive(Clone, Copy)]
struct DescriptorMeta {
    address: GuestAddress,
    length: u32,
    writable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestKind {
    Read,
    Write,
    Flush,
    GetId,
    Unsupported,
}

struct RequestHeader {
    kind: RequestKind,
    sector: u64,
}

#[derive(Clone, Copy)]
struct GuestRegionFastPath {
    guest_start: u64,
    guest_end: u64,
    host_start: usize,
}

fn parse_header(bytes: &[u8; 16]) -> RequestHeader {
    let raw_type = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) & !VIRTIO_BLK_T_BARRIER;
    let kind = match raw_type {
        VIRTIO_BLK_T_IN => RequestKind::Read,
        VIRTIO_BLK_T_OUT => RequestKind::Write,
        VIRTIO_BLK_T_FLUSH => RequestKind::Flush,
        VIRTIO_BLK_T_GET_ID => RequestKind::GetId,
        _ => RequestKind::Unsupported,
    };
    RequestHeader {
        kind,
        sector: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
    }
}

fn virtio_need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

struct QueueWorker {
    stage: Arc<dyn BlockStage>,
    capacity_bytes: u64,
    arena_queue: Option<ArenaQueue>,
    direct_queue: Option<DirectDataQueue>,
    stats: Stats,
    queue: usize,
    cpu: Option<usize>,
    pinned: bool,
    completion_event_enabled: bool,
    serial: [u8; SERIAL_LEN],
    descriptors: Vec<DescriptorMeta>,
    guest_memory_identity: usize,
    guest_memory_snapshot: Option<Arc<GuestMemoryMmap>>,
    guest_regions: Vec<GuestRegionFastPath>,
    last_guest_region: usize,
    fast_avail_ring: u64,
    fast_avail_idx: u16,
    fast_avail_idx_valid: bool,
    event_idx: bool,
    writeback: Arc<AtomicBool>,
    exit_event: EventFd,
}

impl QueueWorker {
    fn new(
        stage: Arc<dyn BlockStage>,
        queue: usize,
        queue_size: u32,
        cpu: Option<usize>,
        completion_event_enabled: bool,
        serial: [u8; SERIAL_LEN],
        writeback: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let arena_queue = stage.open_arena_queue(queue, queue_size)?;
        let direct_queue = stage.open_direct_queue(queue, queue_size)?;
        if arena_queue.is_some() && direct_queue.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a vhost queue cannot have both block-arena and direct-OFI transports",
            ));
        }
        let capacity_bytes = stage.capacity_bytes();
        let completion_event_enabled = completion_event_enabled && arena_queue.is_some();
        Ok(Self {
            stage,
            capacity_bytes,
            arena_queue,
            direct_queue,
            stats: Stats::default(),
            queue,
            cpu,
            pinned: false,
            completion_event_enabled,
            serial,
            descriptors: Vec::with_capacity(32),
            guest_memory_identity: 0,
            guest_memory_snapshot: None,
            guest_regions: Vec::with_capacity(4),
            last_guest_region: 0,
            fast_avail_ring: 0,
            fast_avail_idx: 0,
            fast_avail_idx_valid: false,
            event_idx: false,
            writeback,
            exit_event: EventFd::new(libc::EFD_NONBLOCK)?,
        })
    }

    fn load_descriptors(&mut self, chain: &mut LoadedDescriptorChain) -> bool {
        self.descriptors.clear();
        for descriptor in chain.by_ref() {
            if self.descriptors.len() == MAX_DATA_DESCRIPTORS {
                return false;
            }
            self.descriptors.push(DescriptorMeta {
                address: descriptor.addr(),
                length: descriptor.len(),
                writable: descriptor.is_write_only(),
            });
        }
        self.descriptors.len() >= 2
    }

    fn has_async_data_queue(&self) -> bool {
        self.arena_queue.is_some() || self.direct_queue.is_some()
    }

    fn async_data_shape(&self) -> Option<(usize, bool)> {
        if let Some(arena) = self.arena_queue.as_ref() {
            Some((arena.slot_bytes, arena.read_only))
        } else {
            self.direct_queue
                .as_ref()
                .map(|direct| (direct.slot_bytes(), direct.read_only))
        }
    }

    fn async_data_has_capacity(&self) -> bool {
        if let Some(arena) = self.arena_queue.as_ref() {
            arena.has_capacity()
        } else {
            self.direct_queue
                .as_ref()
                .is_none_or(DirectDataQueue::has_capacity)
        }
    }

    fn load_three_descriptors_fast(
        &mut self,
        descriptor_table: u64,
        queue_size: u16,
        head_index: u16,
    ) -> Option<[DescriptorMeta; 3]> {
        if queue_size == 0
            || head_index >= queue_size
            || descriptor_table & 15 != 0
            || usize::from(queue_size) > MAX_DATA_DESCRIPTORS
        {
            return None;
        }
        let Some(table_bytes) = usize::from(queue_size).checked_mul(16) else {
            return None;
        };
        let Some(table_host) = self.guest_host_address(GuestAddress(descriptor_table), table_bytes)
        else {
            return None;
        };

        let mut index = head_index;
        let mut yielded_bytes = 0u32;
        let mut descriptors = [DescriptorMeta {
            address: GuestAddress(0),
            length: 0,
            writable: false,
        }; 3];
        for (position, descriptor) in descriptors.iter_mut().enumerate() {
            if index >= queue_size {
                return None;
            }
            let offset = usize::from(index) * 16;
            // SAFETY: the complete, 16-byte-aligned descriptor table was
            // range-checked above. The driver cannot modify a descriptor
            // between publishing its avail index and receiving the used one.
            let (address, length, flags, next) = unsafe {
                (
                    u64::from_le(std::ptr::read_volatile((table_host + offset) as *const u64)),
                    u32::from_le(std::ptr::read_volatile(
                        (table_host + offset + 8) as *const u32,
                    )),
                    u16::from_le(std::ptr::read_volatile(
                        (table_host + offset + 12) as *const u16,
                    )),
                    u16::from_le(std::ptr::read_volatile(
                        (table_host + offset + 14) as *const u16,
                    )),
                )
            };
            if flags & VRING_DESC_F_INDIRECT as u16 != 0 {
                // INDIRECT_DESC is not advertised, but leave a defensive
                // complete parser fallback for a nonconforming guest.
                return None;
            }
            let Some(new_yielded_bytes) = yielded_bytes.checked_add(length) else {
                return None;
            };
            yielded_bytes = new_yielded_bytes;
            *descriptor = DescriptorMeta {
                address: GuestAddress(address),
                length,
                writable: flags & VRING_DESC_F_WRITE as u16 != 0,
            };
            let has_next = flags & VRING_DESC_F_NEXT as u16 != 0;
            if has_next != (position != 2) {
                // Ordinary 4K requests have exactly header, data, and status.
                // Any other shape remains supported by the complete parser.
                return None;
            }
            index = next;
        }
        Some(descriptors)
    }

    fn start_arena(&mut self) -> io::Result<()> {
        if let Some(arena) = self.arena_queue.as_mut() {
            arena.start(self.completion_event_enabled)?;
        }
        Ok(())
    }

    fn consume_arena_completion_event(&mut self) -> io::Result<()> {
        if self.completion_event_enabled
            && let Some(arena) = self.arena_queue.as_mut()
        {
            arena.consume_completion_event()?;
        }
        Ok(())
    }

    fn set_arena_completion_event_armed(&mut self, armed: bool) {
        if self.completion_event_enabled
            && let Some(arena) = self.arena_queue.as_mut()
        {
            arena.set_completion_event_armed(armed);
        }
    }

    fn run_arena_task_work(&mut self) -> io::Result<()> {
        if self.completion_event_enabled
            && let Some(arena) = self.arena_queue.as_mut()
        {
            arena.run_task_work()?;
        }
        Ok(())
    }

    fn refresh_guest_regions(
        &mut self,
        memory: &GuestMemoryLoadGuard<GuestMemoryMmap>,
    ) -> io::Result<()> {
        let identity = std::ptr::from_ref(&**memory).addr();
        if identity == self.guest_memory_identity {
            return Ok(());
        }

        self.guest_regions.clear();
        for region in memory.iter() {
            let guest_start = region.start_addr().0;
            let guest_end = guest_start.checked_add(region.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "guest memory region overflows")
            })?;
            let host_start = region
                .get_host_address(MemoryRegionAddress(0))
                .map_err(io::Error::other)?
                .addr();
            self.guest_regions.push(GuestRegionFastPath {
                guest_start,
                guest_end,
                host_start,
            });
        }
        if self.guest_regions.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "vhost guest memory table is empty",
            ));
        }
        self.guest_memory_identity = identity;
        // Keep the exact RCU snapshot that supplied the cached host pointers.
        // Besides extending their lifetime, this prevents allocator-address
        // reuse from making a replacement memory table look identical.
        self.guest_memory_snapshot = Some(memory.clone().into_inner());
        self.last_guest_region = 0;
        self.fast_avail_idx_valid = false;
        Ok(())
    }

    fn guest_host_address(&mut self, address: GuestAddress, length: usize) -> Option<usize> {
        let start = address.0;
        let end = start.checked_add(length as u64)?;
        let cached = self.guest_regions.get(self.last_guest_region)?;
        if start >= cached.guest_start && end <= cached.guest_end {
            return Some(cached.host_start + (start - cached.guest_start) as usize);
        }

        let (index, region) = self
            .guest_regions
            .iter()
            .enumerate()
            .find(|(_, region)| start >= region.guest_start && end <= region.guest_end)?;
        self.last_guest_region = index;
        Some(region.host_start + (start - region.guest_start) as usize)
    }

    fn add_used_arena_fast(
        &mut self,
        vring: &mut RwLockWriteGuard<VringState<AtomicGuestMemory>>,
        head_index: u16,
        used_len: u32,
    ) -> io::Result<bool> {
        let (size, next_used, used_ring) = {
            let queue = vring.get_queue_mut();
            (queue.size(), queue.next_used(), queue.used_ring())
        };
        if size == 0 || head_index >= size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("arena used entry has invalid head {head_index} for queue size {size}"),
            ));
        }

        // Split virtqueues require the used ring to be four-byte aligned. A
        // nonconforming queue is left to the generic implementation so this
        // fast path never performs a misaligned volatile store.
        if used_ring & 3 != 0 {
            return Ok(false);
        }
        let element_offset = 4usize
            .checked_add(usize::from(next_used % size).saturating_mul(8))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "used ring overflows"))?;
        let span = element_offset
            .checked_add(8)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "used ring overflows"))?;
        let Some(host_base) = self.guest_host_address(GuestAddress(used_ring), span) else {
            // A guest memory boundary can theoretically bisect the ring. The
            // generic vm-memory implementation handles that uncommon layout.
            return Ok(false);
        };

        // SAFETY: guest_host_address range-checked the header and selected
        // used element in the retained guest-memory snapshot; split-ring
        // alignment was checked above. Dirty logging is not negotiated.
        unsafe {
            std::ptr::write_volatile(
                (host_base + element_offset) as *mut u32,
                u32::from(head_index).to_le(),
            );
            std::ptr::write_volatile(
                (host_base + element_offset + 4) as *mut u32,
                used_len.to_le(),
            );
        }

        let new_used = next_used.wrapping_add(1);
        // Publish the element before making its index visible to the driver,
        // matching virtio_queue::Queue::add_used's Release store contract.
        // SAFETY: the range and two-byte alignment of used_ring + 2 follow
        // from the same checked, four-byte-aligned mapping above. The field is
        // shared with the driver and therefore accessed atomically.
        unsafe {
            (&*((host_base + 2) as *const AtomicU16)).store(new_used.to_le(), Ordering::Release);
        }
        vring.get_queue_mut().set_next_used(new_used);
        if let Some(arena) = self.arena_queue.as_mut() {
            arena.completion_stats.direct_used_entries += 1;
        } else if let Some(direct) = self.direct_queue.as_mut() {
            direct.stats.direct_used_entries += 1;
        }
        Ok(true)
    }

    fn arena_needs_notification(
        &mut self,
        vring: &mut RwLockWriteGuard<VringState<AtomicGuestMemory>>,
        old_used: u16,
    ) -> bool {
        if !self.event_idx {
            return true;
        }

        // Complete used-ring publication before sampling the driver's event
        // index, matching the ordering in virtio_queue::needs_notification.
        fence(Ordering::SeqCst);
        let (size, new_used, avail_ring) = {
            let queue = vring.get_queue_mut();
            (queue.size(), queue.next_used(), queue.avail_ring())
        };
        let Some(event_offset) = u64::from(size)
            .checked_mul(2)
            .and_then(|n| n.checked_add(4))
        else {
            return true;
        };
        let Some(event_address) = avail_ring.checked_add(event_offset) else {
            return true;
        };
        let Some(event_host) = self.guest_host_address(GuestAddress(event_address), 2) else {
            // A spurious interrupt is safe; suppressing a required one is not.
            return true;
        };
        if event_host & 1 != 0 {
            return true;
        }
        // SAFETY: guest_host_address range-checked the two-byte field and its
        // alignment was checked immediately above. It is shared with the
        // driver and therefore sampled atomically.
        let event =
            u16::from_le(unsafe { (&*(event_host as *const AtomicU16)).load(Ordering::Relaxed) });
        virtio_need_event(event, new_used, old_used)
    }

    fn validate_data_direction(&self, kind: RequestKind) -> bool {
        self.descriptors[1..self.descriptors.len() - 1]
            .iter()
            .all(|descriptor| match kind {
                RequestKind::Read | RequestKind::GetId => descriptor.writable,
                RequestKind::Write => !descriptor.writable,
                RequestKind::Flush => false,
                RequestKind::Unsupported => true,
            })
    }

    fn data_len(&self) -> Option<u64> {
        self.descriptors[1..self.descriptors.len() - 1]
            .iter()
            .try_fold(0u64, |total, descriptor| {
                total.checked_add(u64::from(descriptor.length))
            })
    }

    fn execute(&mut self, memory: &GuestMemoryMmap, header: RequestHeader) -> io::Result<u32> {
        let data_len = self.data_len().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "request length overflow")
        })?;
        let mut offset = header
            .sector
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "sector offset overflow"))?;

        if matches!(header.kind, RequestKind::Read | RequestKind::Write) {
            let end = offset.checked_add(data_len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "request range overflow")
            })?;
            if end > self.stage.capacity_bytes() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "request exceeds volume capacity",
                ));
            }
        }

        match header.kind {
            RequestKind::Read => {
                for descriptor in &self.descriptors[1..self.descriptors.len() - 1] {
                    for destination in
                        memory.get_slices(descriptor.address, descriptor.length as usize)
                    {
                        let mut destination = destination.map_err(io::Error::other)?;
                        self.stage
                            .read_at_guest(self.queue, offset, &mut destination)?;
                        offset += destination.len() as u64;
                    }
                }
                self.stats.reads += 1;
                self.stats.read_bytes += data_len;
                Ok(u32::try_from(data_len).unwrap_or(u32::MAX))
            }
            RequestKind::Write => {
                for descriptor in &self.descriptors[1..self.descriptors.len() - 1] {
                    for source in memory.get_slices(descriptor.address, descriptor.length as usize)
                    {
                        let source = source.map_err(io::Error::other)?;
                        self.stage.write_at_guest(self.queue, offset, &source)?;
                        offset += source.len() as u64;
                    }
                }
                if !self.writeback.load(Ordering::Acquire) {
                    self.stage.flush()?;
                }
                self.stats.writes += 1;
                self.stats.write_bytes += data_len;
                Ok(0)
            }
            RequestKind::Flush => {
                self.stage.flush()?;
                self.stats.flushes += 1;
                Ok(0)
            }
            RequestKind::GetId => {
                let mut copied = 0usize;
                for descriptor in &self.descriptors[1..self.descriptors.len() - 1] {
                    if copied == SERIAL_LEN {
                        break;
                    }
                    let count = (descriptor.length as usize).min(SERIAL_LEN - copied);
                    memory
                        .write_slice(&self.serial[copied..copied + count], descriptor.address)
                        .map_err(io::Error::other)?;
                    copied += count;
                }
                if copied != SERIAL_LEN {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "GET_ID buffer is shorter than 20 bytes",
                    ));
                }
                self.stats.get_ids += 1;
                Ok(SERIAL_LEN as u32)
            }
            RequestKind::Unsupported => unreachable!(),
        }
    }

    fn process_loaded_chain(&mut self, chain: &LoadedDescriptorChain) -> u32 {
        debug_assert!(self.descriptors.len() >= 2);
        let header_descriptor = self.descriptors[0];
        let status_descriptor = *self.descriptors.last().unwrap();
        let memory = chain.memory();
        let mut header_bytes = [0u8; 16];

        let structural_error = header_descriptor.writable
            || header_descriptor.length < header_bytes.len() as u32
            || !status_descriptor.writable
            || status_descriptor.length == 0;
        if structural_error {
            self.stats.io_errors += 1;
            return 0;
        }

        let header = match memory.read_slice(&mut header_bytes, header_descriptor.address) {
            Ok(()) => parse_header(&header_bytes),
            Err(_) => {
                self.stats.io_errors += 1;
                return 0;
            }
        };

        let (status, used_data) = if header.kind == RequestKind::Unsupported {
            self.stats.unsupported += 1;
            (VIRTIO_BLK_S_UNSUPP as u8, 0)
        } else if header.kind == RequestKind::Flush {
            if self.descriptors.len() != 2 {
                self.stats.io_errors += 1;
                (VIRTIO_BLK_S_IOERR as u8, 0)
            } else {
                match self.execute(memory, header) {
                    Ok(length) => (VIRTIO_BLK_S_OK as u8, length),
                    Err(error) => {
                        eprintln!("zcvhost-user-blk: flush failed: {error}");
                        self.stats.io_errors += 1;
                        (VIRTIO_BLK_S_IOERR as u8, 0)
                    }
                }
            }
        } else if self.descriptors.len() < 3 || !self.validate_data_direction(header.kind) {
            self.stats.io_errors += 1;
            (VIRTIO_BLK_S_IOERR as u8, 0)
        } else {
            match self.execute(memory, header) {
                Ok(length) => (VIRTIO_BLK_S_OK as u8, length),
                Err(error) => {
                    eprintln!("zcvhost-user-blk: request failed: {error}");
                    self.stats.io_errors += 1;
                    (VIRTIO_BLK_S_IOERR as u8, 0)
                }
            }
        };

        if memory.write_obj(status, status_descriptor.address).is_err() {
            self.stats.io_errors += 1;
            return 0;
        }
        used_data.saturating_add(1)
    }

    fn process_chain(&mut self, chain: &mut LoadedDescriptorChain) -> u32 {
        if !self.load_descriptors(chain) {
            self.stats.io_errors += 1;
            return 0;
        }
        self.process_loaded_chain(chain)
    }

    fn prepare_three_arena_plan(
        &mut self,
        descriptors: [DescriptorMeta; 3],
    ) -> io::Result<Option<ArenaDataPlan>> {
        let Some((slot_bytes, read_only)) = self.async_data_shape() else {
            return Ok(None);
        };
        let direct_ofi = self.direct_queue.is_some();

        let [header_descriptor, data, status] = descriptors;
        if header_descriptor.writable
            || header_descriptor.length < 16
            || !status.writable
            || status.length == 0
        {
            return Ok(None);
        }
        let mut header_bytes = [0u8; 16];
        let Some(header_host_address) =
            self.guest_host_address(header_descriptor.address, header_bytes.len())
        else {
            return Ok(None);
        };
        // SAFETY: refresh_guest_regions range-checks the immutable vhost
        // memory map and the submitted virtio descriptor remains owned by the
        // device until completion. The destination is a distinct 16-byte
        // stack array.
        unsafe {
            std::ptr::copy_nonoverlapping(
                header_host_address as *const u8,
                header_bytes.as_mut_ptr(),
                header_bytes.len(),
            );
        }
        let header = parse_header(&header_bytes);
        if !matches!(header.kind, RequestKind::Read | RequestKind::Write)
            || match header.kind {
                RequestKind::Read => !data.writable,
                RequestKind::Write => data.writable,
                RequestKind::Flush | RequestKind::GetId | RequestKind::Unsupported => true,
            }
            || if direct_ofi {
                data.length == 0 || data.length as usize > slot_bytes
            } else {
                data.length as usize != slot_bytes
            }
            || (header.kind == RequestKind::Write
                && (read_only || (!direct_ofi && !self.writeback.load(Ordering::Acquire))))
        {
            return Ok(None);
        }
        let offset = header
            .sector
            .checked_mul(SECTOR_SIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "sector offset overflow"))?;
        let end = offset
            .checked_add(u64::from(data.length))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "request range overflow"))?;
        let alignment = if direct_ofi {
            SECTOR_SIZE
        } else {
            slot_bytes as u64
        };
        if offset % alignment != 0 || end > self.capacity_bytes {
            return Ok(None);
        }

        // The common fast path is one aligned, physically contiguous 4 KiB
        // data descriptor. Fragmented guest ranges remain on the complete
        // synchronous fallback rather than acquiring a new bounce buffer.
        let Some(data_host_address) = self.guest_host_address(data.address, data.length as usize)
        else {
            return Ok(None);
        };
        let Some(status_host_address) = self.guest_host_address(status.address, 1) else {
            return Ok(None);
        };
        Ok(Some(ArenaDataPlan {
            data,
            data_host_address,
            status_host_address,
            offset,
            kind: header.kind,
        }))
    }

    fn prepare_arena_plan(&mut self) -> io::Result<Option<ArenaDataPlan>> {
        if self.descriptors.len() != 3 {
            return Ok(None);
        }
        let descriptors = [
            self.descriptors[0],
            self.descriptors[1],
            self.descriptors[2],
        ];
        self.prepare_three_arena_plan(descriptors)
    }

    fn prepare_arena_data(
        &mut self,
        mut chain: LoadedDescriptorChain,
    ) -> io::Result<ArenaPrepareDecision> {
        if !self.has_async_data_queue() {
            return Ok(ArenaPrepareDecision::FallbackLoaded(chain));
        }
        if !self.load_descriptors(&mut chain) {
            return Ok(ArenaPrepareDecision::Invalid(chain));
        }
        let Some(plan) = self.prepare_arena_plan()? else {
            return Ok(ArenaPrepareDecision::FallbackLoaded(chain));
        };
        Ok(ArenaPrepareDecision::Prepared(ArenaPrepared {
            chain: ArenaChain::Loaded(chain),
            data: plan.data,
            data_host_address: plan.data_host_address,
            status_host_address: plan.status_host_address,
            offset: plan.offset,
            kind: plan.kind,
        }))
    }

    fn prepare_arena_fast(
        &mut self,
        vring: &mut RwLockWriteGuard<VringState<AtomicGuestMemory>>,
    ) -> io::Result<FastArenaPrepareDecision> {
        let (ready, size, next_avail, avail_ring, descriptor_table) = {
            let queue = vring.get_queue_mut();
            (
                queue.ready(),
                queue.size(),
                queue.next_avail(),
                queue.avail_ring(),
                queue.desc_table(),
            )
        };
        if !ready || size == 0 || avail_ring & 1 != 0 {
            return Ok(FastArenaPrepareDecision::Fallback);
        }

        if !self.fast_avail_idx_valid
            || self.fast_avail_ring != avail_ring
            || self.fast_avail_idx == next_avail
        {
            let Some(avail_idx_address) = avail_ring.checked_add(2) else {
                return Ok(FastArenaPrepareDecision::Fallback);
            };
            let Some(avail_idx_host) = self.guest_host_address(GuestAddress(avail_idx_address), 2)
            else {
                return Ok(FastArenaPrepareDecision::Fallback);
            };
            if avail_idx_host & 1 != 0 {
                return Ok(FastArenaPrepareDecision::Fallback);
            }
            // SAFETY: the aligned index was range-checked above. Its Acquire
            // load publishes a complete batch of ring heads and descriptors.
            self.fast_avail_idx = u16::from_le(unsafe {
                (&*(avail_idx_host as *const AtomicU16)).load(Ordering::Acquire)
            });
            self.fast_avail_ring = avail_ring;
            self.fast_avail_idx_valid = true;
        }
        let avail_idx = self.fast_avail_idx;
        let available = avail_idx.wrapping_sub(next_avail);
        if available == 0 {
            return Ok(FastArenaPrepareDecision::Empty);
        }
        if available > size {
            return Ok(FastArenaPrepareDecision::Fallback);
        }

        let Some(head_offset) = u64::from(next_avail % size)
            .checked_mul(2)
            .and_then(|offset| offset.checked_add(4))
        else {
            return Ok(FastArenaPrepareDecision::Fallback);
        };
        let Some(head_address) = avail_ring.checked_add(head_offset) else {
            return Ok(FastArenaPrepareDecision::Fallback);
        };
        let Some(head_host) = self.guest_host_address(GuestAddress(head_address), 2) else {
            return Ok(FastArenaPrepareDecision::Fallback);
        };
        if head_host & 1 != 0 {
            return Ok(FastArenaPrepareDecision::Fallback);
        }
        // SAFETY: the driver-published ring entry is aligned and range-checked
        // and the Acquire avail-index load made it visible.
        let head_index = u16::from_le(unsafe { std::ptr::read_volatile(head_host as *const u16) });
        let Some(descriptors) =
            self.load_three_descriptors_fast(descriptor_table, size, head_index)
        else {
            return Ok(FastArenaPrepareDecision::Fallback);
        };
        let Some(plan) = self.prepare_three_arena_plan(descriptors)? else {
            return Ok(FastArenaPrepareDecision::Fallback);
        };

        vring
            .get_queue_mut()
            .set_next_avail(next_avail.wrapping_add(1));
        if let Some(arena) = self.arena_queue.as_mut() {
            arena.completion_stats.direct_avail_entries += 1;
        } else if let Some(direct) = self.direct_queue.as_mut() {
            direct.stats.direct_avail_entries += 1;
        }
        Ok(FastArenaPrepareDecision::Prepared(ArenaPrepared {
            chain: ArenaChain::Fast { head_index },
            data: plan.data,
            data_host_address: plan.data_host_address,
            status_host_address: plan.status_host_address,
            offset: plan.offset,
            kind: plan.kind,
        }))
    }

    fn queue_prepared_arena_data(
        &mut self,
        prepared: ArenaPrepared,
    ) -> io::Result<ArenaQueueDecision> {
        if let Some(direct) = self.direct_queue.as_mut() {
            return direct.queue(prepared);
        }
        let mut buffer = match self
            .arena_queue
            .as_mut()
            .expect("arena queue remains present")
            .allocate()
        {
            Ok(buffer) => buffer,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(ArenaQueueDecision::Pressure(prepared));
            }
            Err(error) => return Err(error),
        };
        if prepared.kind == RequestKind::Write {
            // SAFETY: the cached vhost region spans the complete descriptor
            // and prepared.chain retains the corresponding RCU snapshot.
            let data_slice = unsafe {
                VolatileSlice::new(
                    prepared.data_host_address as *mut u8,
                    prepared.data.length as usize,
                )
            };
            (&mut buffer.as_mut_slice()?[..])
                .write_all_volatile(&data_slice)
                .map_err(io::Error::other)?;
        }
        let request = ArenaPending {
            chain: prepared.chain,
            data: prepared.data,
            data_host_address: prepared.data_host_address,
            status_host_address: prepared.status_host_address,
            buffer,
            kind: prepared.kind,
        };
        self.arena_queue
            .as_mut()
            .expect("arena queue remains present")
            .queue(prepared.offset, request)?;
        Ok(ArenaQueueDecision::Queued)
    }

    fn queue_arena_with_pressure(
        &mut self,
        vring: &mut RwLockWriteGuard<VringState<AtomicGuestMemory>>,
        prepared: ArenaPrepared,
    ) -> io::Result<bool> {
        match self.queue_prepared_arena_data(prepared)? {
            ArenaQueueDecision::Queued => Ok(false),
            ArenaQueueDecision::Pressure(prepared) => {
                // Retire this lane's io_uring work and drop its local slot
                // handles before the exceptional global barrier. Other lanes
                // do the same independently; the shared slow-path mutex
                // prevents a sync storm.
                let used_any = self.drain_arena(vring)?;
                if self.arena_queue.is_some() {
                    self.arena_pressure_sync()?;
                }
                match self.queue_prepared_arena_data(prepared)? {
                    ArenaQueueDecision::Queued => Ok(used_any),
                    ArenaQueueDecision::Pressure(_) => Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "vhost asynchronous lane {} remained full after draining completions",
                            self.queue
                        ),
                    )),
                }
            }
        }
    }

    fn arena_pressure_sync(&self) -> io::Result<()> {
        self.arena_queue
            .as_ref()
            .expect("arena pressure requires an arena queue")
            .pressure_sync()
    }

    fn submit_arena(&mut self) -> io::Result<()> {
        if let Some(arena) = self.arena_queue.as_mut() {
            arena.submit()?;
        }
        Ok(())
    }

    fn arena_outstanding(&self) -> usize {
        self.arena_queue.as_ref().map_or_else(
            || {
                self.direct_queue
                    .as_ref()
                    .map_or(0, DirectDataQueue::outstanding)
            },
            ArenaQueue::outstanding,
        )
    }

    fn retire_direct_one(
        &mut self,
        vring: &mut RwLockWriteGuard<VringState<AtomicGuestMemory>>,
        wait: bool,
    ) -> io::Result<bool> {
        let Some(request) = self
            .direct_queue
            .as_mut()
            .expect("direct queue remains present")
            .take_completion(wait)?
        else {
            return Ok(false);
        };
        let mut success = true;
        if request.kind == RequestKind::Read {
            success = self
                .direct_queue
                .as_mut()
                .expect("direct queue remains present")
                .copy_read_to_guest(&request)
                .is_ok();
        } else if request.kind == RequestKind::Write
            && !self.writeback.load(Ordering::Acquire)
            && self.stage.flush().is_err()
        {
            // In write-through mode, do not expose the used entry until the
            // downstream userspace stage has acknowledged the lane-vector
            // barrier containing this write.
            success = false;
        }
        let status = if success {
            VIRTIO_BLK_S_OK as u8
        } else {
            self.stats.io_errors += 1;
            VIRTIO_BLK_S_IOERR as u8
        };
        // SAFETY: admission range-checked this byte in the retained guest
        // mapping generation.
        unsafe {
            std::ptr::write_volatile(request.status_host_address as *mut u8, status);
        }
        let used_len = if success {
            match request.kind {
                RequestKind::Read => {
                    self.stats.reads += 1;
                    self.stats.read_bytes += u64::from(request.data.length);
                    request.data.length.saturating_add(1)
                }
                RequestKind::Write => {
                    self.stats.writes += 1;
                    self.stats.write_bytes += u64::from(request.data.length);
                    1
                }
                _ => unreachable!("only data requests complete through direct OFI"),
            }
        } else {
            0
        };
        if !self.add_used_arena_fast(vring, request.chain.head_index(), used_len)? {
            let memory = match &request.chain {
                ArenaChain::Loaded(chain) => chain.memory(),
                ArenaChain::Fast { .. } => self
                    .guest_memory_snapshot
                    .as_deref()
                    .ok_or_else(|| io::Error::other("vhost guest memory snapshot is absent"))?,
            };
            vring
                .get_queue_mut()
                .add_used(memory, request.chain.head_index(), used_len)
                .map_err(|error| io::Error::other(format!("direct-OFI add used: {error}")))?;
        }
        self.direct_queue
            .as_mut()
            .expect("direct queue remains present")
            .release(request.slot)?;
        Ok(true)
    }

    fn retire_data_one(
        &mut self,
        vring: &mut RwLockWriteGuard<VringState<AtomicGuestMemory>>,
        wait: bool,
    ) -> io::Result<bool> {
        if self.direct_queue.is_some() {
            self.retire_direct_one(vring, wait)
        } else {
            self.retire_arena_one(vring, wait)
        }
    }

    fn retire_arena_one(
        &mut self,
        vring: &mut RwLockWriteGuard<VringState<AtomicGuestMemory>>,
        wait: bool,
    ) -> io::Result<bool> {
        let completion = match self.arena_queue.as_mut() {
            Some(arena) => arena.take_completion(wait)?,
            None => None,
        };
        let Some((mut request, result)) = completion else {
            return Ok(false);
        };

        let mut success = result == request.data.length as i32;
        match request.kind {
            RequestKind::Read if success => {
                success = request
                    .buffer
                    .wait_reacquire(Duration::from_secs(5))
                    .is_ok();
                if success {
                    success = (|| {
                        let source = request.buffer.as_slice()?;
                        // SAFETY: ArenaPending retains the exact RCU guest
                        // memory snapshot from which this range-checked host
                        // pointer was derived.
                        let mut destination = unsafe {
                            VolatileSlice::new(
                                request.data_host_address as *mut u8,
                                request.data.length as usize,
                            )
                        };
                        (&source[..request.data.length as usize])
                            .read_exact_volatile(&mut destination)
                            .map_err(io::Error::other)
                    })()
                    .is_ok();
                }
            }
            RequestKind::Write => {
                // A successful target may already have returned the alias, or
                // a WAL stage may retain it past this early block completion.
                // Retire the local handle correctly in either case. This is
                // also needed after an error CQE because the kernel may have
                // consumed the handoff before reporting the failure.
                success &= request.buffer.retire_completed_handoff().is_ok();
            }
            RequestKind::Read
            | RequestKind::Flush
            | RequestKind::GetId
            | RequestKind::Unsupported => {}
        }

        let status = if success {
            VIRTIO_BLK_S_OK as u8
        } else {
            self.stats.io_errors += 1;
            VIRTIO_BLK_S_IOERR as u8
        };
        // SAFETY: status_host_address was range-checked against the same RCU
        // guest memory snapshot retained in request.chain. Dirty logging is
        // deliberately not negotiated by this backend, so a direct volatile
        // byte store is the complete status-write contract.
        unsafe {
            std::ptr::write_volatile(request.status_host_address as *mut u8, status);
        }
        let used_len = {
            if success {
                match request.kind {
                    RequestKind::Read => {
                        self.stats.reads += 1;
                        self.stats.read_bytes += u64::from(request.data.length);
                        request.data.length.saturating_add(1)
                    }
                    RequestKind::Write => {
                        self.stats.writes += 1;
                        self.stats.write_bytes += u64::from(request.data.length);
                        1
                    }
                    RequestKind::Flush | RequestKind::GetId | RequestKind::Unsupported => {
                        unreachable!("only data requests complete through the arena ring")
                    }
                }
            } else {
                0
            }
        };
        if !self.add_used_arena_fast(vring, request.chain.head_index(), used_len)? {
            let memory = match &request.chain {
                ArenaChain::Loaded(chain) => chain.memory(),
                ArenaChain::Fast { .. } => self
                    .guest_memory_snapshot
                    .as_deref()
                    .ok_or_else(|| io::Error::other("vhost guest memory snapshot is absent"))?,
            };
            vring
                .get_queue_mut()
                .add_used(memory, request.chain.head_index(), used_len)
                .map_err(|error| io::Error::other(format!("arena add used: {error}")))?;
        }
        if request.buffer.is_application_owned() {
            self.arena_queue
                .as_mut()
                .expect("arena queue remains present while retiring its completion")
                .recycle(request.buffer);
        }
        Ok(true)
    }

    fn drain_arena(
        &mut self,
        vring: &mut RwLockWriteGuard<VringState<AtomicGuestMemory>>,
    ) -> io::Result<bool> {
        self.submit_arena()?;
        let mut used_any = false;
        while self.arena_outstanding() != 0 {
            used_any |= self.retire_data_one(vring, true)?;
            while self.retire_data_one(vring, false)? {
                used_any = true;
            }
        }
        Ok(used_any)
    }

    fn reap_arena_ready(
        &mut self,
        vring: &mut RwLockWriteGuard<VringState<AtomicGuestMemory>>,
    ) -> io::Result<bool> {
        let old_used = vring.get_queue_mut().next_used();
        let mut used_any = false;
        while self.retire_data_one(vring, false)? {
            used_any = true;
        }
        let idle_edge = self.arena_outstanding() == 0;
        let event_required = used_any && self.arena_needs_notification(vring, old_used);
        if used_any && (event_required || idle_edge) {
            if idle_edge
                && !event_required
                && let Some(arena) = self.arena_queue.as_mut()
            {
                arena.record_forced_idle_notification();
            }
            if let Err(error) = vring.signal_used_queue() {
                eprintln!("zcvhost-user-blk: failed to signal used queue: {error}");
            }
        }
        Ok(used_any)
    }

    fn process_queue(
        &mut self,
        vring: &mut RwLockWriteGuard<VringState<AtomicGuestMemory>>,
        memory: &GuestMemoryLoadGuard<GuestMemoryMmap>,
    ) -> io::Result<bool> {
        let old_used = vring.get_queue_mut().next_used();
        let mut used_any = false;
        let memory_identity = std::ptr::from_ref(&**memory).addr();
        if self.guest_memory_identity != 0
            && memory_identity != self.guest_memory_identity
            && self.arena_outstanding() != 0
        {
            // Direct pending requests contain cached host addresses rather
            // than one RCU guard apiece. Complete the old generation before
            // refresh_guest_regions releases its retained snapshot.
            used_any |= self.drain_arena(vring)?;
        }
        self.refresh_guest_regions(memory)?;
        while self.retire_data_one(vring, false)? {
            used_any = true;
        }
        loop {
            if self.has_async_data_queue() {
                if !self.async_data_has_capacity() {
                    self.submit_arena()?;
                    used_any |= self.retire_data_one(vring, true)?;
                }
                match self.prepare_arena_fast(vring)? {
                    FastArenaPrepareDecision::Prepared(prepared) => {
                        used_any |= self.queue_arena_with_pressure(vring, prepared)?;
                        continue;
                    }
                    FastArenaPrepareDecision::Empty => break,
                    FastArenaPrepareDecision::Fallback => {}
                }
            }

            let Some(mut chain) = vring.get_queue_mut().pop_descriptor_chain(memory.clone()) else {
                break;
            };
            let head = chain.head_index();
            let mut descriptors_loaded = false;
            let mut invalid_chain = false;
            if self.has_async_data_queue() {
                match self.prepare_arena_data(chain)? {
                    ArenaPrepareDecision::Prepared(prepared) => {
                        used_any |= self.queue_arena_with_pressure(vring, prepared)?;
                        continue;
                    }
                    ArenaPrepareDecision::FallbackLoaded(returned) => {
                        chain = returned;
                        descriptors_loaded = true;
                    }
                    ArenaPrepareDecision::Invalid(returned) => {
                        chain = returned;
                        invalid_chain = true;
                    }
                }
                if descriptors_loaded || invalid_chain {
                    // Preserve queue order around a flush, an unaligned request,
                    // or another uncommon synchronous fallback.
                    used_any |= self.drain_arena(vring)?;
                }
            }
            let used_len = if invalid_chain {
                self.stats.io_errors += 1;
                0
            } else if descriptors_loaded {
                self.process_loaded_chain(&chain)
            } else {
                self.process_chain(&mut chain)
            };
            if let Err(error) = vring
                .get_queue_mut()
                .add_used(chain.memory(), head, used_len)
            {
                eprintln!("zcvhost-user-blk: add_used failed: {error}");
                break;
            }
            used_any = true;
        }

        self.submit_arena()?;
        if self.arena_outstanding() != 0 && !used_any && !self.completion_event_enabled {
            used_any |= self.retire_data_one(vring, true)?;
        }
        while self.retire_data_one(vring, false)? {
            used_any = true;
        }

        if used_any {
            // EVENT_IDX is advisory. At the transition to zero backend
            // outstanding work, force one callfd notification even if the
            // sampled event index says to suppress it. This closes the
            // driver-rearm/device-idle race without adding notifications to a
            // saturated lane; a missed idle-edge interrupt otherwise leaves a
            // complete guest request asleep forever with no future queue kick.
            let idle_edge = self.arena_outstanding() == 0;
            let event_required = self.arena_needs_notification(vring, old_used);
            let should_signal = event_required || idle_edge;
            if should_signal {
                if idle_edge
                    && !event_required
                    && let Some(arena) = self.arena_queue.as_mut()
                {
                    arena.record_forced_idle_notification();
                }
                if let Err(error) = vring.signal_used_queue() {
                    eprintln!("zcvhost-user-blk: failed to signal used queue: {error}");
                }
            }
        }
        Ok(used_any)
    }

    fn poll_queue(
        &mut self,
        vring: &mut RwLockWriteGuard<VringState<AtomicGuestMemory>>,
        poll_for: Duration,
        memory: &GuestMemoryLoadGuard<GuestMemoryMmap>,
    ) -> io::Result<()> {
        let mut idle_since = Instant::now();
        let mut empty_polls = 0u32;
        loop {
            if self.process_queue(vring, memory)? {
                idle_since = Instant::now();
                empty_polls = 0;
            } else {
                empty_polls = empty_polls.wrapping_add(1);
                if empty_polls & POLL_DEADLINE_SAMPLE_MASK == 0 && idle_since.elapsed() >= poll_for
                {
                    break;
                }
            }
            std::hint::spin_loop();
        }
        Ok(())
    }
}

struct Backend {
    workers: Vec<Mutex<QueueWorker>>,
    config: RwLock<Vec<u8>>,
    read_only: bool,
    queue_size: usize,
    queue_masks: Vec<u64>,
    memory: AtomicGuestMemory,
    acknowledged_features: AtomicU64,
    writeback: Arc<AtomicBool>,
    poll_for: Duration,
    offer_event_idx: bool,
}

impl Backend {
    fn new(
        stage: Arc<dyn BlockStage>,
        memory: AtomicGuestMemory,
        queues: usize,
        queue_size: usize,
        queue_cpus: &[usize],
        read_only: bool,
        poll_for: Duration,
        offer_event_idx: bool,
        serial: [u8; SERIAL_LEN],
    ) -> io::Result<Self> {
        let writeback = Arc::new(AtomicBool::new(true));
        let mut config = vec![0u8; CONFIG_LEN];
        config[0..8].copy_from_slice(&(stage.capacity_bytes() / SECTOR_SIZE).to_le_bytes());
        config[12..16].copy_from_slice(&((queue_size - 2) as u32).to_le_bytes());
        config[20..24].copy_from_slice(&BLOCK_SIZE.to_le_bytes());
        config[28..32].copy_from_slice(&1u32.to_le_bytes());
        config[CONFIG_WCE_OFFSET] = 1;
        config[34..36].copy_from_slice(&(queues as u16).to_le_bytes());

        let workers = (0..queues)
            .map(|queue| {
                Ok(Mutex::new(QueueWorker::new(
                    stage.clone(),
                    queue,
                    queue_size as u32,
                    queue_cpus.get(queue).copied(),
                    true,
                    serial,
                    writeback.clone(),
                )?))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let queue_masks = (0..queues).map(|queue| 1u64 << queue).collect();

        Ok(Self {
            workers,
            config: RwLock::new(config),
            read_only,
            queue_size,
            queue_masks,
            memory,
            acknowledged_features: AtomicU64::new(0),
            writeback,
            poll_for,
            offer_event_idx,
        })
    }

    fn stats(&self) -> Stats {
        self.workers
            .iter()
            .fold(Stats::default(), |mut total, worker| {
                if let Ok(worker) = worker.lock() {
                    total.reads += worker.stats.reads;
                    total.writes += worker.stats.writes;
                    total.flushes += worker.stats.flushes;
                    total.get_ids += worker.stats.get_ids;
                    total.unsupported += worker.stats.unsupported;
                    total.io_errors += worker.stats.io_errors;
                    total.read_bytes += worker.stats.read_bytes;
                    total.write_bytes += worker.stats.write_bytes;
                }
                total
            })
    }

    fn queue_stats(&self) -> Vec<Stats> {
        self.workers
            .iter()
            .map(|worker| worker.lock().map(|worker| worker.stats).unwrap_or_default())
            .collect()
    }

    fn arena_completion_stats(&self) -> ArenaCompletionStats {
        self.workers
            .iter()
            .fold(ArenaCompletionStats::default(), |mut total, worker| {
                if let Ok(worker) = worker.lock()
                    && let Some(arena) = worker.arena_queue.as_ref()
                {
                    total.submit_calls += arena.completion_stats.submit_calls;
                    total.submitted_requests += arena.completion_stats.submitted_requests;
                    total.submit_batch_max = total
                        .submit_batch_max
                        .max(arena.completion_stats.submit_batch_max);
                    total.outstanding_max = total
                        .outstanding_max
                        .max(arena.completion_stats.outstanding_max);
                    total.wait_calls += arena.completion_stats.wait_calls;
                    total.wait_cqes += arena.completion_stats.wait_cqes;
                    total.wait_batch_max = total
                        .wait_batch_max
                        .max(arena.completion_stats.wait_batch_max);
                    total.wait_cq_head_publications +=
                        arena.completion_stats.wait_cq_head_publications;
                    total.ready_batch_publications +=
                        arena.completion_stats.ready_batch_publications;
                    total.ready_batched_cqes += arena.completion_stats.ready_batched_cqes;
                    total.ready_batch_max = total
                        .ready_batch_max
                        .max(arena.completion_stats.ready_batch_max);
                    total.direct_used_entries += arena.completion_stats.direct_used_entries;
                    total.direct_avail_entries += arena.completion_stats.direct_avail_entries;
                    total.completion_event_wakes += arena.completion_stats.completion_event_wakes;
                    total.completion_event_cqes += arena.completion_stats.completion_event_cqes;
                    total.completion_event_arms += arena.completion_stats.completion_event_arms;
                    total.completion_event_disarms +=
                        arena.completion_stats.completion_event_disarms;
                    total.completion_event_task_runs +=
                        arena.completion_stats.completion_event_task_runs;
                    total.forced_idle_notifications +=
                        arena.completion_stats.forced_idle_notifications;
                }
                total
            })
    }

    fn direct_completion_stats(&self) -> DirectCompletionStats {
        self.workers
            .iter()
            .fold(DirectCompletionStats::default(), |mut total, worker| {
                if let Ok(worker) = worker.lock()
                    && let Some(direct) = worker.direct_queue.as_ref()
                {
                    total.posts += direct.stats.posts;
                    total.post_eagain += direct.stats.post_eagain;
                    total.cq_polls += direct.stats.cq_polls;
                    total.cq_completions += direct.stats.cq_completions;
                    total.cq_batch_max = total.cq_batch_max.max(direct.stats.cq_batch_max);
                    total.outstanding_max = total.outstanding_max.max(direct.stats.outstanding_max);
                    total.direct_avail_entries += direct.stats.direct_avail_entries;
                    total.direct_used_entries += direct.stats.direct_used_entries;
                }
                total
            })
    }

    fn arena_completion_listeners(&self) -> Vec<(usize, i32)> {
        self.workers
            .iter()
            .enumerate()
            .filter_map(|(queue, worker)| {
                let worker = worker.lock().ok()?;
                let arena = worker.arena_queue.as_ref()?;
                Some((queue, arena.completion_event_fd()))
            })
            .collect()
    }

    fn update_writeback(&self) {
        let config_wce = 1u64 << VIRTIO_BLK_F_CONFIG_WCE;
        let flush = 1u64 << VIRTIO_BLK_F_FLUSH;
        let acknowledged_features = self.acknowledged_features.load(Ordering::Acquire);
        let enabled = if acknowledged_features & config_wce != 0 {
            self.config
                .read()
                .map(|config| config[CONFIG_WCE_OFFSET] != 0)
                .unwrap_or(true)
        } else {
            acknowledged_features & flush != 0
        };
        self.writeback.store(enabled, Ordering::Release);
    }
}

impl VhostUserBackend for Backend {
    type Bitmap = ();
    type Vring = VringRwLock<AtomicGuestMemory>;

    fn num_queues(&self) -> usize {
        self.workers.len()
    }

    fn max_queue_size(&self) -> usize {
        self.queue_size
    }

    fn features(&self) -> u64 {
        let mut features = (1u64 << VIRTIO_BLK_F_SEG_MAX)
            | (1u64 << VIRTIO_BLK_F_BLK_SIZE)
            | (1u64 << VIRTIO_BLK_F_FLUSH)
            | (1u64 << VIRTIO_BLK_F_MQ)
            | (1u64 << VIRTIO_BLK_F_CONFIG_WCE)
            | (1u64 << VIRTIO_F_VERSION_1)
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();
        if self.offer_event_idx {
            features |= 1u64 << VIRTIO_RING_F_EVENT_IDX;
        }
        if self.read_only {
            features |= 1u64 << VIRTIO_BLK_F_RO;
        }
        features
    }

    fn acked_features(&self, features: u64) {
        self.acknowledged_features
            .store(features, Ordering::Release);
        self.update_writeback();
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::CONFIG
            | VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
    }

    fn set_event_idx(&self, enabled: bool) {
        for worker in &self.workers {
            worker.lock().unwrap().event_idx = enabled;
        }
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        let start = offset as usize;
        let Some(end) = start.checked_add(size as usize) else {
            return Vec::new();
        };
        self.config
            .read()
            .ok()
            .and_then(|config| config.get(start..end).map(<[_]>::to_vec))
            .unwrap_or_default()
    }

    fn set_config(&self, offset: u32, data: &[u8]) -> io::Result<()> {
        let start = offset as usize;
        let end = start
            .checked_add(data.len())
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        {
            let mut config = self
                .config
                .write()
                .map_err(|_| io::Error::other("config lock poisoned"))?;
            let destination = config
                .get_mut(start..end)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
            destination.copy_from_slice(data);
        }
        self.update_writeback();
        Ok(())
    }

    fn queues_per_thread(&self) -> Vec<u64> {
        self.queue_masks.clone()
    }

    fn exit_event(&self, thread_index: usize) -> Option<(EventConsumer, EventNotifier)> {
        let worker = self.workers.get(thread_index)?.lock().ok()?;
        let consumer = worker.exit_event.try_clone().ok()?;
        let notifier = worker.exit_event.try_clone().ok()?;
        // SAFETY: both raw descriptors came from owned EventFd clones and are
        // transferred exactly once into the event wrapper types.
        unsafe {
            Some((
                EventConsumer::from_raw_fd(consumer.into_raw_fd()),
                EventNotifier::from_raw_fd(notifier.into_raw_fd()),
            ))
        }
    }

    fn update_memory(&self, _memory: AtomicGuestMemory) -> io::Result<()> {
        // The backend and vrings own clones of this GuestMemoryAtomic; memory
        // table replacement updates the common ArcSwap observed by both.
        Ok(())
    }

    fn handle_event(
        &self,
        device_event: u16,
        event_set: EventSet,
        vrings: &[VringRwLock<AtomicGuestMemory>],
        thread_id: usize,
    ) -> io::Result<()> {
        let completion_event =
            usize::from(device_event) == usize::from(ARENA_CQ_EVENT_BASE).saturating_add(thread_id);
        if event_set != EventSet::IN
            || (device_event != 0 && !completion_event)
            || vrings.len() != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unexpected vhost event",
            ));
        }
        let mut worker = self
            .workers
            .get(thread_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid queue worker"))?
            .lock()
            .map_err(|_| io::Error::other("queue worker lock poisoned"))?;
        if !worker.pinned {
            if let Some(cpu) = worker.cpu {
                pin_current_thread(cpu)?;
            }
            worker.start_arena().map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("queue {} initialize arena ring: {error}", worker.queue),
                )
            })?;
            worker.pinned = true;
        }
        if worker.completion_event_enabled {
            // An active lane suppresses eventfd signaling. Clear the event
            // which brought us here, then run deferred task work before
            // sampling the CQ. This ordering prevents a task-work wake from
            // being consumed before its CQE becomes visible.
            worker.set_arena_completion_event_armed(false);
            worker.consume_arena_completion_event().map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("queue {} consume arena CQ event: {error}", worker.queue),
                )
            })?;
            worker.run_arena_task_work().map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("queue {} run arena task work: {error}", worker.queue),
                )
            })?;
        }
        let memory = self.memory.memory();
        let mut vring = vrings[0].get_mut();

        if worker.event_idx {
            loop {
                // EVENT_IDX rearming must bracket queue draining.  Enabling
                // notifications without first disabling them can leave the
                // driver's avail_event unchanged after the initial depth is
                // consumed, permanently suppressing the next kick.
                vring
                    .get_queue_mut()
                    .disable_notification(memory.deref())
                    .map_err(io::Error::other)?;
                if let Err(error) = worker.process_queue(&mut vring, &memory) {
                    eprintln!(
                        "zcvhost-user-blk: queue={} phase=event-idx-drain error={error}",
                        worker.queue
                    );
                    return Err(error);
                }
                if !self.poll_for.is_zero() {
                    if let Err(error) = worker.poll_queue(&mut vring, self.poll_for, &memory) {
                        eprintln!(
                            "zcvhost-user-blk: queue={} phase=event-idx-poll error={error}",
                            worker.queue
                        );
                        return Err(error);
                    }
                }
                if !vring
                    .get_queue_mut()
                    .enable_notification(memory.deref())
                    .map_err(io::Error::other)?
                {
                    break;
                }
            }
        } else {
            if let Err(error) = worker.process_queue(&mut vring, &memory) {
                eprintln!(
                    "zcvhost-user-blk: queue={} phase=drain error={error}",
                    worker.queue
                );
                return Err(error);
            }
            if !self.poll_for.is_zero() {
                if let Err(error) = worker.poll_queue(&mut vring, self.poll_for, &memory) {
                    eprintln!(
                        "zcvhost-user-blk: queue={} phase=poll error={error}",
                        worker.queue
                    );
                    return Err(error);
                }
            }
        }
        if worker.completion_event_enabled {
            loop {
                if worker.arena_outstanding() == 0 {
                    worker.consume_arena_completion_event()?;
                    break;
                }

                // Race-free idle transition:
                //  1. discard signals for CQEs already drained while armed=0;
                //  2. arm kernel signaling;
                //  3. run deferred task work;
                //  4. sample/drain the CQ.
                // If step 4 finds nothing, every later completion observes
                // the armed flag. If it finds work, disarm and repeat.
                worker.consume_arena_completion_event()?;
                worker.set_arena_completion_event_armed(true);
                fence(Ordering::SeqCst);
                worker.run_arena_task_work()?;
                if !worker.reap_arena_ready(&mut vring)? {
                    break;
                }
                worker.set_arena_completion_event_armed(false);
            }
        }
        Ok(())
    }
}

struct Options {
    socket: PathBuf,
    leaf: Option<PathBuf>,
    arena_socket: Option<PathBuf>,
    direct_ofi_address: Option<String>,
    direct_ofi_provider: String,
    direct_ofi_endpoint: String,
    direct_ofi_domain: Option<String>,
    direct_ofi_base_service: u16,
    direct_capacity_bytes: Option<u64>,
    direct_slot_bytes: usize,
    direct_require_hugetlb: bool,
    zcnblk_device: PathBuf,
    queues: usize,
    queue_size: usize,
    queue_cpus: Vec<usize>,
    read_only: bool,
    poll_us: u64,
    offer_event_idx: bool,
    serial: [u8; SERIAL_LEN],
}

fn usage() -> &'static str {
    "usage: zcvhost-user-blk --socket PATH (--leaf-file PATH | --arena-socket PATH | --direct-ofi ADDRESS) \
     [--zcnblk-device PATH] [--direct-provider NAME] [--direct-endpoint rdm] \
     [--direct-domain NAME] [--direct-base-service PORT] [--direct-capacity-bytes N] \
     [--direct-slot-bytes N] [--direct-require-hugetlb] [--queues N] \
     [--queue-size N] [--queue-cpus CSV] [--poll-us N] [--no-event-idx] [--read-only] [--serial TEXT]\n\
     A leaf file is terminal test media. An arena socket connects to an existing\n\
     userspace stage through /dev/zcnblk0 without a block-edge payload copy.\n\
     Direct OFI posts registered-memory RMA operations to a remote userspace\n\
     volume with no block edge and no per-I/O system calls.\n\
     This adapter never performs placement."
}

fn parse_usize(name: &str, value: Option<String>) -> io::Result<usize> {
    value
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("missing {name}")))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid {name}")))
}

fn parse_options() -> io::Result<Options> {
    let mut arguments = env::args().skip(1);
    let mut socket = None;
    let mut leaf = None;
    let mut arena_socket = None;
    let mut direct_ofi_address = None;
    let mut direct_ofi_provider = "sockets".to_string();
    let mut direct_ofi_endpoint = "rdm".to_string();
    let mut direct_ofi_domain = None;
    let mut direct_ofi_base_service = 37_000u16;
    let mut direct_capacity_bytes = None;
    let mut direct_slot_bytes = 4096usize;
    let mut direct_require_hugetlb = false;
    let mut zcnblk_device = PathBuf::from("/dev/zcnblk0");
    let mut queues = 1usize;
    let mut queue_size = 256usize;
    let mut queue_cpus = Vec::new();
    let mut read_only = false;
    let mut poll_us = 0u64;
    let mut offer_event_idx = true;
    let mut serial = [b' '; SERIAL_LEN];
    serial[..14].copy_from_slice(b"zcutils-vhost0");

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--socket" => socket = arguments.next().map(PathBuf::from),
            "--leaf-file" => leaf = arguments.next().map(PathBuf::from),
            "--arena-socket" => arena_socket = arguments.next().map(PathBuf::from),
            "--direct-ofi" => direct_ofi_address = arguments.next(),
            "--direct-provider" => {
                direct_ofi_provider = arguments.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing --direct-provider")
                })?;
            }
            "--direct-endpoint" => {
                direct_ofi_endpoint = arguments.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing --direct-endpoint")
                })?;
            }
            "--direct-domain" => direct_ofi_domain = arguments.next(),
            "--direct-base-service" => {
                direct_ofi_base_service =
                    u16::try_from(parse_usize("--direct-base-service", arguments.next())?)
                        .map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "--direct-base-service exceeds u16",
                            )
                        })?;
            }
            "--direct-capacity-bytes" => {
                direct_capacity_bytes = Some(
                    arguments
                        .next()
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "missing --direct-capacity-bytes",
                            )
                        })?
                        .parse::<u64>()
                        .map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "invalid --direct-capacity-bytes",
                            )
                        })?,
                );
            }
            "--direct-slot-bytes" => {
                direct_slot_bytes = parse_usize("--direct-slot-bytes", arguments.next())?
            }
            "--direct-require-hugetlb" => direct_require_hugetlb = true,
            "--zcnblk-device" => {
                zcnblk_device = PathBuf::from(arguments.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing --zcnblk-device")
                })?);
            }
            "--queues" => queues = parse_usize("--queues", arguments.next())?,
            "--queue-size" => queue_size = parse_usize("--queue-size", arguments.next())?,
            "--queue-cpus" => {
                let value = arguments.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing --queue-cpus")
                })?;
                queue_cpus = value
                    .split(',')
                    .map(|cpu| {
                        cpu.parse::<usize>().map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidInput, "invalid --queue-cpus")
                        })
                    })
                    .collect::<io::Result<Vec<_>>>()?;
            }
            "--poll-us" => {
                poll_us = parse_usize("--poll-us", arguments.next())? as u64;
            }
            "--no-event-idx" => offer_event_idx = false,
            "--read-only" => read_only = true,
            "--serial" => {
                let value = arguments.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing --serial")
                })?;
                if value.len() > SERIAL_LEN || !value.is_ascii() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--serial must contain at most 20 ASCII bytes",
                    ));
                }
                serial = [b' '; SERIAL_LEN];
                serial[..value.len()].copy_from_slice(value.as_bytes());
            }
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown option {argument}\n{}", usage()),
                ));
            }
        }
    }

    if !(1..=MAX_QUEUES).contains(&queues) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--queues must be in 1..={MAX_QUEUES}"),
        ));
    }
    if !(16..=32768).contains(&queue_size) || !queue_size.is_power_of_two() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--queue-size must be a power of two in 16..=32768",
        ));
    }
    if usize::from(leaf.is_some())
        + usize::from(arena_socket.is_some())
        + usize::from(direct_ofi_address.is_some())
        != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "choose exactly one of --leaf-file, --arena-socket, or --direct-ofi\n{}",
                usage()
            ),
        ));
    }
    if direct_ofi_address.is_some() && direct_capacity_bytes.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--direct-ofi requires --direct-capacity-bytes",
        ));
    }
    if !queue_cpus.is_empty() && queue_cpus.len() != queues {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--queue-cpus must contain exactly one CPU per queue",
        ));
    }

    Ok(Options {
        socket: socket.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("missing --socket\n{}", usage()),
            )
        })?,
        leaf,
        arena_socket,
        direct_ofi_address,
        direct_ofi_provider,
        direct_ofi_endpoint,
        direct_ofi_domain,
        direct_ofi_base_service,
        direct_capacity_bytes,
        direct_slot_bytes,
        direct_require_hugetlb,
        zcnblk_device,
        queues,
        queue_size,
        queue_cpus,
        read_only,
        poll_us,
        offer_event_idx,
        serial,
    })
}

fn run() -> io::Result<()> {
    let options = parse_options()?;
    if options.socket.exists() {
        std::fs::remove_file(&options.socket)?;
    }
    let performance_mode = options.arena_socket.is_some() || options.direct_ofi_address.is_some();
    let strict = env::var("URING_PLAY_TOPOLOGY_STRICT").as_deref() == Ok("1")
        || env::var("URING_PLAY_TOPOLOGY_FATAL").as_deref() == Ok("1");
    if performance_mode && options.queue_cpus.is_empty() {
        eprintln!(
            "zcvhost-user-blk: PERFORMANCE WARNING: userspace transport mode lacks --queue-cpus; lane-to-worker/NIC affinity is unproven"
        );
        if strict {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "strict topology requires --queue-cpus in userspace transport mode",
            ));
        }
    }
    if options.direct_ofi_address.is_some() && !options.direct_require_hugetlb {
        eprintln!(
            "zcvhost-user-blk: PERFORMANCE WARNING: direct OFI does not require HugeTLB; registered-memory TLB locality is not guaranteed"
        );
        if strict {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "strict direct-OFI topology requires --direct-require-hugetlb",
            ));
        }
    }
    let (stage, backing): (Arc<dyn BlockStage>, String) = if let Some(path) = &options.leaf {
        (
            Arc::new(FileLeaf::open(path, options.read_only)?),
            format!("terminal_leaf={}", path.display()),
        )
    } else if let Some(socket) = options.arena_socket.as_ref() {
        (
            Arc::new(ArenaStage::open(
                socket,
                &options.zcnblk_device,
                options.read_only,
            )?),
            format!(
                "arena_socket={} zcnblk_edge={} zcnblk_noatime=true guest_to_arena_payload_copies=1 block_edge_payload_copies=0",
                socket.display(),
                options.zcnblk_device.display()
            ),
        )
    } else {
        let address = options.direct_ofi_address.as_ref().unwrap();
        let capacity_bytes = options.direct_capacity_bytes.unwrap();
        (
            Arc::new(DirectOfiStage::new(
                VhostOfiClientConfig {
                    provider: options.direct_ofi_provider.clone(),
                    endpoint: options.direct_ofi_endpoint.clone(),
                    address: address.clone(),
                    domain: options.direct_ofi_domain.clone(),
                    base_service: options.direct_ofi_base_service,
                    lanes: options.queues,
                    capacity_bytes,
                    require_hugetlb: options.direct_require_hugetlb,
                },
                options.direct_slot_bytes,
                options.read_only,
            )?),
            format!(
                "direct_ofi={} provider={} endpoint={} domain={} base_service={} registered_slot_bytes={} kernel_block_edge=no data_path_syscalls={} guest_to_registered_payload_copies=1",
                address,
                options.direct_ofi_provider,
                options.direct_ofi_endpoint,
                options.direct_ofi_domain.as_deref().unwrap_or("auto"),
                options.direct_ofi_base_service,
                options.direct_slot_bytes,
                zcutils::vhost_ofi::provider_data_path_syscalls(&options.direct_ofi_provider),
            ),
        )
    };
    let capacity = stage.capacity_bytes();
    let memory = GuestMemoryAtomic::new(GuestMemoryMmap::new());
    let backend = Arc::new(Backend::new(
        stage.clone(),
        memory.clone(),
        options.queues,
        options.queue_size,
        &options.queue_cpus,
        options.read_only,
        Duration::from_micros(options.poll_us),
        options.offer_event_idx,
        options.serial,
    )?);
    let mut daemon =
        VhostUserDaemon::new("zcvhost-user-blk".to_owned(), backend.clone(), memory)
            .map_err(|error| io::Error::other(format!("create vhost daemon: {error:?}")))?;
    let epoll_handlers = daemon.get_epoll_handlers();
    for (queue, eventfd) in backend.arena_completion_listeners() {
        let handler = epoll_handlers.get(queue).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("arena queue {queue} has no matching vhost epoll worker"),
            )
        })?;
        let event_id = u64::from(ARENA_CQ_EVENT_BASE)
            .checked_add(queue as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "CQ event id overflow"))?;
        handler
            .register_listener(eventfd, EventSet::IN, event_id)
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("register arena queue {queue} CQ listener: {error}"),
                )
            })?;
    }

    eprintln!(
        "zcvhost-user-blk: ready socket={} {} capacity_bytes={} queues={} queue_size={} queue_cpus={} poll_us={} cq_wakeup={} event_idx={} placement_owner=downstream-userspace-stage frontend_placement=no",
        options.socket.display(),
        backing,
        capacity,
        options.queues,
        options.queue_size,
        if options.queue_cpus.is_empty() {
            "unpinned".to_owned()
        } else {
            options
                .queue_cpus
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(",")
        },
        options.poll_us,
        if options.direct_ofi_address.is_some() {
            "libfabric-cq-poll"
        } else if options.poll_us == 0 && options.arena_socket.is_some() {
            "io_uring-eventfd"
        } else if options.arena_socket.is_some() {
            "bounded-poll+io_uring-eventfd-idle"
        } else if options.poll_us == 0 {
            "virtqueue-kick"
        } else {
            "bounded-poll"
        },
        options.offer_event_idx,
    );
    daemon
        .serve(&options.socket)
        .map_err(|error| io::Error::other(format!("serve vhost socket: {error:?}")))?;
    stage.shutdown()?;
    let stats = backend.stats();
    let arena_completion_stats = backend.arena_completion_stats();
    let direct_completion_stats = backend.direct_completion_stats();
    eprintln!(
        "zcvhost-user-blk-summary: reads={} writes={} flushes={} get_ids={} unsupported={} io_errors={} read_bytes={} write_bytes={} arena_submit_calls={} arena_submitted_requests={} arena_submit_batch_max={} arena_outstanding_max={} arena_wait_calls={} arena_wait_cqes={} arena_wait_batch_max={} arena_wait_cq_head_publications={} arena_ready_batch_publications={} arena_ready_batched_cqes={} arena_ready_batch_max={} arena_cq_head_publications={} arena_direct_used_entries={} arena_direct_avail_entries={} arena_completion_event_wakes={} arena_completion_event_cqes={} arena_completion_event_arms={} arena_completion_event_disarms={} arena_completion_event_task_runs={} forced_idle_notifications={} direct_posts={} direct_post_eagain={} direct_cq_polls={} direct_cq_completions={} direct_cq_batch_max={} direct_outstanding_max={}",
        stats.reads,
        stats.writes,
        stats.flushes,
        stats.get_ids,
        stats.unsupported,
        stats.io_errors,
        stats.read_bytes,
        stats.write_bytes,
        arena_completion_stats.submit_calls,
        arena_completion_stats.submitted_requests,
        arena_completion_stats.submit_batch_max,
        arena_completion_stats.outstanding_max,
        arena_completion_stats.wait_calls,
        arena_completion_stats.wait_cqes,
        arena_completion_stats.wait_batch_max,
        arena_completion_stats.wait_cq_head_publications,
        arena_completion_stats.ready_batch_publications,
        arena_completion_stats.ready_batched_cqes,
        arena_completion_stats.ready_batch_max,
        arena_completion_stats.wait_cq_head_publications
            + arena_completion_stats.ready_batch_publications,
        arena_completion_stats.direct_used_entries,
        arena_completion_stats.direct_avail_entries,
        arena_completion_stats.completion_event_wakes,
        arena_completion_stats.completion_event_cqes,
        arena_completion_stats.completion_event_arms,
        arena_completion_stats.completion_event_disarms,
        arena_completion_stats.completion_event_task_runs,
        arena_completion_stats.forced_idle_notifications,
        direct_completion_stats.posts,
        direct_completion_stats.post_eagain,
        direct_completion_stats.cq_polls,
        direct_completion_stats.cq_completions,
        direct_completion_stats.cq_batch_max,
        direct_completion_stats.outstanding_max,
    );
    if env::var("ZCVHOST_REPORT_QUEUE_STATS").as_deref() == Ok("1") {
        for (queue, stats) in backend.queue_stats().into_iter().enumerate() {
            eprintln!(
                "zcvhost-user-blk-queue-summary: queue={} reads={} writes={} flushes={} io_errors={} read_bytes={} write_bytes={}",
                queue,
                stats.reads,
                stats.writes,
                stats.flushes,
                stats.io_errors,
                stats.read_bytes,
                stats.write_bytes,
            );
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("zcvhost-user-blk: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_leaf() -> PathBuf {
        env::temp_dir().join(format!(
            "zcvhost-user-blk-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn event_index_interval_handles_batches_and_u16_wraparound() {
        assert!(virtio_need_event(3, 4, 3));
        assert!(!virtio_need_event(4, 4, 3));
        assert!(virtio_need_event(6, 8, 5));
        assert!(!virtio_need_event(8, 8, 5));
        assert!(virtio_need_event(0, 1, u16::MAX - 1));
        assert!(!virtio_need_event(1, 1, u16::MAX - 1));
        assert!(!virtio_need_event(8, 8, 8));
    }

    #[test]
    fn header_parser_uses_little_endian_sector_and_ignores_legacy_barrier() {
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&(VIRTIO_BLK_T_OUT | VIRTIO_BLK_T_BARRIER).to_le_bytes());
        bytes[8..16].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        let header = parse_header(&bytes);
        assert_eq!(header.kind, RequestKind::Write);
        assert_eq!(header.sector, 0x0102_0304_0506_0708);
    }

    #[test]
    fn terminal_file_leaf_is_bounded_and_persistent() {
        let path = temporary_leaf();
        let file = File::create(&path).unwrap();
        file.set_len(4096).unwrap();
        drop(file);

        let leaf = FileLeaf::open(&path, false).unwrap();
        let payload = [0xa5u8; 512];
        leaf.write_at(0, 1024, &payload).unwrap();
        leaf.flush().unwrap();
        let mut result = [0u8; 512];
        leaf.read_at(0, 1024, &mut result).unwrap();
        assert_eq!(result, payload);
        assert!(leaf.write_at(0, 4096, &[1]).is_err());
        drop(leaf);

        let read_only = FileLeaf::open(&path, true).unwrap();
        assert!(read_only.write_at(0, 0, &[1]).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn config_reports_sector_capacity_queue_count_and_writeback() {
        let path = temporary_leaf();
        let file = File::create(&path).unwrap();
        file.set_len(1024 * 1024).unwrap();
        drop(file);
        let leaf = Arc::new(FileLeaf::open(&path, false).unwrap());
        let memory = GuestMemoryAtomic::new(GuestMemoryMmap::new());
        let backend = Backend::new(
            leaf,
            memory,
            4,
            256,
            &[],
            false,
            Duration::ZERO,
            true,
            [b'x'; SERIAL_LEN],
        )
        .unwrap();
        let config = backend.config.read().unwrap();
        assert_eq!(u64::from_le_bytes(config[0..8].try_into().unwrap()), 2048);
        assert_eq!(u32::from_le_bytes(config[20..24].try_into().unwrap()), 512);
        assert_eq!(u16::from_le_bytes(config[34..36].try_into().unwrap()), 4);
        assert_eq!(config[CONFIG_WCE_OFFSET], 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn guest_region_fast_path_is_bounded_and_refreshes_with_the_memory_table() {
        let path = temporary_leaf();
        let file = File::create(&path).unwrap();
        file.set_len(1024 * 1024).unwrap();
        drop(file);
        let leaf = Arc::new(FileLeaf::open(&path, false).unwrap());
        let atomic_memory = GuestMemoryAtomic::new(GuestMemoryMmap::new());
        let backend = Backend::new(
            leaf,
            atomic_memory,
            1,
            256,
            &[],
            false,
            Duration::ZERO,
            true,
            [b'x'; SERIAL_LEN],
        )
        .unwrap();
        let mut worker = backend.workers[0].lock().unwrap();

        let first = GuestMemoryMmap::from_ranges(&[
            (GuestAddress(0x1000), 0x2000),
            (GuestAddress(0x8000), 0x1000),
        ])
        .unwrap();
        let descriptors = [
            virtio_queue::desc::split::Descriptor::new(0x1800, 16, VRING_DESC_F_NEXT as u16, 1),
            virtio_queue::desc::split::Descriptor::new(
                0x1900,
                4096,
                (VRING_DESC_F_NEXT | VRING_DESC_F_WRITE) as u16,
                2,
            ),
            virtio_queue::desc::split::Descriptor::new(0x1a00, 1, VRING_DESC_F_WRITE as u16, 0),
        ];
        for (index, descriptor) in descriptors.into_iter().enumerate() {
            first
                .write_obj(descriptor, GuestAddress(0x1000 + index as u64 * 16))
                .unwrap();
        }
        first.write_obj(0x5au8, GuestAddress(0x1800)).unwrap();
        let first_atomic = GuestMemoryAtomic::new(first);
        let first_memory = first_atomic.memory();
        worker.refresh_guest_regions(&first_memory).unwrap();
        let loaded = worker.load_three_descriptors_fast(0x1000, 4, 0).unwrap();
        assert_eq!(loaded[1].length, 4096);
        assert!(loaded[1].writable);
        let first_host = worker.guest_host_address(GuestAddress(0x1800), 1).unwrap();
        // SAFETY: guest_host_address range-checked this byte in `first`.
        assert_eq!(
            unsafe { std::ptr::read_volatile(first_host as *const u8) },
            0x5a
        );
        assert!(worker.guest_host_address(GuestAddress(0x2fff), 2).is_none());
        assert!(worker.guest_host_address(GuestAddress(0x7000), 1).is_none());

        let second = GuestMemoryMmap::from_ranges(&[(GuestAddress(0x1000), 0x2000)]).unwrap();
        second.write_obj(0xa5u8, GuestAddress(0x1800)).unwrap();
        let second_atomic = GuestMemoryAtomic::new(second);
        let second_memory = second_atomic.memory();
        worker.refresh_guest_regions(&second_memory).unwrap();
        let second_host = worker.guest_host_address(GuestAddress(0x1800), 1).unwrap();
        assert_ne!(first_host, second_host);
        // SAFETY: guest_host_address range-checked this byte in `second`.
        assert_eq!(
            unsafe { std::ptr::read_volatile(second_host as *const u8) },
            0xa5
        );

        drop(worker);
        drop(backend);
        std::fs::remove_file(path).unwrap();
    }
}
