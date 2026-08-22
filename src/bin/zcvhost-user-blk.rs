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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use virtio_queue::{DescriptorChain, QueueT};
use vm_memory::{
    Bytes, GuestAddress, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryBackend,
    GuestMemoryLoadGuard, ReadVolatile, VolatileSlice, WriteVolatile,
};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::event::{EventConsumer, EventNotifier};
use vmm_sys_util::eventfd::EventFd;
use zcutils::zcnblk_app_arena::{ZcnblkAppArena, pin_current_thread};

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
}

impl ArenaStage {
    fn open(socket: &Path, device: &Path, read_only: bool) -> io::Result<Self> {
        let arena = ZcnblkAppArena::connect(socket)?;
        let device = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
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

struct QueueWorker {
    stage: Arc<dyn BlockStage>,
    stats: Stats,
    queue: usize,
    cpu: Option<usize>,
    pinned: bool,
    memory: AtomicGuestMemory,
    serial: [u8; SERIAL_LEN],
    descriptors: Vec<DescriptorMeta>,
    event_idx: bool,
    writeback: Arc<AtomicBool>,
    exit_event: EventFd,
}

impl QueueWorker {
    fn new(
        stage: Arc<dyn BlockStage>,
        queue: usize,
        cpu: Option<usize>,
        memory: AtomicGuestMemory,
        serial: [u8; SERIAL_LEN],
        writeback: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        Ok(Self {
            stage,
            stats: Stats::default(),
            queue,
            cpu,
            pinned: false,
            memory,
            serial,
            descriptors: Vec::with_capacity(32),
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

    fn process_chain(&mut self, chain: &mut LoadedDescriptorChain) -> u32 {
        if !self.load_descriptors(chain) {
            self.stats.io_errors += 1;
            return 0;
        }

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

    fn process_queue(
        &mut self,
        vring: &mut RwLockWriteGuard<VringState<AtomicGuestMemory>>,
    ) -> bool {
        let mut used_any = false;
        while let Some(mut chain) = vring
            .get_queue_mut()
            .pop_descriptor_chain(self.memory.memory())
        {
            let head = chain.head_index();
            let used_len = self.process_chain(&mut chain);
            if let Err(error) = vring
                .get_queue_mut()
                .add_used(chain.memory(), head, used_len)
            {
                eprintln!("zcvhost-user-blk: add_used failed: {error}");
                break;
            }
            used_any = true;
        }

        if used_any {
            let should_signal = if self.event_idx {
                vring
                    .get_queue_mut()
                    .needs_notification(self.memory.memory().deref())
                    .unwrap_or(true)
            } else {
                true
            };
            if should_signal {
                if let Err(error) = vring.signal_used_queue() {
                    eprintln!("zcvhost-user-blk: failed to signal used queue: {error}");
                }
            }
        }
        used_any
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
                    queue_cpus.get(queue).copied(),
                    memory.clone(),
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
        // Every worker owns a clone of this GuestMemoryAtomic; memory table
        // replacement updates the common ArcSwap observed by all clones.
        Ok(())
    }

    fn handle_event(
        &self,
        device_event: u16,
        event_set: EventSet,
        vrings: &[VringRwLock<AtomicGuestMemory>],
        thread_id: usize,
    ) -> io::Result<()> {
        if event_set != EventSet::IN || device_event != 0 || vrings.len() != 1 {
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
            worker.pinned = true;
        }
        let mut vring = vrings[0].get_mut();

        if worker.event_idx {
            loop {
                // EVENT_IDX rearming must bracket queue draining.  Enabling
                // notifications without first disabling them can leave the
                // driver's avail_event unchanged after the initial depth is
                // consumed, permanently suppressing the next kick.
                vring
                    .get_queue_mut()
                    .disable_notification(self.memory.memory().deref())
                    .map_err(io::Error::other)?;
                worker.process_queue(&mut vring);
                if !self.poll_for.is_zero() {
                    let mut idle_since = Instant::now();
                    loop {
                        if worker.process_queue(&mut vring) {
                            idle_since = Instant::now();
                        } else if idle_since.elapsed() >= self.poll_for {
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
                if !vring
                    .get_queue_mut()
                    .enable_notification(self.memory.memory().deref())
                    .map_err(io::Error::other)?
                {
                    break;
                }
            }
        } else {
            worker.process_queue(&mut vring);
            if !self.poll_for.is_zero() {
                let mut idle_since = Instant::now();
                loop {
                    if worker.process_queue(&mut vring) {
                        idle_since = Instant::now();
                    } else if idle_since.elapsed() >= self.poll_for {
                        break;
                    }
                    std::hint::spin_loop();
                }
            }
        }
        Ok(())
    }
}

struct Options {
    socket: PathBuf,
    leaf: Option<PathBuf>,
    arena_socket: Option<PathBuf>,
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
    "usage: zcvhost-user-blk --socket PATH (--leaf-file PATH | --arena-socket PATH) \
     [--zcnblk-device PATH] [--queues N] \
     [--queue-size N] [--queue-cpus CSV] [--poll-us N] [--no-event-idx] [--read-only] [--serial TEXT]\n\
     A leaf file is terminal test media. An arena socket connects to an existing\n\
     userspace stage through /dev/zcnblk0 without a block-edge payload copy.\n\
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
    if leaf.is_some() == arena_socket.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "choose exactly one of --leaf-file or --arena-socket\n{}",
                usage()
            ),
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
    if options.arena_socket.is_some() && options.queue_cpus.is_empty() {
        eprintln!(
            "zcvhost-user-blk: PERFORMANCE WARNING: arena mode lacks --queue-cpus; lane-to-worker/hctx affinity is unproven"
        );
        if env::var("URING_PLAY_TOPOLOGY_STRICT").as_deref() == Ok("1")
            || env::var("URING_PLAY_TOPOLOGY_FATAL").as_deref() == Ok("1")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "strict topology requires --queue-cpus in arena mode",
            ));
        }
    }
    let (stage, backing): (Arc<dyn BlockStage>, String) = if let Some(path) = &options.leaf {
        (
            Arc::new(FileLeaf::open(path, options.read_only)?),
            format!("terminal_leaf={}", path.display()),
        )
    } else {
        let socket = options.arena_socket.as_ref().unwrap();
        (
            Arc::new(ArenaStage::open(
                socket,
                &options.zcnblk_device,
                options.read_only,
            )?),
            format!(
                "arena_socket={} zcnblk_edge={} guest_to_arena_payload_copies=1 block_edge_payload_copies=0",
                socket.display(),
                options.zcnblk_device.display()
            ),
        )
    };
    let capacity = stage.capacity_bytes();
    let memory = GuestMemoryAtomic::new(GuestMemoryMmap::new());
    let backend = Arc::new(Backend::new(
        stage,
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

    eprintln!(
        "zcvhost-user-blk: ready socket={} {} capacity_bytes={} queues={} queue_size={} queue_cpus={} poll_us={} event_idx={} placement_owner=downstream-userspace-stage frontend_placement=no",
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
        options.offer_event_idx,
    );
    daemon
        .serve(&options.socket)
        .map_err(|error| io::Error::other(format!("serve vhost socket: {error:?}")))?;
    let stats = backend.stats();
    eprintln!(
        "zcvhost-user-blk-summary: reads={} writes={} flushes={} get_ids={} unsupported={} io_errors={} read_bytes={} write_bytes={}",
        stats.reads,
        stats.writes,
        stats.flushes,
        stats.get_ids,
        stats.unsupported,
        stats.io_errors,
        stats.read_bytes,
        stats.write_bytes,
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
}
