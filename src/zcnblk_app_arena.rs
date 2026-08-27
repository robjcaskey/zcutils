//! Application-owned buffers in the imported zcnblk HugeTLB payload arena.
//!
//! A buffer is reserved in userspace, submitted through `/dev/zcnblk0`, and
//! handed to the kernel without a payload copy. Applications must submit on
//! the CPU/hctx mapped to the selected lane.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::RawRing;

pub const ZCNBLK_APP_ARENA_MAGIC: u64 = 0x3141_5041_434e_435a; // "ZCNCAPA1"
pub const ZCNBLK_APP_ARENA_VERSION: u32 = 1;
pub const ZCNBLK_APP_ARENA_F_EXTERNAL_HUGETLB: u32 = 1 << 0;
pub const ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED: u64 = u64::MAX - 1;
const ZCNBLK_SHM_CHANNEL_BYTES: usize = 320;
const ZCNBLK_SHM_CHANNEL_PAYLOAD_FREE_SLOTS: usize = 256;
const LOCAL_SLOT_FREE: u8 = 0;
const LOCAL_SLOT_LIVE: u8 = 1;
const LOCAL_SLOT_ORPHAN: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ZcnblkAppArenaDescriptor {
    pub magic: u64,
    pub version: u32,
    pub descriptor_bytes: u32,
    pub flags: u32,
    pub channels: u32,
    pub payload_entries: u32,
    pub slot_bytes: u32,
    pub channel_bytes: u32,
    pub payload_free_slots_offset: u32,
    pub reserved: u32,
    pub reserved2: u32,
    pub channel_offset: u64,
    pub payload_owner_offset: u64,
    pub payload_offset: u64,
    pub region_bytes: u64,
}

impl ZcnblkAppArenaDescriptor {
    pub(crate) fn validate(self) -> io::Result<Self> {
        if self.magic != ZCNBLK_APP_ARENA_MAGIC
            || self.version != ZCNBLK_APP_ARENA_VERSION
            || self.descriptor_bytes as usize != size_of::<Self>()
            || self.flags & ZCNBLK_APP_ARENA_F_EXTERNAL_HUGETLB == 0
            || self.channels == 0
            || self.payload_entries == 0
            || self.slot_bytes == 0
            || self.channel_bytes as usize != ZCNBLK_SHM_CHANNEL_BYTES
            || self.payload_free_slots_offset as usize != ZCNBLK_SHM_CHANNEL_PAYLOAD_FREE_SLOTS
            || self.reserved != 0
            || self.reserved2 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid zcnblk application arena descriptor",
            ));
        }
        let slots = u64::from(self.channels)
            .checked_mul(u64::from(self.payload_entries))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "slot count overflow"))?;
        let owner_end = self
            .payload_owner_offset
            .checked_add(slots.checked_mul(8).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "owner table overflow")
            })?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "owner end overflow"))?;
        let payload_end = self
            .payload_offset
            .checked_add(
                slots
                    .checked_mul(u64::from(self.slot_bytes))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "payload table overflow")
                    })?,
            )
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "payload end overflow"))?;
        let channel_end = self
            .channel_offset
            .checked_add(u64::from(self.channels) * u64::from(self.channel_bytes))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "channel end overflow"))?;
        if owner_end > self.payload_offset
            || payload_end > self.region_bytes
            || channel_end > self.region_bytes
            || self.payload_owner_offset % 8 != 0
            || self.channel_offset % 8 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "zcnblk application arena layout is out of bounds",
            ));
        }
        Ok(self)
    }
}

struct ArenaInner {
    ptr: *mut u8,
    len: usize,
    descriptor: ZcnblkAppArenaDescriptor,
    // Process-local identity for a live application handle or a completed
    // handed-off handle whose shared APP_RESERVED return is still pending.
    // Keeping both states in one byte avoids a second locked atomic operation
    // on every ordinary allocation.
    local_slots: Vec<AtomicU8>,
    // A write may complete at the block edge while a downstream WAL still
    // owns its payload lease. Once the application handle is retired, record
    // that process-local ownership here so a later target return to
    // APP_RESERVED can be adopted safely. This is deliberately separate from
    // the shared owner token: APP_RESERVED does not identify one of several
    // possible arena-importing processes.
    _fd: OwnedFd,
}

unsafe impl Send for ArenaInner {}
unsafe impl Sync for ArenaInner {}

impl Drop for ArenaInner {
    fn drop(&mut self) {
        self.release_returned_orphans();
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

impl ArenaInner {
    fn global_slot(&self, lane: u32, slot: u32) -> usize {
        lane as usize * self.descriptor.payload_entries as usize + slot as usize
    }

    fn owner(&self, global_slot: usize) -> &AtomicU64 {
        let offset = self.descriptor.payload_owner_offset as usize + global_slot * 8;
        unsafe { &*self.ptr.add(offset).cast::<AtomicU64>() }
    }

    fn free_slots(&self, lane: u32) -> &AtomicU64 {
        let offset = self.descriptor.channel_offset as usize
            + lane as usize * self.descriptor.channel_bytes as usize
            + self.descriptor.payload_free_slots_offset as usize;
        unsafe { &*self.ptr.add(offset).cast::<AtomicU64>() }
    }

    fn reserve(&self, lane: u32, slot: u32) -> bool {
        let global = self.global_slot(lane, slot);
        let local = &self.local_slots[global];
        match local.compare_exchange(
            LOCAL_SLOT_FREE,
            LOCAL_SLOT_LIVE,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => {}
            Err(LOCAL_SLOT_ORPHAN) => {
                let owner = self.owner(global).load(Ordering::Acquire);
                if owner == ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED {
                    if local
                        .compare_exchange(
                            LOCAL_SLOT_ORPHAN,
                            LOCAL_SLOT_LIVE,
                            Ordering::Acquire,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        // This process paid the free-slot debit before the
                        // handoff. Adopt the returned token without changing
                        // the shared count a second time.
                        return true;
                    }
                    return false;
                }
                if owner != 0
                    || local
                        .compare_exchange(
                            LOCAL_SLOT_ORPHAN,
                            LOCAL_SLOT_LIVE,
                            Ordering::Acquire,
                            Ordering::Relaxed,
                        )
                        .is_err()
                {
                    return false;
                }
                // A target may release an abandoned token all the way to
                // zero. Treat it as a fresh reservation with a fresh debit.
            }
            Err(_) => return false,
        }
        if self
            .owner(global)
            .compare_exchange(
                0,
                ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            // If an orphan was released to zero, its former process-local
            // claim is no longer exclusive. A competing importer may have
            // won the shared owner CAS, so discard this local claim.
            local.store(LOCAL_SLOT_FREE, Ordering::Release);
            return false;
        }
        let prior = self.free_slots(lane).fetch_sub(1, Ordering::AcqRel);
        if prior == 0 {
            self.free_slots(lane).fetch_add(1, Ordering::Release);
            self.owner(global).store(0, Ordering::Release);
            local.store(LOCAL_SLOT_FREE, Ordering::Release);
            return false;
        }
        true
    }

    fn release_app_reservation(&self, lane: u32, slot: u32) -> bool {
        let global = self.global_slot(lane, slot);
        if self
            .owner(global)
            .compare_exchange(
                ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.free_slots(lane).fetch_add(1, Ordering::Release);
            true
        } else {
            false
        }
    }

    fn reacquire_existing(&self, lane: u32, slot: u32) -> bool {
        let global = self.global_slot(lane, slot);
        let owner = self.owner(global);
        // Avoid issuing a locked RMW while the kernel or target visibly owns
        // the slot. The release producer needs this cacheline; fighting it
        // with guaranteed-to-fail CAS operations only delays the handoff.
        if owner.load(Ordering::Acquire) != 0 {
            return false;
        }
        if owner
            .compare_exchange(
                0,
                ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        let prior = self.free_slots(lane).fetch_sub(1, Ordering::AcqRel);
        if prior == 0 {
            self.free_slots(lane).fetch_add(1, Ordering::Release);
            self.owner(global).store(0, Ordering::Release);
            return false;
        }
        true
    }

    fn returned_to_app(&self, lane: u32, slot: u32) -> bool {
        self.owner(self.global_slot(lane, slot))
            .load(Ordering::Acquire)
            == ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED
    }

    /// Cold importer-disconnect cleanup. A clean guest shutdown may flush a
    /// target-retained write after its application handle was retired, with no
    /// later allocation to adopt and release the returned token. Reclaim only
    /// slots marked as this process's orphans and already returned by the
    /// target; a downstream-owned lease is deliberately left untouched.
    fn release_returned_orphans(&self) -> u64 {
        let mut released = 0u64;
        for lane in 0..self.descriptor.channels {
            for slot in 0..self.descriptor.payload_entries {
                let global = self.global_slot(lane, slot);
                if self.local_slots[global].load(Ordering::Acquire) != LOCAL_SLOT_ORPHAN {
                    continue;
                }
                if self
                    .owner(global)
                    .compare_exchange(
                        ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED,
                        0,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    self.free_slots(lane).fetch_add(1, Ordering::Release);
                    self.local_slots[global].store(LOCAL_SLOT_FREE, Ordering::Release);
                    released += 1;
                }
            }
        }
        released
    }
}

/// A mapping of the target-exported HugeTLB payload arena.
#[derive(Clone)]
pub struct ZcnblkAppArena {
    inner: Arc<ArenaInner>,
}

impl ZcnblkAppArena {
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut stream = UnixStream::connect(path)?;
        let (descriptor, fd) = receive_descriptor(&mut stream)?;
        Self::from_descriptor(descriptor.validate()?, fd)
    }

    fn from_descriptor(descriptor: ZcnblkAppArenaDescriptor, fd: OwnedFd) -> io::Result<Self> {
        let len = usize::try_from(descriptor.region_bytes).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "arena length exceeds usize")
        })?;
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let slot_count = descriptor.channels as usize * descriptor.payload_entries as usize;
        Ok(Self {
            inner: Arc::new(ArenaInner {
                ptr: ptr.cast(),
                len,
                descriptor,
                local_slots: (0..slot_count)
                    .map(|_| AtomicU8::new(LOCAL_SLOT_FREE))
                    .collect(),
                _fd: fd,
            }),
        })
    }

    pub fn channels(&self) -> u32 {
        self.inner.descriptor.channels
    }

    pub fn slot_bytes(&self) -> usize {
        self.inner.descriptor.slot_bytes as usize
    }

    pub fn slots_per_lane(&self) -> u32 {
        self.inner.descriptor.payload_entries
    }

    /// Reserves any free payload buffer belonging to `lane`.
    pub fn allocate(&self, lane: u32) -> io::Result<ZcnblkAppArenaBuffer> {
        self.allocate_from(lane, 0)
    }

    /// Reserves a lane-local payload buffer, beginning at `start_slot` and
    /// wrapping once. A lane-confined caller can carry the returned slot + 1
    /// as a plain cursor, avoiding repeated probes of its own live buffers
    /// without adding a shared atomic increment to the hot path.
    pub fn allocate_from(&self, lane: u32, start_slot: u32) -> io::Result<ZcnblkAppArenaBuffer> {
        if lane >= self.channels() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "lane out of range",
            ));
        }
        let entries = self.inner.descriptor.payload_entries;
        let start_slot = start_slot % entries;
        for displacement in 0..entries {
            let candidate = u64::from(start_slot) + u64::from(displacement);
            let slot = if candidate >= u64::from(entries) {
                (candidate - u64::from(entries)) as u32
            } else {
                candidate as u32
            };
            if self.inner.reserve(lane, slot) {
                return Ok(ZcnblkAppArenaBuffer {
                    inner: Arc::clone(&self.inner),
                    lane,
                    slot,
                    app_owned: true,
                });
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "no lane-local zcnblk application arena slot is free",
        ))
    }
}

/// A lane-local slot. Its bytes are accessible only while the application owns it.
pub struct ZcnblkAppArenaBuffer {
    inner: Arc<ArenaInner>,
    lane: u32,
    slot: u32,
    app_owned: bool,
}

impl ZcnblkAppArenaBuffer {
    pub fn lane(&self) -> u32 {
        self.lane
    }

    pub fn slot(&self) -> u32 {
        self.slot
    }

    /// Reports whether this handle has already reacquired the application
    /// token and can be retained for lane-local reuse without releasing and
    /// reserving the shared slot again.
    ///
    /// This is a process-local state query. It becomes true only after the
    /// shared owner token was observed or reacquired by the methods on this
    /// handle, and `handoff_to_kernel` clears it before submission.
    pub fn is_application_owned(&self) -> bool {
        self.app_owned
    }

    fn bytes_ptr(&self) -> *mut u8 {
        let global = self.inner.global_slot(self.lane, self.slot);
        let offset = self.inner.descriptor.payload_offset as usize
            + global * self.inner.descriptor.slot_bytes as usize;
        unsafe { self.inner.ptr.add(offset) }
    }

    fn ensure_owned(&self) -> io::Result<()> {
        if self.app_owned
            && self
                .inner
                .owner(self.inner.global_slot(self.lane, self.slot))
                .load(Ordering::Acquire)
                == ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED
        {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "zcnblk arena buffer is owned by the kernel or target",
            ))
        }
    }

    pub fn as_slice(&self) -> io::Result<&[u8]> {
        self.ensure_owned()?;
        Ok(unsafe {
            std::slice::from_raw_parts(self.bytes_ptr(), self.inner.descriptor.slot_bytes as usize)
        })
    }

    pub fn as_mut_slice(&mut self) -> io::Result<&mut [u8]> {
        self.ensure_owned()?;
        Ok(unsafe {
            std::slice::from_raw_parts_mut(
                self.bytes_ptr(),
                self.inner.descriptor.slot_bytes as usize,
            )
        })
    }

    /// Transfer logical access to the kernel before submitting this exact
    /// pointer through an async O_DIRECT interface such as io_uring.
    ///
    /// The returned pointer remains mapped, but it must not be dereferenced
    /// until `wait_reacquire` succeeds. If SQ submission itself fails before
    /// the kernel sees the I/O, call `recover_unsubmitted`.
    pub fn handoff_to_kernel(&mut self) -> io::Result<(*mut u8, usize)> {
        self.ensure_owned()?;
        self.app_owned = false;
        Ok((self.bytes_ptr(), self.inner.descriptor.slot_bytes as usize))
    }

    /// Undo a handoff only if the application token is still untouched.
    pub fn recover_unsubmitted(&mut self) -> io::Result<()> {
        if self.app_owned {
            return Ok(());
        }
        if self
            .inner
            .owner(self.inner.global_slot(self.lane, self.slot))
            .load(Ordering::Acquire)
            == ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED
        {
            self.app_owned = true;
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "kernel has already consumed the zcnblk arena handoff",
            ))
        }
    }

    /// Retires an application handle after its asynchronous block I/O CQE.
    ///
    /// A target that no longer needs the payload returns the shared owner to
    /// `APP_RESERVED`; this method reacquires it so ordinary `Drop` releases
    /// the slot. If a writeback target still owns the lease, `Drop` records the
    /// completed handle as a process-local orphan which `allocate` can adopt
    /// after a later sync. Call this only after the submitted I/O has produced
    /// its CQE.
    pub fn retire_completed_handoff(&mut self) -> io::Result<()> {
        if self.app_owned {
            return Ok(());
        }
        if self.inner.returned_to_app(self.lane, self.slot) {
            self.app_owned = true;
        }
        Ok(())
    }

    /// Submit one slot-sized O_DIRECT write. Call `wait_reacquire` before reuse.
    pub fn write_at(&mut self, file: &File, offset: u64) -> io::Result<()> {
        self.submit_at(file, offset, true)
    }

    /// Submit one slot-sized O_DIRECT read and reacquire the completed buffer.
    pub fn read_at(&mut self, file: &File, offset: u64) -> io::Result<()> {
        self.submit_at(file, offset, false)?;
        self.wait_reacquire(Duration::from_secs(5))
    }

    fn submit_at(&mut self, file: &File, offset: u64, write: bool) -> io::Result<()> {
        let offset = libc::off_t::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset exceeds off_t"))?;
        let (buffer, len) = self.handoff_to_kernel()?;
        let done = unsafe {
            if write {
                libc::pwrite(file.as_raw_fd(), buffer.cast(), len, offset)
            } else {
                libc::pread(file.as_raw_fd(), buffer.cast(), len, offset)
            }
        };
        if done == len as isize {
            return Ok(());
        }
        let _ = self.recover_unsubmitted();
        if done < 0 {
            Err(io::Error::last_os_error())
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short zcnblk arena I/O",
            ))
        }
    }

    /// Wait until the target releases this slot, then reserve it for the app again.
    pub fn wait_reacquire(&mut self, timeout: Duration) -> io::Result<()> {
        if self.app_owned {
            return Ok(());
        }
        // The target publishes its payload-owner return before the block CQE,
        // so this is the overwhelmingly common path. Do not read the clock or
        // enter the scheduler for an ownership token that is already ours.
        if self.inner.returned_to_app(self.lane, self.slot) {
            self.app_owned = true;
            return Ok(());
        }
        if self.inner.reacquire_existing(self.lane, self.slot) {
            self.app_owned = true;
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
        loop {
            // A completion can become visible a few cacheline round trips
            // after its CQE. Keep that transient race out of the scheduler and
            // amortize timeout clock reads across a bounded spin window.
            for _ in 0..64 {
                if self.inner.returned_to_app(self.lane, self.slot) {
                    self.app_owned = true;
                    return Ok(());
                }
                if self.inner.reacquire_existing(self.lane, self.slot) {
                    self.app_owned = true;
                    return Ok(());
                }
                std::hint::spin_loop();
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::yield_now();
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "timed out waiting for zcnblk arena slot release lane={} slot={} owner={} free_slots={}",
                self.lane,
                self.slot,
                self.inner
                    .owner(self.inner.global_slot(self.lane, self.slot))
                    .load(Ordering::Acquire),
                self.inner.free_slots(self.lane).load(Ordering::Acquire),
            ),
        ))
    }

    pub fn sync_and_reacquire(&mut self, file: &File, timeout: Duration) -> io::Result<()> {
        file.sync_all()?;
        self.wait_reacquire(timeout)
    }
}

impl Drop for ZcnblkAppArenaBuffer {
    fn drop(&mut self) {
        let global = self.inner.global_slot(self.lane, self.slot);
        let next_local_state =
            if self.app_owned && self.inner.release_app_reservation(self.lane, self.slot) {
                LOCAL_SLOT_FREE
            } else {
                // Synchronous fallback callers may drop immediately after pwrite
                // without a separate retirement call. Preserve their ownership
                // claim too; otherwise a target return to APP_RESERVED becomes an
                // unidentifiable, permanently stranded slot.
                LOCAL_SLOT_ORPHAN
            };
        self.inner.local_slots[global].store(next_local_state, Ordering::Release);
    }
}

/// A lane-local asynchronous submission ring for buffers leased from the
/// application arena. The ring only transports an already selected lane's
/// pointer through the zcnblk client edge; it does not choose placement,
/// mirrors, stripes, tiers, or downstream paths.
pub struct ZcnblkAppArenaIoRing {
    ring: RawRing,
}

// RawRing owns its fd and mmap pointers. Moving exclusive ownership does not
// invalidate either mapping, and ZcnblkAppArenaIoRing never exposes shared
// access, so this permits transfer without claiming Sync or concurrent ring
// use. Callers using IORING_SETUP_SINGLE_ISSUER must still construct and submit
// the live ring on the same worker; the vhost frontend does that lazily.
unsafe impl Send for ZcnblkAppArenaIoRing {}

#[derive(Clone, Copy, Debug, Default)]
pub struct ZcnblkAppArenaIoCompletion {
    pub user_data: u64,
    pub result: i32,
}

impl ZcnblkAppArenaIoRing {
    pub fn new(entries: u32) -> io::Result<Self> {
        if entries == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zcnblk arena io_uring needs at least one entry",
            ));
        }
        Ok(Self {
            ring: RawRing::new(entries, entries.saturating_mul(2))?,
        })
    }

    /// Build a ring whose completion eventfd can wake an otherwise idle
    /// single-issuer worker without relying on another submission or enter.
    pub fn new_event_driven(entries: u32) -> io::Result<Self> {
        if entries == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zcnblk arena io_uring needs at least one entry",
            ));
        }
        Ok(Self {
            ring: RawRing::new_event_driven(entries, entries.saturating_mul(2))?,
        })
    }

    pub fn queue_read(
        &mut self,
        file: &File,
        buffer: &mut ZcnblkAppArenaBuffer,
        offset: u64,
        user_data: u64,
    ) -> io::Result<()> {
        self.queue(file, buffer, offset, user_data, false)
    }

    pub fn queue_write(
        &mut self,
        file: &File,
        buffer: &mut ZcnblkAppArenaBuffer,
        offset: u64,
        user_data: u64,
    ) -> io::Result<()> {
        self.queue(file, buffer, offset, user_data, true)
    }

    fn queue(
        &mut self,
        file: &File,
        buffer: &mut ZcnblkAppArenaBuffer,
        offset: u64,
        user_data: u64,
        write: bool,
    ) -> io::Result<()> {
        let (pointer, len) = buffer.handoff_to_kernel()?;
        let len = u32::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena buffer exceeds u32"))?;
        let queued = if write {
            self.ring.queue_write(
                file.as_raw_fd(),
                pointer.cast_const(),
                len,
                offset,
                user_data,
            )
        } else {
            self.ring
                .queue_read(file.as_raw_fd(), pointer, len, offset, user_data)
        };
        if let Err(error) = queued {
            let _ = buffer.recover_unsubmitted();
            return Err(error);
        }
        Ok(())
    }

    pub fn submit(&mut self) -> io::Result<()> {
        self.ring.submit_pending()
    }

    /// Notify `eventfd` whenever this lane's io_uring publishes completions.
    ///
    /// The caller owns the descriptor and must keep it open until this ring is
    /// dropped. Registration is performed by the eventual SINGLE_ISSUER lane
    /// worker, alongside ring construction, rather than by a setup thread.
    pub fn register_completion_eventfd(&self, eventfd: RawFd) -> io::Result<()> {
        self.ring.register_eventfd(eventfd)
    }

    pub fn set_completion_eventfd_enabled(&self, enabled: bool) {
        self.ring.set_eventfd_enabled(enabled);
    }

    /// Run deferred io_uring task work without waiting for a completion.
    /// Event-driven consumers call this after an eventfd wake and once more
    /// while arming the idle edge, closing the task-work-before-CQE race.
    pub fn run_task_work(&mut self) -> io::Result<()> {
        self.ring.run_task_work()
    }

    pub fn try_completion(&mut self) -> Option<ZcnblkAppArenaIoCompletion> {
        self.ring
            .try_pop_cqe()
            .map(|completion| ZcnblkAppArenaIoCompletion {
                user_data: completion.user_data,
                result: completion.res,
            })
    }

    /// Copy a contiguous ready-CQ batch into caller-owned lane-local storage.
    /// This does not submit or wait and allocates no memory.
    pub fn try_completions(&mut self, completions: &mut [ZcnblkAppArenaIoCompletion]) -> usize {
        let mut next = 0usize;
        self.ring.drain_cqes(completions.len(), |completion| {
            completions[next] = ZcnblkAppArenaIoCompletion {
                user_data: completion.user_data,
                result: completion.res,
            };
            next += 1;
        });
        next
    }

    pub fn wait_completion(&mut self) -> io::Result<ZcnblkAppArenaIoCompletion> {
        let completion = self.ring.wait_cqe()?;
        Ok(ZcnblkAppArenaIoCompletion {
            user_data: completion.user_data,
            result: completion.res,
        })
    }

    /// Wait until at least `minimum` completions are ready, then copy as many
    /// as fit into caller-owned lane-local storage. This allocates no memory
    /// and lets high-QD frontends amortize both the wait syscall and their
    /// completion notification without changing low-QD behavior.
    pub fn wait_completions(
        &mut self,
        minimum: usize,
        completions: &mut [ZcnblkAppArenaIoCompletion],
    ) -> io::Result<usize> {
        if completions.is_empty() {
            return Ok(0);
        }
        let minimum = minimum.clamp(1, completions.len());
        let first = self.ring.wait_cqe_min(minimum as u32)?;
        completions[0] = ZcnblkAppArenaIoCompletion {
            user_data: first.user_data,
            result: first.res,
        };
        let mut next = 1usize;
        self.ring.drain_cqes(completions.len() - 1, |completion| {
            completions[next] = ZcnblkAppArenaIoCompletion {
                user_data: completion.user_data,
                result: completion.res,
            };
            next += 1;
        });
        Ok(next)
    }
}

pub fn open_block_direct(path: impl AsRef<Path>) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(path)
}

pub fn pin_current_thread(cpu: usize) -> io::Result<()> {
    if cpu >= libc::CPU_SETSIZE as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CPU out of range",
        ));
    }
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[repr(align(8))]
struct ControlBuffer([u8; 64]);

pub(crate) fn send_descriptor(
    stream: &mut UnixStream,
    descriptor: ZcnblkAppArenaDescriptor,
    fd: RawFd,
) -> io::Result<()> {
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(
            (&descriptor as *const ZcnblkAppArenaDescriptor).cast(),
            size_of::<ZcnblkAppArenaDescriptor>(),
        )
    };
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut control = ControlBuffer([0; 64]);
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.0.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) as usize };
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as usize;
        ptr::write(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
    }
    let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if (sent as usize) < bytes.len() {
        stream.write_all(&bytes[sent as usize..])?;
    }
    Ok(())
}

fn receive_descriptor(stream: &mut UnixStream) -> io::Result<(ZcnblkAppArenaDescriptor, OwnedFd)> {
    let mut descriptor = MaybeUninit::<ZcnblkAppArenaDescriptor>::zeroed();
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            descriptor.as_mut_ptr().cast::<u8>(),
            size_of::<ZcnblkAppArenaDescriptor>(),
        )
    };
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = ControlBuffer([0; 64]);
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.0.as_mut_ptr().cast();
    msg.msg_controllen = control.0.len();
    let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if received <= 0 {
        return Err(if received < 0 {
            io::Error::last_os_error()
        } else {
            io::Error::new(io::ErrorKind::UnexpectedEof, "arena exporter closed")
        });
    }
    let mut raw_fd = None;
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                raw_fd = Some(ptr::read(libc::CMSG_DATA(cmsg).cast::<RawFd>()));
                break;
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    let fd = raw_fd.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "arena descriptor has no file descriptor",
        )
    })?;
    if (received as usize) < bytes.len() {
        stream.read_exact(&mut bytes[received as usize..])?;
    }
    Ok((unsafe { descriptor.assume_init() }, unsafe {
        OwnedFd::from_raw_fd(fd)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_sys_util::eventfd::EventFd;

    #[test]
    fn descriptor_layout_is_stable() {
        assert_eq!(size_of::<ZcnblkAppArenaDescriptor>(), 80);
    }

    #[test]
    fn fd_export_and_application_reservation_round_trip() -> io::Result<()> {
        let raw_fd = unsafe {
            libc::memfd_create(
                b"zcnblk-app-arena-test\0".as_ptr().cast(),
                libc::MFD_CLOEXEC,
            )
        };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let region_bytes = 16 * 1024;
        if unsafe { libc::ftruncate(fd.as_raw_fd(), region_bytes) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let descriptor = ZcnblkAppArenaDescriptor {
            magic: ZCNBLK_APP_ARENA_MAGIC,
            version: ZCNBLK_APP_ARENA_VERSION,
            descriptor_bytes: size_of::<ZcnblkAppArenaDescriptor>() as u32,
            flags: ZCNBLK_APP_ARENA_F_EXTERNAL_HUGETLB,
            channels: 1,
            payload_entries: 1,
            slot_bytes: 4096,
            channel_bytes: ZCNBLK_SHM_CHANNEL_BYTES as u32,
            payload_free_slots_offset: ZCNBLK_SHM_CHANNEL_PAYLOAD_FREE_SLOTS as u32,
            reserved: 0,
            reserved2: 0,
            channel_offset: 512,
            payload_owner_offset: 4096,
            payload_offset: 8192,
            region_bytes: region_bytes as u64,
        };
        let init = unsafe {
            libc::mmap(
                ptr::null_mut(),
                region_bytes as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if init == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let free = unsafe {
            &*init
                .cast::<u8>()
                .add(512 + ZCNBLK_SHM_CHANNEL_PAYLOAD_FREE_SLOTS)
                .cast::<AtomicU64>()
        };
        free.store(1, Ordering::Release);
        unsafe { libc::munmap(init, region_bytes as usize) };

        let (mut sender, mut receiver) = UnixStream::pair()?;
        send_descriptor(&mut sender, descriptor, fd.as_raw_fd())?;
        let (received, received_fd) = receive_descriptor(&mut receiver)?;
        let arena = ZcnblkAppArena::from_descriptor(received.validate()?, received_fd)?;
        {
            let mut buffer = arena.allocate(0)?;
            assert_eq!(free_count(&arena), 0);
            buffer.as_mut_slice()?[0] = 0x5a;
            assert_eq!(buffer.as_slice()?[0], 0x5a);
            let (_, len) = buffer.handoff_to_kernel()?;
            assert_eq!(len, 4096);
            assert!(buffer.as_mut_slice().is_err());
            buffer.recover_unsubmitted()?;
            assert_eq!(buffer.as_slice()?[0], 0x5a);
        }
        assert_eq!(free_count(&arena), 1);
        Ok(())
    }

    #[test]
    fn async_ring_round_trips_one_block_on_lane_thread() -> io::Result<()> {
        let (arena, file) = ring_test_arena(2)?;
        let worker = std::thread::spawn(move || -> io::Result<()> {
            let mut ring = ZcnblkAppArenaIoRing::new(2)?;
            let mut buffer = arena.allocate(0)?;
            buffer.as_mut_slice()?.fill(0x6d);
            ring.queue_write(&file, &mut buffer, 0, 11)?;
            ring.submit()?;
            let completion = ring.wait_completion()?;
            assert_eq!(completion.user_data, 11);
            assert_eq!(completion.result, 4096);
            buffer.wait_reacquire(Duration::from_secs(1))?;

            buffer.as_mut_slice()?.fill(0);
            ring.queue_read(&file, &mut buffer, 0, 12)?;
            ring.submit()?;
            let completion = ring.wait_completion()?;
            assert_eq!(completion.user_data, 12);
            assert_eq!(completion.result, 4096);
            buffer.wait_reacquire(Duration::from_secs(1))?;
            assert!(buffer.as_slice()?.iter().all(|byte| *byte == 0x6d));
            Ok(())
        });
        worker.join().expect("lane worker panicked")
    }

    #[test]
    fn async_ring_reaps_ready_completions_as_one_batch() -> io::Result<()> {
        let (arena, file) = ring_test_arena(4)?;
        let worker = std::thread::spawn(move || -> io::Result<()> {
            let mut ring = ZcnblkAppArenaIoRing::new(4)?;
            let mut buffers = (0..4)
                .map(|_| arena.allocate(0))
                .collect::<io::Result<Vec<_>>>()?;
            for (index, buffer) in buffers.iter_mut().enumerate() {
                buffer.as_mut_slice()?.fill(index as u8 + 1);
                ring.queue_write(&file, buffer, 0, 100 + index as u64)?;
            }
            ring.submit()?;

            let mut ready = [ZcnblkAppArenaIoCompletion::default(); 4];
            let count = ring.wait_completions(4, &mut ready)?;
            assert_eq!(count, 4);
            let mut seen = ready
                .iter()
                .map(|completion| completion.user_data)
                .collect::<Vec<_>>();
            seen.sort_unstable();
            assert_eq!(seen, vec![100, 101, 102, 103]);
            for buffer in &mut buffers {
                buffer.wait_reacquire(Duration::from_secs(1))?;
            }
            Ok(())
        });
        worker.join().expect("lane worker panicked")
    }

    #[test]
    fn async_ring_completion_eventfd_wakes_the_lane() -> io::Result<()> {
        let (arena, file) = ring_test_arena(1)?;
        let worker = std::thread::spawn(move || -> io::Result<()> {
            let event = EventFd::new(libc::EFD_CLOEXEC | libc::EFD_NONBLOCK)?;
            let mut ring = ZcnblkAppArenaIoRing::new_event_driven(1)?;
            // Eventfd is the idle worker's only wake source in this mode. A
            // deferred-taskrun ring would wait for that worker to enter while
            // the worker simultaneously waits for the CQE eventfd signal.
            assert!(!ring.ring.defer_taskrun);
            ring.register_completion_eventfd(event.as_raw_fd())?;
            let mut buffer = arena.allocate(0)?;
            buffer.as_mut_slice()?.fill(0x4e);
            ring.queue_write(&file, &mut buffer, 0, 77)?;
            ring.submit()?;

            let deadline = Instant::now() + Duration::from_secs(1);
            let notification_count = loop {
                match event.read() {
                    Ok(count) => break count,
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::yield_now();
                    }
                    Err(error) => return Err(error),
                }
            };
            assert!(notification_count >= 1);
            let completion = ring.wait_completion()?;
            assert_eq!(completion.user_data, 77);
            assert_eq!(completion.result, 4096);
            buffer.wait_reacquire(Duration::from_secs(1))?;
            Ok(())
        });
        worker.join().expect("lane worker panicked")
    }

    #[test]
    fn lane_cursor_starts_at_hint_and_wraps_once() -> io::Result<()> {
        let (arena, _file) = ring_test_arena(2)?;
        let second = arena.allocate_from(0, 1)?;
        assert_eq!(second.slot(), 1);
        let first = arena.allocate_from(0, 1)?;
        assert_eq!(first.slot(), 0);
        drop((first, second));
        assert_eq!(free_count(&arena), 2);
        Ok(())
    }

    #[test]
    fn completed_write_alias_is_released_or_reclaimed_after_late_return() -> io::Result<()> {
        let (arena, _file) = ring_test_arena(1)?;
        let owner = arena.inner.owner(0);

        // Immediate target return: retirement reacquires APP_RESERVED and
        // ordinary Drop balances the original free-slot debit.
        {
            let mut buffer = arena.allocate(0)?;
            buffer.handoff_to_kernel()?;
            buffer.retire_completed_handoff()?;
        }
        assert_eq!(owner.load(Ordering::Acquire), 0);
        assert_eq!(free_count(&arena), 1);

        // A synchronous fallback may return from pwrite and drop its handle
        // without calling the asynchronous CQE retirement helper. Drop must
        // retain enough process-local identity for graceful disconnect cleanup
        // to release an already returned APP_RESERVED token.
        {
            let mut buffer = arena.allocate(0)?;
            buffer.handoff_to_kernel()?;
            drop(buffer);
        }
        assert_eq!(arena.inner.release_returned_orphans(), 1);
        assert_eq!(owner.load(Ordering::Acquire), 0);
        assert_eq!(free_count(&arena), 1);

        // Delayed writeback return: the completed handle is gone while the
        // target still owns its sequence token. Allocation must fail until the
        // target returns APP_RESERVED, then adopt exactly this process's
        // orphan without double-debiting the shared free count.
        {
            let mut buffer = arena.allocate(0)?;
            buffer.handoff_to_kernel()?;
            owner.store(41, Ordering::Release);
            buffer.retire_completed_handoff()?;
        }
        assert_eq!(free_count(&arena), 0);
        match arena.allocate(0) {
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
            Ok(_) => panic!("target-retained orphan was allocated before return"),
        }
        owner.store(ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED, Ordering::Release);
        {
            let buffer = arena.allocate(0)?;
            assert_eq!(free_count(&arena), 0);
            drop(buffer);
        }
        assert_eq!(owner.load(Ordering::Acquire), 0);
        assert_eq!(free_count(&arena), 1);
        Ok(())
    }

    fn ring_test_arena(entries: u32) -> io::Result<(ZcnblkAppArena, File)> {
        let raw_fd = unsafe {
            libc::memfd_create(
                b"zcnblk-app-arena-ring-test\0".as_ptr().cast(),
                libc::MFD_CLOEXEC,
            )
        };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let payload_offset = 8192usize;
        let region_bytes = payload_offset + entries as usize * 4096;
        if unsafe { libc::ftruncate(fd.as_raw_fd(), region_bytes as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let descriptor = ZcnblkAppArenaDescriptor {
            magic: ZCNBLK_APP_ARENA_MAGIC,
            version: ZCNBLK_APP_ARENA_VERSION,
            descriptor_bytes: size_of::<ZcnblkAppArenaDescriptor>() as u32,
            flags: ZCNBLK_APP_ARENA_F_EXTERNAL_HUGETLB,
            channels: 1,
            payload_entries: entries,
            slot_bytes: 4096,
            channel_bytes: ZCNBLK_SHM_CHANNEL_BYTES as u32,
            payload_free_slots_offset: ZCNBLK_SHM_CHANNEL_PAYLOAD_FREE_SLOTS as u32,
            reserved: 0,
            reserved2: 0,
            channel_offset: 512,
            payload_owner_offset: 4096,
            payload_offset: payload_offset as u64,
            region_bytes: region_bytes as u64,
        };
        let init = unsafe {
            libc::mmap(
                ptr::null_mut(),
                region_bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if init == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let free = unsafe {
            &*init
                .cast::<u8>()
                .add(512 + ZCNBLK_SHM_CHANNEL_PAYLOAD_FREE_SLOTS)
                .cast::<AtomicU64>()
        };
        free.store(u64::from(entries), Ordering::Release);
        unsafe { libc::munmap(init, region_bytes) };

        let file_fd = unsafe {
            libc::memfd_create(
                b"zcnblk-app-arena-io-test\0".as_ptr().cast(),
                libc::MFD_CLOEXEC,
            )
        };
        if file_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::ftruncate(file_fd, 4096) } != 0 {
            unsafe { libc::close(file_fd) };
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(file_fd) };
        let arena = ZcnblkAppArena::from_descriptor(descriptor.validate()?, fd)?;
        Ok((arena, file))
    }

    fn free_count(arena: &ZcnblkAppArena) -> u64 {
        arena.inner.free_slots(0).load(Ordering::Acquire)
    }
}
