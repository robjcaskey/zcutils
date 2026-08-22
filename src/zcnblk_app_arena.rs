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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::RawRing;

pub const ZCNBLK_APP_ARENA_MAGIC: u64 = 0x3141_5041_434e_435a; // "ZCNCAPA1"
pub const ZCNBLK_APP_ARENA_VERSION: u32 = 1;
pub const ZCNBLK_APP_ARENA_F_EXTERNAL_HUGETLB: u32 = 1 << 0;
pub const ZCNBLK_SHM_PAYLOAD_OWNER_APP_RESERVED: u64 = u64::MAX - 1;
const ZCNBLK_SHM_CHANNEL_BYTES: usize = 320;
const ZCNBLK_SHM_CHANNEL_PAYLOAD_FREE_SLOTS: usize = 256;

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
    local_slots: Vec<AtomicBool>,
    _fd: OwnedFd,
}

unsafe impl Send for ArenaInner {}
unsafe impl Sync for ArenaInner {}

impl Drop for ArenaInner {
    fn drop(&mut self) {
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
        if self.local_slots[global]
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
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
            self.local_slots[global].store(false, Ordering::Release);
            return false;
        }
        let prior = self.free_slots(lane).fetch_sub(1, Ordering::AcqRel);
        if prior == 0 {
            self.free_slots(lane).fetch_add(1, Ordering::Release);
            self.owner(global).store(0, Ordering::Release);
            self.local_slots[global].store(false, Ordering::Release);
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
                local_slots: (0..slot_count).map(|_| AtomicBool::new(false)).collect(),
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

    /// Reserves any free payload buffer belonging to `lane`.
    pub fn allocate(&self, lane: u32) -> io::Result<ZcnblkAppArenaBuffer> {
        if lane >= self.channels() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "lane out of range",
            ));
        }
        for slot in 0..self.inner.descriptor.payload_entries {
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
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.inner.returned_to_app(self.lane, self.slot) {
                self.app_owned = true;
                return Ok(());
            }
            if self.inner.reacquire_existing(self.lane, self.slot) {
                self.app_owned = true;
                return Ok(());
            }
            std::hint::spin_loop();
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
        if self.app_owned {
            self.inner.release_app_reservation(self.lane, self.slot);
        }
        self.inner.local_slots[self.inner.global_slot(self.lane, self.slot)]
            .store(false, Ordering::Release);
    }
}

/// A lane-local asynchronous submission ring for buffers leased from the
/// application arena. The ring only transports an already selected lane's
/// pointer through the zcnblk client edge; it does not choose placement,
/// mirrors, stripes, tiers, or downstream paths.
pub struct ZcnblkAppArenaIoRing {
    ring: RawRing,
}

#[derive(Clone, Copy, Debug)]
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

    pub fn try_completion(&mut self) -> Option<ZcnblkAppArenaIoCompletion> {
        self.ring
            .try_pop_cqe()
            .map(|completion| ZcnblkAppArenaIoCompletion {
                user_data: completion.user_data,
                result: completion.res,
            })
    }

    pub fn wait_completion(&mut self) -> io::Result<ZcnblkAppArenaIoCompletion> {
        let completion = self.ring.wait_cqe()?;
        Ok(ZcnblkAppArenaIoCompletion {
            user_data: completion.user_data,
            result: completion.res,
        })
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

    fn free_count(arena: &ZcnblkAppArena) -> u64 {
        arena.inner.free_slots(0).load(Ordering::Acquire)
    }
}
