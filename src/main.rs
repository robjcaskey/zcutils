use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicU64, Ordering, fence};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::{Duration, Instant};

mod io_slots;

const IORING_REGISTER_QUERY: u32 = 35;
const IORING_REGISTER_ZCRX_IFQ: u32 = 32;
const IO_URING_QUERY_OPCODES: u32 = 0;
const IO_URING_QUERY_ZCRX: u32 = 1;

const IORING_OP_SEND: u32 = 26;
const IORING_OP_RECV: u32 = 27;
const IORING_OP_MSG_RING: u32 = 40;
const IORING_OP_WRITE_FIXED: u32 = 5;
const IORING_OP_WRITE: u32 = 23;
const IORING_OP_SEND_ZC: u32 = 47;
const IORING_OP_RECV_ZC: u32 = 58;
const IORING_OP_READV_FIXED: u32 = 60;
const IORING_OP_WRITEV_FIXED: u32 = 61;
const IORING_OP_PIPE: u32 = 62;
const IORING_OP_NOP128: u32 = 63;
const IORING_OP_URING_CMD128: u32 = 64;
const IORING_REGISTER_BUFFERS: u32 = 0;
const IORING_UNREGISTER_BUFFERS: u32 = 1;
const IORING_REGISTER_FILES: u32 = 2;
const IORING_UNREGISTER_FILES: u32 = 3;
const IORING_REGISTER_NAPI: u32 = 27;
const IORING_UNREGISTER_NAPI: u32 = 28;
const IORING_REGISTER_CLONE_BUFFERS: u32 = 30;
const IORING_REGISTER_SEND_MSG_RING: u32 = 31;
const IORING_REGISTER_RESIZE_RINGS: u32 = 33;
const IORING_REGISTER_MEM_REGION: u32 = 34;
const IORING_REGISTER_ZCRX_CTRL: u32 = 36;
const IORING_REGISTER_BPF_FILTER: u32 = 37;

const IORING_SETUP_CQSIZE: u32 = 1 << 3;
const IORING_SETUP_SUBMIT_ALL: u32 = 1 << 7;
const IORING_SETUP_COOP_TASKRUN: u32 = 1 << 8;
const IORING_SETUP_CQE32: u32 = 1 << 11;
const IORING_SETUP_SINGLE_ISSUER: u32 = 1 << 12;
const IORING_SETUP_DEFER_TASKRUN_U32: u32 = 1 << 13;

const IORING_RECV_MULTISHOT: u16 = 1 << 1;
const IORING_CQE_F_MORE: u32 = 1 << 1;
const IORING_CQE_F_NOTIF: u32 = 1 << 3;
const IORING_ENTER_GETEVENTS: u32 = 1 << 0;
const IORING_ENTER_NO_IOWAIT: u32 = 1 << 7;
const IORING_OFF_SQ_RING: u64 = 0;
const IORING_OFF_CQ_RING: u64 = 0x8000000;
const IORING_OFF_SQES: u64 = 0x10000000;
const IORING_ZCRX_AREA_SHIFT: u64 = 48;
const IORING_ZCRX_AREA_MASK: u64 = !((1u64 << IORING_ZCRX_AREA_SHIFT) - 1);
const RECV_ZC_USER_DATA: u64 = 0x7a637278;

const IORING_SETUP_DEFER_TASKRUN: u64 = 1 << 13;
const IORING_SETUP_CQE_MIXED: u64 = 1 << 18;
const IORING_SETUP_SQE_MIXED: u64 = 1 << 19;
const IORING_SETUP_SQ_REWIND: u64 = 1 << 20;
const IORING_FEAT_RECVSEND_BUNDLE: u64 = 1 << 14;
const IORING_FEAT_NO_IOWAIT: u32 = 1 << 17;

const IORING_MEM_REGION_TYPE_USER: u32 = 1;
const ZCRX_REG_IMPORT: u64 = 1;
const IORING_ZCRX_AREA_DMABUF: u64 = 1;
const ZCRX_FEATURE_RX_PAGE_SIZE: u32 = 1;
const IOSQE_FIXED_FILE: u8 = 1 << 0;
const IORING_RECVSEND_FIXED_BUF: u16 = 1 << 2;
const IORING_SEND_ZC_REPORT_USAGE: u16 = 1 << 3;
const IORING_NOTIF_USAGE_ZC_COPIED: i32 = 1i32 << 31;
const IOSQE_CQE_SKIP_SUCCESS: u8 = 1 << 6;
const CAP_NET_ADMIN: u64 = 12;
const LENSTREAM_HEADER_LEN: usize = 4;
const RAFT_APPEND_MAGIC: &[u8; 8] = b"URFTAE01";
const RAFT_ACK_MAGIC: &[u8; 8] = b"URFTACK1";
const RAFT_APPEND_HEADER_LEN: usize = 32;
const RAFT_ACK_LEN: usize = 16;
const SECTOR_SIZE: usize = 512;
const TCP_READY_BYTE: u8 = 0xa5;
const TCP_ACK_BYTE: u8 = 0x5a;
const TCP_START_BYTE: u8 = 0xc3;
const IORING_MAX_REG_BUFFERS: usize = 1 << 14;
const BLKGETSIZE64: libc::c_ulong = 0x80081272;
const BY_PARTUUID_DIR: &str = "/dev/disk/by-partuuid";
const RAW_PARTITION_ALLOWLIST: &str = "allowed-raw-partitions.txt";

#[repr(C)]
#[derive(Default)]
struct IoUringQueryHdr {
    next_entry: u64,
    query_data: u64,
    query_op: u32,
    size: u32,
    result: i32,
    resv: [u32; 3],
}

#[repr(C)]
#[derive(Default)]
struct IoUringQueryOpcode {
    nr_request_opcodes: u32,
    nr_register_opcodes: u32,
    feature_flags: u64,
    ring_setup_flags: u64,
    enter_flags: u64,
    sqe_flags: u64,
    nr_query_opcodes: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct IoUringQueryZcrx {
    register_flags: u64,
    area_flags: u64,
    nr_ctrl_opcodes: u32,
    features: u32,
    rq_hdr_size: u32,
    rq_hdr_alignment: u32,
    resv2: u64,
}

#[repr(C)]
#[derive(Default)]
struct IoSqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default)]
struct IoCqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default)]
struct IoUringParams {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: IoSqringOffsets,
    cq_off: IoCqringOffsets,
}

#[repr(C)]
#[derive(Default)]
struct IoUringRegionDesc {
    user_addr: u64,
    size: u64,
    flags: u32,
    id: u32,
    mmap_offset: u64,
    resv: [u64; 4],
}

#[repr(C)]
#[derive(Default)]
struct IoUringZcrxAreaReg {
    addr: u64,
    len: u64,
    rq_area_token: u64,
    flags: u32,
    dmabuf_fd: u32,
    resv2: [u64; 2],
}

#[repr(C)]
#[derive(Default)]
struct IoUringZcrxOffsets {
    head: u32,
    tail: u32,
    rqes: u32,
    resv2: u32,
    resv: [u64; 2],
}

#[repr(C)]
#[derive(Default)]
struct IoUringZcrxIfqReg {
    if_idx: u32,
    if_rxq: u32,
    rq_entries: u32,
    flags: u32,
    area_ptr: u64,
    region_ptr: u64,
    offsets: IoUringZcrxOffsets,
    zcrx_id: u32,
    rx_buf_len: u32,
    resv: [u64; 3],
}

#[repr(C)]
#[derive(Default)]
struct IoUringNapi {
    busy_poll_to: u32,
    prefer_busy_poll: u8,
    opcode: u8,
    pad: [u8; 2],
    op_param: u32,
    resv: u32,
}

#[repr(C)]
struct IoUringZcrxRqe {
    off: u64,
    len: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct IoUringSqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    rw_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    zcrx_ifq_idx: u32,
    addr3: u64,
    pad2: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct IoUringCqe32 {
    user_data: u64,
    res: i32,
    flags: u32,
    zcrx_off: u64,
    zcrx_pad: u64,
}

struct RawRing {
    fd: i32,
    sq_ring_ptr: *mut u8,
    sq_ring_size: usize,
    cq_ring_ptr: *mut u8,
    cq_ring_size: usize,
    sqes: *mut IoUringSqe,
    sqes_size: usize,
    sq_head: *mut u32,
    sq_tail: *mut u32,
    sq_mask: *mut u32,
    sq_entries: *mut u32,
    sq_array: *mut u32,
    cq_head: *mut u32,
    cq_tail: *mut u32,
    cq_mask: *mut u32,
    cqes: *mut IoUringCqe32,
    pending_submit: u32,
    enter_flags: u32,
    cqe_spin: u32,
    stats_enabled: bool,
    stats: RawRingStats,
}

#[derive(Default, Clone, Copy)]
struct RawRingStats {
    sqes_queued: u64,
    submit_syscalls: u64,
    wait_syscalls: u64,
    sqes_submitted: u64,
    cqes_popped: u64,
    try_pop_empty: u64,
    wait_cqe_calls: u64,
    submit_short: u64,
    cqe_spin_loops: u64,
}

struct ZcrxContext {
    rq_ptr: *mut libc::c_void,
    rq_size: usize,
    area_ptr: *mut libc::c_void,
    area_size: usize,
    area_memory_policy: &'static str,
    rx_buf_len: u32,
    rq_head: *mut u32,
    rq_tail_ptr: *mut u32,
    rqes: *mut IoUringZcrxRqe,
    rq_tail: u32,
    rq_entries: u32,
    area_token: u64,
    zcrx_id: u32,
}

fn io_uring_query<T>(query_op: u32, data: &mut T) -> io::Result<i32> {
    let mut hdr = IoUringQueryHdr {
        query_data: data as *mut T as u64,
        query_op,
        size: size_of::<T>() as u32,
        ..IoUringQueryHdr::default()
    };

    let ret = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            -1i32,
            IORING_REGISTER_QUERY,
            &mut hdr as *mut IoUringQueryHdr,
            0u32,
        )
    };

    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(hdr.result)
}

fn io_uring_register(
    fd: i32,
    opcode: u32,
    arg: *mut libc::c_void,
    nr_args: u32,
) -> io::Result<i64> {
    let ret = unsafe { libc::syscall(libc::SYS_io_uring_register, fd, opcode, arg, nr_args) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn io_uring_setup(entries: u32, params: &mut IoUringParams) -> io::Result<i32> {
    let ret = unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            entries,
            params as *mut IoUringParams,
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret as i32)
    }
}

fn has_cap_net_admin() -> io::Result<bool> {
    let status = fs::read_to_string("/proc/self/status")?;
    let cap_eff = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:\t"))
        .ok_or_else(|| io::Error::other("CapEff missing from /proc/self/status"))?;
    let bits = u64::from_str_radix(cap_eff.trim(), 16)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

    Ok((bits & (1 << CAP_NET_ADMIN)) != 0)
}

fn yes(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

fn page_size() -> io::Result<usize> {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(page_size as usize)
    }
}

fn mmap_rw(size: usize) -> io::Result<*mut libc::c_void> {
    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(ptr)
    }
}

fn mmap_shared(fd: i32, size: usize, offset: u64) -> io::Result<*mut u8> {
    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            offset as libc::off_t,
        )
    };
    if ptr == libc::MAP_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(ptr as *mut u8)
    }
}

fn munmap_if_mapped(ptr: *mut libc::c_void, size: usize) {
    if !ptr.is_null() && size != 0 {
        unsafe {
            libc::munmap(ptr, size);
        }
    }
}

fn first_touch_pages(ptr: *mut libc::c_void, len: usize, byte: u8) -> io::Result<()> {
    if ptr.is_null() || len == 0 {
        return Ok(());
    }

    let page_size = page_size()?;
    let mut offset = 0usize;
    while offset < len {
        unsafe {
            ptr::write_volatile((ptr as *mut u8).add(offset), byte);
        }
        offset = offset.saturating_add(page_size);
    }
    unsafe {
        ptr::write_volatile((ptr as *mut u8).add(len - 1), byte);
    }
    Ok(())
}

fn maybe_mbind_preferred(
    ptr: *mut libc::c_void,
    len: usize,
    preferred_numa_node: Option<i32>,
    label: &str,
) -> &'static str {
    if !env_truthy("URING_PLAY_WAL_MEMBIND") && !env_truthy("URING_PLAY_ZCRX_MEMBIND") {
        return "first-touch";
    }

    let Some(node) = preferred_numa_node else {
        return "first-touch-no-numa-node";
    };

    match mbind_preferred(ptr, len, node) {
        Ok(()) => "mbind-preferred+first-touch",
        Err(err) => {
            eprintln!(
                "warning: URING_PLAY_WAL_MEMBIND=1 failed for {label} node={node}: {err}; \
                 falling back to first-touch placement"
            );
            "first-touch-mbind-failed"
        }
    }
}

#[cfg(target_os = "linux")]
fn mbind_preferred(ptr: *mut libc::c_void, len: usize, node: i32) -> io::Result<()> {
    if node < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid NUMA node {node}"),
        ));
    }
    let bits = std::mem::size_of::<libc::c_ulong>() * 8;
    let node_usize = node as usize;
    if node_usize >= bits {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("NUMA node {node} exceeds single-word mbind mask support"),
        ));
    }

    const MPOL_PREFERRED: libc::c_int = 1;
    let nodemask: libc::c_ulong = 1u64
        .checked_shl(node_usize as u32)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "NUMA node mask overflow"))?
        as libc::c_ulong;
    let maxnode = (node_usize + 1) as libc::c_ulong;
    let ret = unsafe {
        libc::syscall(
            libc::SYS_mbind,
            ptr,
            len,
            MPOL_PREFERRED,
            &nodemask as *const libc::c_ulong,
            maxnode,
            0usize,
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
fn mbind_preferred(_ptr: *mut libc::c_void, _len: usize, _node: i32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "mbind is only available on Linux",
    ))
}

struct FixedSendBuffers {
    ptr: *mut u8,
    stride: usize,
    count: usize,
    map_len: usize,
    memory_policy: &'static str,
}

impl FixedSendBuffers {
    fn new(count: usize, len: usize) -> io::Result<Self> {
        Self::new_with_preferred_numa(count, len, None)
    }

    fn new_with_preferred_numa(
        count: usize,
        len: usize,
        preferred_numa_node: Option<i32>,
    ) -> io::Result<Self> {
        let page_size = page_size()?;
        let mut buffers = Self::new_mmap(
            count,
            len,
            page_size,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
        )?;
        buffers.memory_policy = maybe_mbind_preferred(
            buffers.ptr.cast(),
            buffers.map_len,
            preferred_numa_node,
            "small-pages",
        );

        unsafe {
            let _ = libc::madvise(buffers.ptr.cast(), buffers.map_len, libc::MADV_NOHUGEPAGE);
            ptr::write_bytes(buffers.ptr, 0x5a, buffers.map_len);
        }

        Ok(buffers)
    }

    #[allow(dead_code)]
    fn new_hugetlb(count: usize, len: usize) -> io::Result<Self> {
        Self::new_hugetlb_with_preferred_numa(count, len, None)
    }

    fn new_hugetlb_with_preferred_numa(
        count: usize,
        len: usize,
        preferred_numa_node: Option<i32>,
    ) -> io::Result<Self> {
        let hugepage_size = default_hugepage_size()?;
        let mut buffers = Self::new_mmap(
            count,
            len,
            hugepage_size,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB,
        )?;
        buffers.memory_policy = maybe_mbind_preferred(
            buffers.ptr.cast(),
            buffers.map_len,
            preferred_numa_node,
            "hugetlb",
        );

        unsafe {
            ptr::write_bytes(buffers.ptr, 0x5a, buffers.map_len);
        }

        Ok(buffers)
    }

    fn new_mmap(count: usize, len: usize, alignment: usize, flags: i32) -> io::Result<Self> {
        let stride = align_up(len.max(1), alignment);
        let map_len = stride.checked_mul(count).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "fixed send buffer mapping is too large",
            )
        })?;
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            ptr: ptr.cast(),
            stride,
            count,
            map_len,
            memory_policy: "first-touch",
        })
    }

    fn ptr(&self, index: usize) -> *mut u8 {
        debug_assert!(index < self.count);
        unsafe { self.ptr.add(index * self.stride) }
    }

    fn iovecs(&self, len: usize) -> Vec<libc::iovec> {
        (0..self.count)
            .map(|index| libc::iovec {
                iov_base: self.ptr(index).cast(),
                iov_len: len,
            })
            .collect()
    }

    fn fill_each(&self, len: usize, mut fill: impl FnMut(&mut [u8])) {
        for index in 0..self.count {
            let buf = unsafe { slice::from_raw_parts_mut(self.ptr(index), len) };
            fill(buf);
        }
    }

    fn base_addr(&self) -> usize {
        self.ptr as usize
    }

    fn stride(&self) -> usize {
        self.stride
    }

    fn map_len(&self) -> usize {
        self.map_len
    }

    fn memory_policy(&self) -> &'static str {
        self.memory_policy
    }
}

impl Drop for FixedSendBuffers {
    fn drop(&mut self) {
        munmap_if_mapped(self.ptr.cast(), self.map_len);
    }
}

#[derive(Clone, Copy)]
struct FixedBufferView {
    ptr_addr: usize,
    stride: usize,
    count: usize,
}

impl FixedBufferView {
    fn from_buffers(buffers: &FixedSendBuffers) -> Self {
        Self {
            ptr_addr: buffers.ptr as usize,
            stride: buffers.stride,
            count: buffers.count,
        }
    }

    fn ptr(self, index: usize) -> *mut u8 {
        debug_assert!(index < self.count);
        (self.ptr_addr + index * self.stride) as *mut u8
    }

    fn iovecs(self, len: usize) -> Vec<libc::iovec> {
        (0..self.count)
            .map(|index| libc::iovec {
                iov_base: self.ptr(index).cast(),
                iov_len: len,
            })
            .collect()
    }
}

fn io_uring_enter(fd: i32, to_submit: u32, min_complete: u32, flags: u32) -> io::Result<i64> {
    loop {
        let ret = unsafe {
            libc::syscall(
                libc::SYS_io_uring_enter,
                fd,
                to_submit,
                min_complete,
                flags,
                ptr::null::<libc::sigset_t>(),
                0usize,
            )
        };
        if ret >= 0 {
            return Ok(ret);
        }

        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(err);
        }
    }
}

impl RawRing {
    fn new(entries: u32, cq_entries: u32) -> io::Result<Self> {
        Self::new_with_stats(entries, cq_entries, false)
    }

    fn new_with_stats(entries: u32, cq_entries: u32, stats_enabled: bool) -> io::Result<Self> {
        let mut params = IoUringParams {
            flags: IORING_SETUP_COOP_TASKRUN
                | IORING_SETUP_SINGLE_ISSUER
                | IORING_SETUP_DEFER_TASKRUN_U32
                | IORING_SETUP_SUBMIT_ALL
                | IORING_SETUP_CQE32
                | IORING_SETUP_CQSIZE,
            cq_entries,
            ..IoUringParams::default()
        };
        let fd = io_uring_setup(entries, &mut params)?;
        let enter_flags = if env_truthy("URING_PLAY_ENTER_NO_IOWAIT")
            && (params.features & IORING_FEAT_NO_IOWAIT) != 0
        {
            IORING_ENTER_NO_IOWAIT
        } else {
            0
        };
        let cqe_spin = env_usize_or("URING_PLAY_CQE_SPIN", 0).min(u32::MAX as usize) as u32;

        let sq_ring_size =
            params.sq_off.array as usize + params.sq_entries as usize * size_of::<u32>();
        let cq_ring_size =
            params.cq_off.cqes as usize + params.cq_entries as usize * size_of::<IoUringCqe32>();
        let sqes_size = params.sq_entries as usize * size_of::<IoUringSqe>();

        let sq_ring_ptr = match mmap_shared(fd, sq_ring_size, IORING_OFF_SQ_RING) {
            Ok(ptr) => ptr,
            Err(err) => {
                unsafe {
                    libc::close(fd);
                }
                return Err(err);
            }
        };
        let cq_ring_ptr = match mmap_shared(fd, cq_ring_size, IORING_OFF_CQ_RING) {
            Ok(ptr) => ptr,
            Err(err) => {
                munmap_if_mapped(sq_ring_ptr as *mut libc::c_void, sq_ring_size);
                unsafe {
                    libc::close(fd);
                }
                return Err(err);
            }
        };
        let sqes = match mmap_shared(fd, sqes_size, IORING_OFF_SQES) {
            Ok(ptr) => ptr as *mut IoUringSqe,
            Err(err) => {
                munmap_if_mapped(sq_ring_ptr as *mut libc::c_void, sq_ring_size);
                munmap_if_mapped(cq_ring_ptr as *mut libc::c_void, cq_ring_size);
                unsafe {
                    libc::close(fd);
                }
                return Err(err);
            }
        };

        let ptr_at = |base: *mut u8, offset: u32| unsafe { base.add(offset as usize) };

        Ok(Self {
            fd,
            sq_ring_ptr,
            sq_ring_size,
            cq_ring_ptr,
            cq_ring_size,
            sqes,
            sqes_size,
            sq_head: ptr_at(sq_ring_ptr, params.sq_off.head) as *mut u32,
            sq_tail: ptr_at(sq_ring_ptr, params.sq_off.tail) as *mut u32,
            sq_mask: ptr_at(sq_ring_ptr, params.sq_off.ring_mask) as *mut u32,
            sq_entries: ptr_at(sq_ring_ptr, params.sq_off.ring_entries) as *mut u32,
            sq_array: ptr_at(sq_ring_ptr, params.sq_off.array) as *mut u32,
            cq_head: ptr_at(cq_ring_ptr, params.cq_off.head) as *mut u32,
            cq_tail: ptr_at(cq_ring_ptr, params.cq_off.tail) as *mut u32,
            cq_mask: ptr_at(cq_ring_ptr, params.cq_off.ring_mask) as *mut u32,
            cqes: ptr_at(cq_ring_ptr, params.cq_off.cqes) as *mut IoUringCqe32,
            pending_submit: 0,
            enter_flags,
            cqe_spin,
            stats_enabled,
            stats: RawRingStats::default(),
        })
    }

    fn fd(&self) -> i32 {
        self.fd
    }

    fn stats(&self) -> RawRingStats {
        self.stats
    }

    fn sq_available(&self) -> usize {
        unsafe {
            let head = ptr::read_volatile(self.sq_head);
            let tail = ptr::read_volatile(self.sq_tail);
            let entries = ptr::read_volatile(self.sq_entries);
            entries.saturating_sub(tail.wrapping_sub(head)) as usize
        }
    }

    fn push_sqe<F>(&mut self, user_data: u64, fill: F) -> io::Result<()>
    where
        F: FnOnce(&mut IoUringSqe),
    {
        unsafe {
            let head = ptr::read_volatile(self.sq_head);
            let tail = ptr::read_volatile(self.sq_tail);
            let entries = ptr::read_volatile(self.sq_entries);

            if tail.wrapping_sub(head) >= entries {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "io_uring SQ is full",
                ));
            }

            let mask = ptr::read_volatile(self.sq_mask);
            let idx = tail & mask;
            let sqe = self.sqes.add(idx as usize);
            ptr::write(sqe, IoUringSqe::default());
            (*sqe).user_data = user_data;
            fill(&mut *sqe);

            ptr::write_volatile(self.sq_array.add(idx as usize), idx);
            fence(Ordering::Release);
            ptr::write_volatile(self.sq_tail, tail.wrapping_add(1));
            self.pending_submit = self.pending_submit.saturating_add(1);
            if self.stats_enabled {
                self.stats.sqes_queued = self.stats.sqes_queued.saturating_add(1);
            }
        }

        Ok(())
    }

    fn queue_send(
        &mut self,
        fd: i32,
        buf: *const u8,
        len: u32,
        flags: u32,
        user_data: u64,
    ) -> io::Result<()> {
        self.push_sqe(user_data, |sqe| {
            sqe.opcode = IORING_OP_SEND as u8;
            sqe.fd = fd;
            sqe.addr = buf as u64;
            sqe.len = len;
            sqe.rw_flags = flags;
        })
    }

    fn queue_send_fixed_file(
        &mut self,
        file_index: u32,
        buf: *const u8,
        len: u32,
        flags: u32,
        user_data: u64,
    ) -> io::Result<()> {
        self.push_sqe(user_data, |sqe| {
            sqe.opcode = IORING_OP_SEND as u8;
            sqe.flags |= IOSQE_FIXED_FILE;
            sqe.fd = file_index as i32;
            sqe.addr = buf as u64;
            sqe.len = len;
            sqe.rw_flags = flags;
        })
    }

    fn queue_send_zc(
        &mut self,
        fd: i32,
        buf: *const u8,
        len: u32,
        flags: u32,
        fixed_buf_index: Option<u16>,
        report_usage: bool,
        user_data: u64,
    ) -> io::Result<()> {
        self.push_sqe(user_data, |sqe| {
            sqe.opcode = IORING_OP_SEND_ZC as u8;
            sqe.fd = fd;
            sqe.addr = buf as u64;
            sqe.len = len;
            sqe.rw_flags = flags;
            if report_usage {
                sqe.ioprio |= IORING_SEND_ZC_REPORT_USAGE;
            }
            if let Some(index) = fixed_buf_index {
                sqe.ioprio |= IORING_RECVSEND_FIXED_BUF;
                sqe.buf_index = index;
            }
        })
    }

    fn queue_send_zc_fixed_file(
        &mut self,
        file_index: u32,
        buf: *const u8,
        len: u32,
        flags: u32,
        fixed_buf_index: Option<u16>,
        report_usage: bool,
        user_data: u64,
    ) -> io::Result<()> {
        self.push_sqe(user_data, |sqe| {
            sqe.opcode = IORING_OP_SEND_ZC as u8;
            sqe.flags |= IOSQE_FIXED_FILE;
            sqe.fd = file_index as i32;
            sqe.addr = buf as u64;
            sqe.len = len;
            sqe.rw_flags = flags;
            if report_usage {
                sqe.ioprio |= IORING_SEND_ZC_REPORT_USAGE;
            }
            if let Some(index) = fixed_buf_index {
                sqe.ioprio |= IORING_RECVSEND_FIXED_BUF;
                sqe.buf_index = index;
            }
        })
    }

    fn queue_recv(
        &mut self,
        fd: i32,
        buf: *mut u8,
        len: u32,
        flags: u32,
        user_data: u64,
    ) -> io::Result<()> {
        self.push_sqe(user_data, |sqe| {
            sqe.opcode = IORING_OP_RECV as u8;
            sqe.fd = fd;
            sqe.addr = buf as u64;
            sqe.len = len;
            sqe.rw_flags = flags;
        })
    }

    fn queue_recv_fixed_file(
        &mut self,
        file_index: u32,
        buf: *mut u8,
        len: u32,
        flags: u32,
        user_data: u64,
    ) -> io::Result<()> {
        self.push_sqe(user_data, |sqe| {
            sqe.opcode = IORING_OP_RECV as u8;
            sqe.flags |= IOSQE_FIXED_FILE;
            sqe.fd = file_index as i32;
            sqe.addr = buf as u64;
            sqe.len = len;
            sqe.rw_flags = flags;
        })
    }

    fn queue_msg_ring(
        &mut self,
        target_ring_fd: i32,
        res: u32,
        data: u64,
        flags: u32,
        skip_source_cqe: bool,
        user_data: u64,
    ) -> io::Result<()> {
        self.push_sqe(user_data, |sqe| {
            sqe.opcode = IORING_OP_MSG_RING as u8;
            sqe.fd = target_ring_fd;
            sqe.off = data;
            sqe.len = res;
            sqe.rw_flags = flags;
            if skip_source_cqe {
                sqe.flags |= IOSQE_CQE_SKIP_SUCCESS;
            }
        })
    }

    fn queue_write(
        &mut self,
        fd: i32,
        buf: *const u8,
        len: u32,
        offset: u64,
        user_data: u64,
    ) -> io::Result<()> {
        self.push_sqe(user_data, |sqe| {
            sqe.opcode = IORING_OP_WRITE as u8;
            sqe.fd = fd;
            sqe.off = offset;
            sqe.addr = buf as u64;
            sqe.len = len;
        })
    }

    fn queue_write_fixed(
        &mut self,
        fd: i32,
        buf: *const u8,
        len: u32,
        offset: u64,
        buf_index: u16,
        user_data: u64,
    ) -> io::Result<()> {
        self.push_sqe(user_data, |sqe| {
            sqe.opcode = IORING_OP_WRITE_FIXED as u8;
            sqe.fd = fd;
            sqe.off = offset;
            sqe.addr = buf as u64;
            sqe.len = len;
            sqe.buf_index = buf_index;
        })
    }

    fn queue_write_fixed_file(
        &mut self,
        file_index: u32,
        buf: *const u8,
        len: u32,
        offset: u64,
        buf_index: u16,
        user_data: u64,
    ) -> io::Result<()> {
        self.push_sqe(user_data, |sqe| {
            sqe.opcode = IORING_OP_WRITE_FIXED as u8;
            sqe.flags |= IOSQE_FIXED_FILE;
            sqe.fd = file_index as i32;
            sqe.off = offset;
            sqe.addr = buf as u64;
            sqe.len = len;
            sqe.buf_index = buf_index;
        })
    }

    fn submit_pending(&mut self) -> io::Result<()> {
        while self.pending_submit > 0 {
            let pending_before = self.pending_submit;
            if self.stats_enabled {
                self.stats.submit_syscalls = self.stats.submit_syscalls.saturating_add(1);
            }
            let ret = io_uring_enter(self.fd, self.pending_submit, 0, self.enter_flags)?;
            if ret == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "io_uring submitted zero SQEs",
                ));
            }
            if self.stats_enabled {
                let submitted = ret as u32;
                self.stats.sqes_submitted =
                    self.stats.sqes_submitted.saturating_add(submitted as u64);
                if submitted < pending_before {
                    self.stats.submit_short = self.stats.submit_short.saturating_add(1);
                }
            }
            self.pending_submit = self.pending_submit.saturating_sub(ret as u32);
        }
        Ok(())
    }

    fn submit_recv_zc(&mut self, fd: i32, zcrx_id: u32, len: u32) -> io::Result<()> {
        self.queue_recv_zc(fd, zcrx_id, len, RECV_ZC_USER_DATA)?;
        self.submit_pending()
    }

    fn queue_recv_zc(&mut self, fd: i32, zcrx_id: u32, len: u32, user_data: u64) -> io::Result<()> {
        self.queue_recv_zc_with_mode(fd, zcrx_id, len, user_data, true)
    }

    fn queue_recv_zc_with_mode(
        &mut self,
        fd: i32,
        zcrx_id: u32,
        len: u32,
        user_data: u64,
        multishot: bool,
    ) -> io::Result<()> {
        self.push_sqe(user_data, |sqe| {
            sqe.opcode = IORING_OP_RECV_ZC as u8;
            if multishot {
                sqe.ioprio = IORING_RECV_MULTISHOT;
            }
            sqe.fd = fd;
            sqe.len = len;
            sqe.zcrx_ifq_idx = zcrx_id;
        })
    }

    #[allow(dead_code)]
    fn queue_slot_rw(
        &mut self,
        slot_id: io_slots::IoSlotId,
        buf_offset: u64,
        file_offset: u64,
        len: u32,
        direction: io_slots::SlotRw,
        user_data: u64,
    ) -> io::Result<()> {
        let request =
            io_slots::SlotRwRequest::new(slot_id, buf_offset, file_offset, len, direction)?;
        self.push_sqe(user_data, |sqe| {
            io_slots::prep_slot_rw_request(sqe, request);
        })
    }

    #[allow(dead_code)]
    fn register_io_slot(&self, buf_index: u32, file_index: u32) -> io::Result<io_slots::IoSlotId> {
        io_slots::register_io_slot(
            self.fd,
            io_slots::SlotDescriptor::new(buf_index, file_index),
        )
    }

    #[allow(dead_code)]
    fn unregister_io_slot(&self, slot_id: io_slots::IoSlotId) -> io::Result<()> {
        io_slots::unregister_io_slot(self.fd, slot_id)
    }

    fn register_buffers(&self, iovecs: &mut [libc::iovec]) -> io::Result<()> {
        io_uring_register(
            self.fd,
            IORING_REGISTER_BUFFERS,
            iovecs.as_mut_ptr() as *mut libc::c_void,
            iovecs.len() as u32,
        )?;
        Ok(())
    }

    fn unregister_buffers(&self) -> io::Result<()> {
        io_uring_register(self.fd, IORING_UNREGISTER_BUFFERS, ptr::null_mut(), 0)?;
        Ok(())
    }

    fn register_files(&self, fds: &mut [i32]) -> io::Result<()> {
        io_uring_register(
            self.fd,
            IORING_REGISTER_FILES,
            fds.as_mut_ptr() as *mut libc::c_void,
            fds.len() as u32,
        )?;
        Ok(())
    }

    fn unregister_files(&self) -> io::Result<()> {
        io_uring_register(self.fd, IORING_UNREGISTER_FILES, ptr::null_mut(), 0)?;
        Ok(())
    }

    fn wait_cqe(&mut self) -> io::Result<IoUringCqe32> {
        if self.stats_enabled {
            self.stats.wait_cqe_calls = self.stats.wait_cqe_calls.saturating_add(1);
        }
        self.submit_pending()?;
        loop {
            if let Some(cqe) = self.try_pop_cqe() {
                return Ok(cqe);
            }

            for _ in 0..self.cqe_spin {
                std::hint::spin_loop();
                if self.stats_enabled {
                    self.stats.cqe_spin_loops = self.stats.cqe_spin_loops.saturating_add(1);
                }
                if let Some(cqe) = self.try_pop_cqe() {
                    return Ok(cqe);
                }
            }

            if self.stats_enabled {
                self.stats.wait_syscalls = self.stats.wait_syscalls.saturating_add(1);
            }
            io_uring_enter(self.fd, 0, 1, IORING_ENTER_GETEVENTS | self.enter_flags)?;
        }
    }

    fn try_pop_cqe(&mut self) -> Option<IoUringCqe32> {
        unsafe {
            let head = ptr::read_volatile(self.cq_head);
            let tail = ptr::read_volatile(self.cq_tail);

            if head == tail {
                if self.stats_enabled {
                    self.stats.try_pop_empty = self.stats.try_pop_empty.saturating_add(1);
                }
                return None;
            }

            let mask = ptr::read_volatile(self.cq_mask);
            let cqe = ptr::read(self.cqes.add((head & mask) as usize));
            fence(Ordering::Release);
            ptr::write_volatile(self.cq_head, head.wrapping_add(1));
            if self.stats_enabled {
                self.stats.cqes_popped = self.stats.cqes_popped.saturating_add(1);
            }
            Some(cqe)
        }
    }

    fn register_napi_from_env(&self, label: &str) -> io::Result<()> {
        let Some(mut napi) = napi_config_from_env()? else {
            return Ok(());
        };
        io_uring_register(
            self.fd,
            IORING_REGISTER_NAPI,
            &mut napi as *mut IoUringNapi as *mut libc::c_void,
            1,
        )?;
        println!(
            "{label}: registered io_uring NAPI busy_poll_to={}us prefer_busy_poll={} tracking={}",
            napi.busy_poll_to,
            napi.prefer_busy_poll != 0,
            napi_tracking_label(napi.op_param)
        );
        Ok(())
    }
}

impl Drop for RawRing {
    fn drop(&mut self) {
        munmap_if_mapped(self.sqes as *mut libc::c_void, self.sqes_size);
        munmap_if_mapped(self.sq_ring_ptr as *mut libc::c_void, self.sq_ring_size);
        munmap_if_mapped(self.cq_ring_ptr as *mut libc::c_void, self.cq_ring_size);
        unsafe {
            libc::close(self.fd);
        }
    }
}

fn zcrx_env_area_size() -> usize {
    env_usize_or("URING_PLAY_ZCRX_AREA_MB", 64) * 1024 * 1024usize
}

fn env_usize_or(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn env_u8_or(name: &str, default: u8) -> io::Result<u8> {
    match env::var(name) {
        Ok(value) if !value.is_empty() => parse_u8_arg(&value).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name}={value:?} is not a valid byte: {err}"),
            )
        }),
        _ => Ok(default),
    }
}

impl ZcrxContext {
    fn register(
        ring_fd: i32,
        ifname: &str,
        rxq: u32,
        verbose: bool,
        prefill_byte: Option<u8>,
    ) -> io::Result<Self> {
        Self::register_with_options(ring_fd, ifname, rxq, verbose, prefill_byte, None, None)
    }

    fn register_with_options(
        ring_fd: i32,
        ifname: &str,
        rxq: u32,
        verbose: bool,
        prefill_byte: Option<u8>,
        preferred_numa_node: Option<i32>,
        area_size_override: Option<usize>,
    ) -> io::Result<Self> {
        let zcrx = if verbose {
            query_zcrx()?
        } else {
            query_zcrx_raw()?.0
        };
        let ifindex = if_nametoindex(ifname)?;
        let page_size = page_size()?;
        let rx_buf_len = env_size_opt("URING_PLAY_ZCRX_RX_BUF_LEN")?.unwrap_or(0);
        if rx_buf_len != 0 {
            if !rx_buf_len.is_power_of_two() || rx_buf_len < page_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "URING_PLAY_ZCRX_RX_BUF_LEN={rx_buf_len} must be a power of two \
                         and at least page size {page_size}"
                    ),
                ));
            }
            if rx_buf_len > u32::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "URING_PLAY_ZCRX_RX_BUF_LEN must fit in u32",
                ));
            }
        }
        let rq_entries = 32768usize;
        let rq_size = align_up(
            align_up(zcrx.rq_hdr_size as usize, zcrx.rq_hdr_alignment as usize)
                + rq_entries * size_of::<IoUringZcrxRqe>(),
            page_size,
        );
        let area_size = area_size_override.unwrap_or_else(zcrx_env_area_size);

        let rq_ptr = mmap_rw(rq_size)?;
        let area_ptr = match mmap_rw(area_size) {
            Ok(ptr) => ptr,
            Err(err) => {
                munmap_if_mapped(rq_ptr, rq_size);
                return Err(err);
            }
        };
        let area_memory_policy =
            maybe_mbind_preferred(area_ptr, area_size, preferred_numa_node, "zcrx-area");
        let _rq_memory_policy =
            maybe_mbind_preferred(rq_ptr, rq_size, preferred_numa_node, "zcrx-refill-queue");

        if let Some(byte) = prefill_byte {
            unsafe {
                ptr::write_bytes(area_ptr, byte, area_size);
            }
            first_touch_pages(rq_ptr, rq_size, 0)?;
        } else if env_enabled_or("URING_PLAY_ZCRX_FIRST_TOUCH", true) {
            first_touch_pages(area_ptr, area_size, 0)?;
            first_touch_pages(rq_ptr, rq_size, 0)?;
        }

        let mut area = IoUringZcrxAreaReg {
            addr: area_ptr as u64,
            len: area_size as u64,
            ..IoUringZcrxAreaReg::default()
        };
        let mut rq_region = IoUringRegionDesc {
            user_addr: rq_ptr as u64,
            size: rq_size as u64,
            flags: IORING_MEM_REGION_TYPE_USER,
            ..IoUringRegionDesc::default()
        };
        let mut ifq = IoUringZcrxIfqReg {
            if_idx: ifindex,
            if_rxq: rxq,
            rq_entries: rq_entries as u32,
            area_ptr: &mut area as *mut IoUringZcrxAreaReg as u64,
            region_ptr: &mut rq_region as *mut IoUringRegionDesc as u64,
            rx_buf_len: rx_buf_len as u32,
            ..IoUringZcrxIfqReg::default()
        };

        let mut register_result = io_uring_register(
            ring_fd,
            IORING_REGISTER_ZCRX_IFQ,
            &mut ifq as *mut IoUringZcrxIfqReg as *mut libc::c_void,
            1,
        );
        if let Err(err) = &register_result
            && rx_buf_len != 0
            && env_enabled_or("URING_PLAY_ZCRX_RX_BUF_LEN_FALLBACK", true)
            && matches!(
                err.raw_os_error(),
                Some(code) if code == libc::EINVAL || code == libc::ERANGE || code == libc::EOPNOTSUPP
            )
        {
            eprintln!(
                "warning: ZCRX IFQ rejected URING_PLAY_ZCRX_RX_BUF_LEN={rx_buf_len}: {err}; \
                 retrying with kernel default"
            );
            area.rq_area_token = 0;
            rq_region.id = 0;
            rq_region.mmap_offset = 0;
            ifq.offsets = IoUringZcrxOffsets::default();
            ifq.zcrx_id = 0;
            ifq.rx_buf_len = 0;
            register_result = io_uring_register(
                ring_fd,
                IORING_REGISTER_ZCRX_IFQ,
                &mut ifq as *mut IoUringZcrxIfqReg as *mut libc::c_void,
                1,
            );
        }
        if let Err(err) = register_result {
            munmap_if_mapped(rq_ptr, rq_size);
            munmap_if_mapped(area_ptr, area_size);
            return Err(err);
        }

        if verbose {
            println!("registered ZCRX IFQ:");
            println!("  interface: {ifname} ifindex={ifindex} rxq={rxq}");
            println!("  zcrx_id: {}", ifq.zcrx_id);
            println!("  rq_entries: {}", ifq.rq_entries);
            println!("  rq_size: {rq_size}");
            println!("  area_size: {area_size}");
            println!("  rx_buf_len: {}", ifq.rx_buf_len);
            println!("  area_memory_policy: {area_memory_policy}");
            println!(
                "  area_base: 0x{:x} area_alignment: {}",
                area_ptr as usize,
                address_alignment(area_ptr as usize)
            );
            println!("  area_token: 0x{:x}", area.rq_area_token);
            println!(
                "  refill offsets: head={} tail={} rqes={}",
                ifq.offsets.head, ifq.offsets.tail, ifq.offsets.rqes
            );
        }

        Ok(Self {
            rq_ptr,
            rq_size,
            area_ptr,
            area_size,
            area_memory_policy,
            rx_buf_len: ifq.rx_buf_len,
            rq_head: unsafe { (rq_ptr as *mut u8).add(ifq.offsets.head as usize) as *mut u32 },
            rq_tail_ptr: unsafe { (rq_ptr as *mut u8).add(ifq.offsets.tail as usize) as *mut u32 },
            rqes: unsafe {
                (rq_ptr as *mut u8).add(ifq.offsets.rqes as usize) as *mut IoUringZcrxRqe
            },
            rq_tail: 0,
            rq_entries: ifq.rq_entries,
            area_token: area.rq_area_token,
            zcrx_id: ifq.zcrx_id,
        })
    }

    fn offset_for_cqe(&self, cqe: &IoUringCqe32) -> io::Result<usize> {
        if cqe.res < 0 {
            return Err(io::Error::from_raw_os_error(-cqe.res));
        }

        let offset = (cqe.zcrx_off & !IORING_ZCRX_AREA_MASK) as usize;
        let len = cqe.res as usize;
        if offset
            .checked_add(len)
            .is_none_or(|end| end > self.area_size)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ZCRX CQE points outside registered area: offset={offset} len={len}"),
            ));
        }

        Ok(offset)
    }

    fn data_for_cqe(&self, cqe: &IoUringCqe32) -> io::Result<&[u8]> {
        let offset = self.offset_for_cqe(cqe)?;
        let len = cqe.res as usize;
        Ok(unsafe { slice::from_raw_parts((self.area_ptr as *const u8).add(offset), len) })
    }

    fn try_return_buffer(&mut self, cqe: &IoUringCqe32) -> io::Result<bool> {
        if cqe.res <= 0 {
            return Ok(true);
        }

        let queued = self
            .rq_tail
            .wrapping_sub(unsafe { ptr::read_volatile(self.rq_head) });
        if queued >= self.rq_entries {
            return Ok(false);
        }

        unsafe {
            let mask = self.rq_entries - 1;
            let rqe = self.rqes.add((self.rq_tail & mask) as usize);
            ptr::write(
                rqe,
                IoUringZcrxRqe {
                    off: (cqe.zcrx_off & !IORING_ZCRX_AREA_MASK) | self.area_token,
                    len: cqe.res as u32,
                    pad: 0,
                },
            );
            self.rq_tail = self.rq_tail.wrapping_add(1);
            fence(Ordering::Release);
            ptr::write_volatile(self.rq_tail_ptr, self.rq_tail);
        }

        Ok(true)
    }

    fn return_buffer(&mut self, cqe: &IoUringCqe32) -> io::Result<()> {
        if self.try_return_buffer(cqe)? {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "ZCRX refill queue is full",
        ))
    }
}

impl Drop for ZcrxContext {
    fn drop(&mut self) {
        munmap_if_mapped(self.rq_ptr, self.rq_size);
        munmap_if_mapped(self.area_ptr, self.area_size);
    }
}

fn if_nametoindex(ifname: &str) -> io::Result<u32> {
    let c_ifname = CString::new(ifname)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interface name contains NUL"))?;
    let ifindex = unsafe { libc::if_nametoindex(c_ifname.as_ptr()) };
    if ifindex == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ifindex)
    }
}

fn query_opcodes() -> io::Result<IoUringQueryOpcode> {
    let mut opcodes = IoUringQueryOpcode::default();
    let opcode_result = io_uring_query(IO_URING_QUERY_OPCODES, &mut opcodes)?;

    println!("io_uring opcode query result: {opcode_result}");
    println!("request opcodes: {}", opcodes.nr_request_opcodes);
    println!("register opcodes: {}", opcodes.nr_register_opcodes);
    println!("query opcodes: {}", opcodes.nr_query_opcodes);
    println!("feature flags: 0x{:x}", opcodes.feature_flags);
    println!("setup flags: 0x{:x}", opcodes.ring_setup_flags);
    println!("enter flags: 0x{:x}", opcodes.enter_flags);
    println!("sqe flags: 0x{:x}", opcodes.sqe_flags);
    println!(
        "has IORING_OP_SEND_ZC: {}",
        yes(opcodes.nr_request_opcodes > IORING_OP_SEND_ZC)
    );
    println!(
        "has IORING_OP_RECV_ZC: {}",
        yes(opcodes.nr_request_opcodes > IORING_OP_RECV_ZC)
    );
    println!(
        "has IORING_OP_READV_FIXED: {}",
        yes(opcodes.nr_request_opcodes > IORING_OP_READV_FIXED)
    );
    println!(
        "has IORING_OP_WRITEV_FIXED: {}",
        yes(opcodes.nr_request_opcodes > IORING_OP_WRITEV_FIXED)
    );
    println!(
        "has IORING_OP_PIPE: {}",
        yes(opcodes.nr_request_opcodes > IORING_OP_PIPE)
    );
    println!(
        "has IORING_OP_NOP128: {}",
        yes(opcodes.nr_request_opcodes > IORING_OP_NOP128)
    );
    println!(
        "has IORING_OP_URING_CMD128: {}",
        yes(opcodes.nr_request_opcodes > IORING_OP_URING_CMD128)
    );
    println!(
        "has IORING_OP_SLOT_RW: {}",
        yes(opcodes.nr_request_opcodes > io_slots::IORING_OP_SLOT_RW)
    );
    println!(
        "has IORING_REGISTER_NAPI: {}",
        yes(opcodes.nr_register_opcodes > IORING_REGISTER_NAPI)
    );
    println!(
        "has IORING_UNREGISTER_NAPI: {}",
        yes(opcodes.nr_register_opcodes > IORING_UNREGISTER_NAPI)
    );
    println!(
        "has IORING_REGISTER_CLONE_BUFFERS: {}",
        yes(opcodes.nr_register_opcodes > IORING_REGISTER_CLONE_BUFFERS)
    );
    println!(
        "has IORING_REGISTER_SEND_MSG_RING: {}",
        yes(opcodes.nr_register_opcodes > IORING_REGISTER_SEND_MSG_RING)
    );
    println!(
        "has IORING_REGISTER_RESIZE_RINGS: {}",
        yes(opcodes.nr_register_opcodes > IORING_REGISTER_RESIZE_RINGS)
    );
    println!(
        "has IORING_REGISTER_MEM_REGION: {}",
        yes(opcodes.nr_register_opcodes > IORING_REGISTER_MEM_REGION)
    );
    println!(
        "has IORING_REGISTER_ZCRX_IFQ: {}",
        yes(opcodes.nr_register_opcodes > IORING_REGISTER_ZCRX_IFQ)
    );
    println!(
        "has IORING_REGISTER_ZCRX_CTRL: {}",
        yes(opcodes.nr_register_opcodes > IORING_REGISTER_ZCRX_CTRL)
    );
    println!(
        "has IORING_REGISTER_BPF_FILTER: {}",
        yes(opcodes.nr_register_opcodes > IORING_REGISTER_BPF_FILTER)
    );
    println!(
        "has IORING_REGISTER_IO_SLOT: {}",
        yes(opcodes.nr_register_opcodes > io_slots::IORING_REGISTER_IO_SLOT)
    );
    println!(
        "has IORING_UNREGISTER_IO_SLOT: {}",
        yes(opcodes.nr_register_opcodes > io_slots::IORING_UNREGISTER_IO_SLOT)
    );
    println!(
        "has IORING_SETUP_DEFER_TASKRUN: {}",
        yes((opcodes.ring_setup_flags & IORING_SETUP_DEFER_TASKRUN) != 0)
    );
    println!(
        "has IORING_SETUP_CQE_MIXED: {}",
        yes((opcodes.ring_setup_flags & IORING_SETUP_CQE_MIXED) != 0)
    );
    println!(
        "has IORING_SETUP_SQE_MIXED: {}",
        yes((opcodes.ring_setup_flags & IORING_SETUP_SQE_MIXED) != 0)
    );
    println!(
        "has IORING_SETUP_SQ_REWIND: {}",
        yes((opcodes.ring_setup_flags & IORING_SETUP_SQ_REWIND) != 0)
    );
    println!(
        "has IORING_FEAT_RECVSEND_BUNDLE: {}",
        yes((opcodes.feature_flags & IORING_FEAT_RECVSEND_BUNDLE) != 0)
    );
    println!(
        "has IORING_ENTER_NO_IOWAIT: {}",
        yes((opcodes.enter_flags & IORING_ENTER_NO_IOWAIT as u64) != 0)
    );

    Ok(opcodes)
}

fn print_io_uring_api_opportunities(opcodes: &IoUringQueryOpcode) {
    println!("io_uring api opportunities:");
    println!(
        "  io-slots WAL path: {}",
        if opcodes.nr_request_opcodes > io_slots::IORING_OP_SLOT_RW
            && opcodes.nr_register_opcodes > io_slots::IORING_REGISTER_IO_SLOT
        {
            "active in slot-wal-bench, tcp-wal-mux-server, udp-wal-mux-server"
        } else {
            "unavailable on this kernel"
        }
    );
    println!(
        "  network send zerocopy: {}",
        if opcodes.nr_request_opcodes > IORING_OP_SEND_ZC {
            "active with tcp-bench-uring-mux-send send-zc/fixed-zc"
        } else {
            "unavailable on this kernel"
        }
    );
    println!(
        "  network receive zerocopy: {}",
        if opcodes.nr_request_opcodes > IORING_OP_RECV_ZC
            && opcodes.nr_register_opcodes > IORING_REGISTER_ZCRX_IFQ
        {
            "active with tcp-bench-uring-mux-server zcrx and tcp-wal-mux-server URING_PLAY_TCP_WAL_MODE=zcrx"
        } else {
            "unavailable on this kernel"
        }
    );
    println!(
        "  io_uring NAPI busy poll: {}",
        if opcodes.nr_register_opcodes > IORING_REGISTER_NAPI {
            "available; enable with URING_PLAY_REGISTER_NAPI=1 or URING_PLAY_NAPI_BUSY_POLL_US=<us>"
        } else {
            "unavailable on this kernel"
        }
    );
    println!(
        "  no-iowait enter flag: {}",
        if (opcodes.enter_flags & IORING_ENTER_NO_IOWAIT as u64) != 0 {
            "available; enable with URING_PLAY_ENTER_NO_IOWAIT=1"
        } else {
            "unavailable on this kernel"
        }
    );
    println!(
        "  recv/send bundle: {}",
        if (opcodes.feature_flags & IORING_FEAT_RECVSEND_BUNDLE) != 0 {
            "available; candidate for provided-buffer UDP/TCP batching"
        } else {
            "unavailable on this kernel"
        }
    );
    println!(
        "  clone registered buffers: {}",
        if opcodes.nr_register_opcodes > IORING_REGISTER_CLONE_BUFFERS {
            "available; candidate for split RX/WAL rings sharing one pinned buffer table"
        } else {
            "unavailable on this kernel"
        }
    );
    println!(
        "  BPF ring filter: {}",
        if opcodes.nr_register_opcodes > IORING_REGISTER_BPF_FILTER {
            "available; candidate for CQE filtering/classification once we add verifier-safe programs"
        } else {
            "unavailable on this kernel"
        }
    );
    println!(
        "  SQ/CQE mixed + SQ rewind: {}",
        if (opcodes.ring_setup_flags
            & (IORING_SETUP_CQE_MIXED | IORING_SETUP_SQE_MIXED | IORING_SETUP_SQ_REWIND))
            != 0
        {
            "available; candidate for raw-ring overhead experiments"
        } else {
            "unavailable on this kernel"
        }
    );
}

fn query_zcrx_raw() -> io::Result<(IoUringQueryZcrx, i32)> {
    let mut zcrx = IoUringQueryZcrx::default();
    let zcrx_result = io_uring_query(IO_URING_QUERY_ZCRX, &mut zcrx)?;

    Ok((zcrx, zcrx_result))
}

fn query_zcrx() -> io::Result<IoUringQueryZcrx> {
    let (zcrx, zcrx_result) = query_zcrx_raw()?;

    println!("zcrx query result: {zcrx_result}");
    println!("zcrx register flags: 0x{:x}", zcrx.register_flags);
    println!("zcrx area flags: 0x{:x}", zcrx.area_flags);
    println!("zcrx control opcodes: {}", zcrx.nr_ctrl_opcodes);
    println!("zcrx features: 0x{:x}", zcrx.features);
    println!("zcrx refill header size: {}", zcrx.rq_hdr_size);
    println!("zcrx refill header alignment: {}", zcrx.rq_hdr_alignment);
    println!(
        "zcrx supports imported contexts: {}",
        yes((zcrx.register_flags & ZCRX_REG_IMPORT) != 0)
    );
    println!(
        "zcrx supports dmabuf areas: {}",
        yes((zcrx.area_flags & IORING_ZCRX_AREA_DMABUF) != 0)
    );
    println!(
        "zcrx supports selectable rx page size: {}",
        yes((zcrx.features & ZCRX_FEATURE_RX_PAGE_SIZE) != 0)
    );
    println!(
        "current process has CAP_NET_ADMIN: {}",
        yes(has_cap_net_admin().unwrap_or(false))
    );

    Ok(zcrx)
}

fn probe() -> io::Result<()> {
    let opcodes = query_opcodes()?;
    print_io_uring_api_opportunities(&opcodes);
    println!("local io-slot compat: {}", io_slots::local_compat_summary());
    println!(
        "upstream liburing io-slot helpers: {}",
        io_slots::liburing_slot_helper_probe().summary()
    );
    query_zcrx()?;
    println!(
        "rdma steering: run rdma-probe and rdma-plan; RDMA lanes map to QPs/CQs or libfabric endpoints, not userspace 5-tuple reassembly"
    );
    Ok(())
}

#[derive(Clone)]
struct CommandReport {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

fn command_report(program: &str, args: &[&str]) -> io::Result<CommandReport> {
    let output = Command::new(program).args(args).output()?;
    Ok(CommandReport {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

fn print_command_report(label: &str, program: &str, args: &[&str]) {
    let Some(path) = program_in_path(program) else {
        println!("{label}: command {program:?} not found");
        return;
    };

    match command_report(path.to_string_lossy().as_ref(), args) {
        Ok(report) => {
            let status = report
                .status
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string());
            println!("{label}: status={status}");
            if report.stdout.is_empty() && report.stderr.is_empty() {
                println!("{label}: <no output>");
            } else {
                for line in report.stdout.lines() {
                    println!("{label}: stdout: {line}");
                }
                for line in report.stderr.lines() {
                    println!("{label}: stderr: {line}");
                }
            }
        }
        Err(err) => println!("{label}: failed: {err}"),
    }
}

fn program_in_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return path.is_file().then(|| path.to_path_buf());
    }

    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn read_trimmed_path<P: AsRef<Path>>(path: P) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn list_dir_entry_names(path: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };

    let mut names = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn symlink_basename(path: &Path) -> Option<String> {
    fs::read_link(path).ok().and_then(|target| {
        target
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.to_string())
    })
}

fn file_or_symlink_value(path: &Path) -> Option<String> {
    read_trimmed_path(path).or_else(|| symlink_basename(path))
}

fn runtime_library_path(soname: &str, candidates: &[&str]) -> Option<String> {
    for ldconfig in ["/sbin/ldconfig", "/usr/sbin/ldconfig", "ldconfig"] {
        let Some(path) = program_in_path(ldconfig) else {
            continue;
        };
        if let Ok(report) = command_report(path.to_string_lossy().as_ref(), &["-p"]) {
            for line in report.stdout.lines() {
                if !line.contains(soname) {
                    continue;
                }
                if let Some((_, path)) = line.rsplit_once("=>") {
                    return Some(path.trim().to_string());
                }
            }
        }
    }

    candidates
        .iter()
        .find(|path| Path::new(path).exists())
        .map(|path| path.to_string())
}

fn print_presence(label: &str, value: bool) {
    println!("{label}: {}", yes(value));
}

struct RdmaSysfsPort {
    port: String,
    link_layer: Option<String>,
    state: Option<String>,
    rate: Option<String>,
    gids: Vec<String>,
    gid_types: Vec<String>,
    gid_netdevs: Vec<String>,
}

struct RdmaSysfsDevice {
    name: String,
    driver: Option<String>,
    node_type: Option<String>,
    fw_ver: Option<String>,
    ports: Vec<RdmaSysfsPort>,
}

fn read_rdma_sysfs_devices() -> Vec<RdmaSysfsDevice> {
    let root = Path::new("/sys/class/infiniband");
    let mut devices = Vec::new();

    for name in list_dir_entry_names(root) {
        let base = root.join(&name);
        let ports_dir = base.join("ports");
        let mut ports = Vec::new();

        for port in list_dir_entry_names(&ports_dir) {
            let port_dir = ports_dir.join(&port);
            let gid_types = list_dir_entry_names(&port_dir.join("gid_attrs/types"))
                .into_iter()
                .filter_map(|entry| read_trimmed_path(port_dir.join("gid_attrs/types").join(entry)))
                .collect::<Vec<_>>();
            let gid_netdevs = list_dir_entry_names(&port_dir.join("gid_attrs/ndevs"))
                .into_iter()
                .filter_map(|entry| {
                    file_or_symlink_value(&port_dir.join("gid_attrs/ndevs").join(entry))
                })
                .collect::<Vec<_>>();
            let gids = list_dir_entry_names(&port_dir.join("gids"))
                .into_iter()
                .filter_map(|entry| read_trimmed_path(port_dir.join("gids").join(entry)))
                .collect::<Vec<_>>();

            ports.push(RdmaSysfsPort {
                port,
                link_layer: read_trimmed_path(port_dir.join("link_layer")),
                state: read_trimmed_path(port_dir.join("state")),
                rate: read_trimmed_path(port_dir.join("rate")),
                gids,
                gid_types,
                gid_netdevs,
            });
        }

        devices.push(RdmaSysfsDevice {
            name,
            driver: symlink_basename(&base.join("device/driver")),
            node_type: read_trimmed_path(base.join("node_type")),
            fw_ver: read_trimmed_path(base.join("fw_ver")),
            ports,
        });
    }

    devices
}

fn count_netdev_queues(netdev: &str, prefix: &str) -> usize {
    let path = Path::new("/sys/class/net").join(netdev).join("queues");
    list_dir_entry_names(&path)
        .into_iter()
        .filter(|name| name.starts_with(prefix))
        .count()
}

fn print_netdev_rdma_summary(netdev: &str) -> io::Result<()> {
    let base = Path::new("/sys/class/net").join(netdev);
    if !base.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("netdev {netdev:?} does not exist"),
        ));
    }

    println!("rdma-netdev: name={netdev}");
    println!(
        "rdma-netdev: operstate={} mtu={} speed={} numa_node={} driver={} rx_queues={} tx_queues={}",
        read_trimmed_path(base.join("operstate")).unwrap_or_else(|| "unknown".to_string()),
        read_trimmed_path(base.join("mtu")).unwrap_or_else(|| "unknown".to_string()),
        read_trimmed_path(base.join("speed")).unwrap_or_else(|| "unknown".to_string()),
        read_trimmed_path(base.join("device/numa_node")).unwrap_or_else(|| "unknown".to_string()),
        symlink_basename(&base.join("device/driver")).unwrap_or_else(|| "unknown".to_string()),
        count_netdev_queues(netdev, "rx-"),
        count_netdev_queues(netdev, "tx-"),
    );
    Ok(())
}

fn rdma_kernel_module_available(module: &str) -> bool {
    let release = read_trimmed_path("/proc/sys/kernel/osrelease").unwrap_or_default();
    if release.is_empty() {
        return false;
    }
    let module_dir = Path::new("/lib/modules").join(release);
    let needles = [
        format!("{module}.ko"),
        format!("{module}.ko.xz"),
        format!("{module}.ko.zst"),
        format!("{module}.ko.gz"),
    ];

    let mut stack = vec![module_dir];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                if needles.iter().any(|needle| needle == name) {
                    return true;
                }
            }
        }
    }
    false
}

fn print_rdma_device_summary(devices: &[RdmaSysfsDevice]) {
    if devices.is_empty() {
        println!("rdma-device: none");
        return;
    }

    for device in devices {
        println!(
            "rdma-device: name={} driver={} node_type={} fw_ver={}",
            device.name,
            device.driver.as_deref().unwrap_or("unknown"),
            device.node_type.as_deref().unwrap_or("unknown"),
            device.fw_ver.as_deref().unwrap_or("unknown"),
        );
        if device.ports.is_empty() {
            println!("rdma-port: dev={} ports=none", device.name);
        }
        for port in &device.ports {
            println!(
                "rdma-port: dev={} port={} link_layer={} state={} rate={} gids={} gid_types={} gid_netdevs={}",
                device.name,
                port.port,
                port.link_layer.as_deref().unwrap_or("unknown"),
                port.state.as_deref().unwrap_or("unknown"),
                port.rate.as_deref().unwrap_or("unknown"),
                port.gids.len(),
                if port.gid_types.is_empty() {
                    "none".to_string()
                } else {
                    port.gid_types.join(",")
                },
                if port.gid_netdevs.is_empty() {
                    "none".to_string()
                } else {
                    port.gid_netdevs.join(",")
                },
            );
        }
    }
}

fn rdma_device_exists(name: &str) -> bool {
    Path::new("/sys/class/infiniband").join(name).exists()
}

fn rdma_device_for_netdev(netdev: &str) -> Option<String> {
    read_rdma_sysfs_devices()
        .into_iter()
        .find(|device| {
            device
                .ports
                .iter()
                .any(|port| port.gid_netdevs.iter().any(|name| name == netdev))
        })
        .map(|device| device.name)
}

fn rdma_probe(netdev: Option<&str>) -> io::Result<()> {
    println!(
        "rdma-probe: kernel={}",
        read_trimmed_path("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".to_string())
    );
    print_presence("rdma-tool-rdma", program_in_path("rdma").is_some());
    print_presence(
        "rdma-tool-ibv_devices",
        program_in_path("ibv_devices").is_some(),
    );
    print_presence(
        "rdma-tool-ibv_devinfo",
        program_in_path("ibv_devinfo").is_some(),
    );
    print_presence("rdma-tool-fi_info", program_in_path("fi_info").is_some());
    println!(
        "rdma-lib-libibverbs: {}",
        runtime_library_path(
            "libibverbs.so",
            &[
                "/lib/x86_64-linux-gnu/libibverbs.so.1",
                "/usr/lib/x86_64-linux-gnu/libibverbs.so.1"
            ],
        )
        .unwrap_or_else(|| "not-found".to_string())
    );
    println!(
        "rdma-lib-librdmacm: {}",
        runtime_library_path(
            "librdmacm.so",
            &[
                "/lib/x86_64-linux-gnu/librdmacm.so.1",
                "/usr/lib/x86_64-linux-gnu/librdmacm.so.1"
            ],
        )
        .unwrap_or_else(|| "not-found".to_string())
    );
    println!(
        "rdma-lib-libfabric: {}",
        runtime_library_path(
            "libfabric.so",
            &[
                "/lib/x86_64-linux-gnu/libfabric.so.1",
                "/usr/lib/x86_64-linux-gnu/libfabric.so.1"
            ],
        )
        .unwrap_or_else(|| "not-found".to_string())
    );
    print_presence(
        "rdma-header-verbs",
        Path::new("/usr/include/infiniband/verbs.h").exists(),
    );
    print_presence(
        "rdma-header-rdmacm",
        Path::new("/usr/include/rdma/rdma_cma.h").exists(),
    );
    print_presence(
        "rdma-header-libfabric",
        Path::new("/usr/include/rdma/fabric.h").exists(),
    );
    print_presence("rdma-module-rxe", rdma_kernel_module_available("rdma_rxe"));
    print_presence("rdma-module-siw", rdma_kernel_module_available("siw"));

    let char_devs = list_dir_entry_names(Path::new("/dev/infiniband"));
    println!(
        "rdma-dev-infiniband: {}",
        if char_devs.is_empty() {
            "none".to_string()
        } else {
            char_devs.join(",")
        }
    );

    let devices = read_rdma_sysfs_devices();
    print_rdma_device_summary(&devices);
    if let Some(netdev) = netdev {
        print_netdev_rdma_summary(netdev)?;
    }

    print_command_report("rdma-cmd-dev", "rdma", &["dev"]);
    print_command_report("rdma-cmd-link", "rdma", &["link"]);
    if program_in_path("fi_info").is_some() {
        print_command_report("rdma-cmd-fi-efa", "fi_info", &["-p", "efa"]);
        print_command_report("rdma-cmd-fi-verbs", "fi_info", &["-p", "verbs"]);
    }

    println!(
        "rdma-model: ConnectX/RoCE should use one or more QPs per peer for entropy; RoCEv2 UDP source port is provider/HCA flow entropy, not an application reassembly key"
    );
    println!(
        "rdma-model: EFA should use libfabric FI_EP_RDM/FI_AV_TABLE lanes; SRD segmentation and reassembly are provider/device work"
    );
    println!(
        "rdma-next: if no RDMA devices exist and rxe is available, use rdma-rxe-add <netdev> [rxe-name] for a correctness-only Soft-RoCE device"
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum RdmaFabricPlan {
    Auto,
    ConnectxRoce,
    AwsEfa,
    AwsEfaDirect,
    InfiniBand,
    SoftRoce,
}

impl RdmaFabricPlan {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "" | "auto" => Ok(Self::Auto),
            "connectx" | "roce" | "rocev2" | "mlx5" => Ok(Self::ConnectxRoce),
            "efa" | "aws-efa" => Ok(Self::AwsEfa),
            "efa-direct" | "aws-efa-direct" => Ok(Self::AwsEfaDirect),
            "ib" | "infiniband" => Ok(Self::InfiniBand),
            "rxe" | "soft-roce" | "softroce" => Ok(Self::SoftRoce),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown RDMA fabric {other:?}; use auto, roce, efa, efa-direct, ib, or rxe"
                ),
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::ConnectxRoce => "roce",
            Self::AwsEfa => "efa",
            Self::AwsEfaDirect => "efa-direct",
            Self::InfiniBand => "ib",
            Self::SoftRoce => "rxe",
        }
    }

    fn reassembly_owner(self) -> &'static str {
        match self {
            Self::AwsEfa | Self::AwsEfaDirect => "efa-srd-provider",
            Self::Auto | Self::ConnectxRoce | Self::InfiniBand | Self::SoftRoce => "hca-qp-psn",
        }
    }

    fn lane_object(self) -> &'static str {
        match self {
            Self::AwsEfa => "fi_endpoint_rdm",
            Self::AwsEfaDirect => "fi_endpoint_rdm_direct",
            Self::Auto | Self::ConnectxRoce | Self::InfiniBand | Self::SoftRoce => "rc_qp",
        }
    }

    fn entropy_model(self) -> &'static str {
        match self {
            Self::AwsEfa => "fi_av_table+provider-srd-path-selection",
            Self::AwsEfaDirect => "fi_av_table+efa-direct-device-fastpath",
            Self::InfiniBand => "lid/sl/path+qp",
            Self::SoftRoce => "rocev2-udp-src-port-from-rxe-qp-cpu-slow",
            Self::Auto | Self::ConnectxRoce => "rocev2-udp-src-port-from-hca-qp-entropy",
        }
    }

    fn rx_distribution_model(self) -> &'static str {
        match self {
            Self::AwsEfa | Self::AwsEfaDirect => "cq-per-endpoint-lane",
            Self::InfiniBand => "cq-per-qp-lane-no-ip-5tuple",
            Self::SoftRoce => "software-rxe-cq-per-qp-lane",
            Self::Auto | Self::ConnectxRoce => {
                "cq-per-qp-lane; raw-packet/RSS can use ibv_rwq_ind_table when applicable"
            }
        }
    }
}

fn rdma_plan(
    fabric: RdmaFabricPlan,
    peers: usize,
    lanes_per_peer: usize,
    workers: usize,
) -> io::Result<()> {
    if peers == 0 || lanes_per_peer == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "peers and lanes-per-peer must be greater than zero",
        ));
    }
    let total_lanes = peers.checked_mul(lanes_per_peer).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "peers times lanes overflows")
    })?;
    let workers = tcp_bench_auto_workers(workers, total_lanes);

    println!(
        "rdma-plan: fabric={} peers={peers} lanes_per_peer={lanes_per_peer} total_lanes={total_lanes} workers={workers}",
        fabric.label()
    );
    println!(
        "rdma-plan: lane_object={} reassembly_owner={} entropy={} rx_distribution={}",
        fabric.lane_object(),
        fabric.reassembly_owner(),
        fabric.entropy_model(),
        fabric.rx_distribution_model(),
    );
    println!(
        "rdma-plan: app-rule=no-userspace-5tuple-reassembly; raft-stream-id=(peer,lane); transport-reassembly={}",
        fabric.reassembly_owner()
    );

    for peer in 0..peers {
        for lane in 0..lanes_per_peer {
            let global = peer * lanes_per_peer + lane;
            let worker = global % workers;
            let completion_cq = worker;
            let wal_shard = global;
            let mr_region = worker;
            println!(
                "rdma-lane: peer={peer} lane={lane} global={global} worker={worker} \
                 {}={global} send_cq={completion_cq} recv_cq={completion_cq} \
                 mr_region={mr_region} wal_shard={wal_shard} entropy={}",
                fabric.lane_object(),
                fabric.entropy_model(),
            );
        }
    }

    match fabric {
        RdmaFabricPlan::AwsEfa => {
            println!(
                "rdma-efa-note: prefer FI_EP_RDM with FI_AV_TABLE for Raft messages; use FI_MULTI_RECV for posted receive pools and CQ-per-worker polling"
            );
        }
        RdmaFabricPlan::AwsEfaDirect => {
            println!(
                "rdma-efa-note: efa-direct is a fast-path candidate for fixed-size WAL chunks but has stricter mode and message-size limits"
            );
        }
        RdmaFabricPlan::ConnectxRoce | RdmaFabricPlan::Auto | RdmaFabricPlan::SoftRoce => {
            println!(
                "rdma-roce-note: use multiple RC QPs per remote peer for ECMP/source-port entropy; use ibv_create_qp_ex+rwq_ind_tbl only for RSS-style receive steering where the QP type supports it"
            );
        }
        RdmaFabricPlan::InfiniBand => {
            println!(
                "rdma-ib-note: no IP/UDP 5-tuple exists on native IB; use path/SL/QP fanout and CQ-per-worker locality"
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum TransportPathPlan {
    AwsTcpPublic,
    AwsTcpVpc,
    AwsTcpCluster,
    AwsTcpEnaExpress,
    AwsTcpInterRegion,
    RdmaRoce,
    RdmaEfa,
    RdmaEfaDirect,
    RdmaInfiniBand,
    RdmaRxe,
}

impl TransportPathPlan {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "tcp-public" | "public" | "aws-public" | "efa-tcp" | "public-efa-tcp" => {
                Ok(Self::AwsTcpPublic)
            }
            "tcp-vpc" | "vpc" | "aws-vpc" | "private" => Ok(Self::AwsTcpVpc),
            "tcp-cluster" | "cluster" | "placement-group" | "cpg" => Ok(Self::AwsTcpCluster),
            "tcp-ena-express" | "ena-express" | "srd-tcp" => Ok(Self::AwsTcpEnaExpress),
            "tcp-inter-region" | "inter-region" | "cross-region" => Ok(Self::AwsTcpInterRegion),
            "rdma-roce" | "roce" | "connectx" | "mlx5" => Ok(Self::RdmaRoce),
            "rdma-efa" | "efa-rdma" | "efa" => Ok(Self::RdmaEfa),
            "rdma-efa-direct" | "efa-direct" => Ok(Self::RdmaEfaDirect),
            "rdma-ib" | "ib" | "infiniband" => Ok(Self::RdmaInfiniBand),
            "rdma-rxe" | "rxe" | "soft-roce" | "softroce" => Ok(Self::RdmaRxe),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown transport path {other:?}; use tcp-public, tcp-vpc, tcp-cluster, \
                     tcp-ena-express, tcp-inter-region, rdma-roce, rdma-efa, \
                     rdma-efa-direct, rdma-ib, or rdma-rxe"
                ),
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::AwsTcpPublic => "tcp-public",
            Self::AwsTcpVpc => "tcp-vpc",
            Self::AwsTcpCluster => "tcp-cluster",
            Self::AwsTcpEnaExpress => "tcp-ena-express",
            Self::AwsTcpInterRegion => "tcp-inter-region",
            Self::RdmaRoce => "rdma-roce",
            Self::RdmaEfa => "rdma-efa",
            Self::RdmaEfaDirect => "rdma-efa-direct",
            Self::RdmaInfiniBand => "rdma-ib",
            Self::RdmaRxe => "rdma-rxe",
        }
    }

    fn is_tcp(self) -> bool {
        matches!(
            self,
            Self::AwsTcpPublic
                | Self::AwsTcpVpc
                | Self::AwsTcpCluster
                | Self::AwsTcpEnaExpress
                | Self::AwsTcpInterRegion
        )
    }

    fn lane_object(self) -> &'static str {
        match self {
            Self::AwsTcpPublic
            | Self::AwsTcpVpc
            | Self::AwsTcpCluster
            | Self::AwsTcpEnaExpress
            | Self::AwsTcpInterRegion => "5tuple",
            Self::RdmaRoce | Self::RdmaInfiniBand | Self::RdmaRxe => "qp",
            Self::RdmaEfa => "fi_endpoint",
            Self::RdmaEfaDirect => "fi_endpoint_direct",
        }
    }

    fn steering_model(self) -> &'static str {
        match self {
            Self::AwsTcpPublic | Self::AwsTcpInterRegion => {
                "many TCP/UDP 5-tuples across AWS SDN public/cross-region path"
            }
            Self::AwsTcpVpc => "many TCP/UDP 5-tuples across normal VPC path",
            Self::AwsTcpCluster => "many TCP/UDP 5-tuples inside a cluster placement group",
            Self::AwsTcpEnaExpress => "TCP/UDP flow fanout with ENA Express/SRD when available",
            Self::RdmaRoce => "many RC QPs per peer for HCA/ECMP entropy",
            Self::RdmaEfa => "libfabric FI_EP_RDM endpoints/address-vector lanes",
            Self::RdmaEfaDirect => "EFA direct endpoints for fixed-size fast-path lanes",
            Self::RdmaInfiniBand => "native IB QPs and path/SL fanout",
            Self::RdmaRxe => "software RoCE QPs for correctness testing",
        }
    }

    fn default_tcp_flow_gbps(self) -> Option<f64> {
        match self {
            Self::AwsTcpPublic | Self::AwsTcpVpc | Self::AwsTcpInterRegion => Some(5.0),
            Self::AwsTcpCluster => Some(10.0),
            Self::AwsTcpEnaExpress => Some(25.0),
            Self::RdmaRoce
            | Self::RdmaEfa
            | Self::RdmaEfaDirect
            | Self::RdmaInfiniBand
            | Self::RdmaRxe => None,
        }
    }

    fn fallback_path(self) -> &'static str {
        match self {
            Self::AwsTcpPublic | Self::AwsTcpInterRegion => "primary for inter-region/public",
            Self::AwsTcpVpc | Self::AwsTcpCluster | Self::AwsTcpEnaExpress => {
                "fallback when RDMA/libfabric is unavailable"
            }
            Self::RdmaRoce | Self::RdmaEfa | Self::RdmaEfaDirect | Self::RdmaInfiniBand => {
                "primary only when peers share supported RDMA fabric"
            }
            Self::RdmaRxe => "test-only; not a throughput target",
        }
    }
}

fn parse_gbps_arg(value: &str, name: &str) -> io::Result<f64> {
    let value = value.trim();
    let gbps = value.parse::<f64>().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name}={value:?} is not a valid Gbps value: {err}"),
        )
    })?;
    if !gbps.is_finite() || gbps <= 0.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be finite and greater than zero"),
        ));
    }
    Ok(gbps)
}

fn env_gbps_or(name: &str, default: f64) -> io::Result<f64> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => parse_gbps_arg(&value, name),
        _ => Ok(default),
    }
}

fn env_gbps_opt(name: &str) -> io::Result<Option<f64>> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => parse_gbps_arg(&value, name).map(Some),
        _ => Ok(None),
    }
}

fn lanes_for_gbps(target_gbps: f64, per_lane_gbps: f64, min_lanes: usize) -> usize {
    ((target_gbps / per_lane_gbps).ceil() as usize)
        .max(1)
        .max(min_lanes)
}

fn transport_path_plan(
    transport: TransportPathPlan,
    target_peer_gbps: f64,
    peers: usize,
    min_lanes_per_peer: usize,
    workers: usize,
) -> io::Result<()> {
    if peers == 0 || min_lanes_per_peer == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "peers and min-lanes-per-peer must be greater than zero",
        ));
    }

    let per_lane_gbps = if transport.is_tcp() {
        env_gbps_or(
            "URING_PLAY_TCP_FLOW_GBPS",
            transport.default_tcp_flow_gbps().unwrap_or(5.0),
        )?
    } else {
        env_gbps_opt("URING_PLAY_RDMA_LANE_GBPS")?.unwrap_or(0.0)
    };
    let lanes_per_peer = if per_lane_gbps > 0.0 {
        lanes_for_gbps(target_peer_gbps, per_lane_gbps, min_lanes_per_peer)
    } else {
        min_lanes_per_peer
    };
    let total_lanes = peers.checked_mul(lanes_per_peer).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "peers times lanes overflows")
    })?;
    let workers = tcp_bench_auto_workers(workers, total_lanes);

    println!(
        "path-plan: transport={} target_peer_gbps={target_peer_gbps:.3} peers={peers} \
         lanes_per_peer={lanes_per_peer} total_lanes={total_lanes} workers={workers}",
        transport.label()
    );
    println!(
        "path-plan: lane_object={} steering={} role={}",
        transport.lane_object(),
        transport.steering_model(),
        transport.fallback_path()
    );
    if per_lane_gbps > 0.0 {
        println!(
            "path-plan: assumed_per_lane_gbps={per_lane_gbps:.3} aggregate_peer_budget_gbps={:.3} override_env={}",
            per_lane_gbps * lanes_per_peer as f64,
            if transport.is_tcp() {
                "URING_PLAY_TCP_FLOW_GBPS"
            } else {
                "URING_PLAY_RDMA_LANE_GBPS"
            }
        );
    } else {
        println!(
            "path-plan: assumed_per_lane_gbps=unknown aggregate_peer_budget_gbps=unknown override_env=URING_PLAY_RDMA_LANE_GBPS"
        );
    }

    for peer in 0..peers {
        for lane in 0..lanes_per_peer {
            let global = peer * lanes_per_peer + lane;
            let worker = global % workers;
            if transport.is_tcp() {
                println!(
                    "path-lane: peer={peer} lane={lane} global={global} worker={worker} \
                     dst_port_lane={lane} src_port_lane={global} object=5tuple \
                     wal_shard={global}"
                );
            } else {
                println!(
                    "path-lane: peer={peer} lane={lane} global={global} worker={worker} \
                     object={} object_id={global} cq={worker} mr_region={worker} wal_shard={global}",
                    transport.lane_object()
                );
            }
        }
    }

    if transport.is_tcp() {
        println!(
            "path-plan-tcp-note: use our mux ports plus source-port pinning to create distinct 5-tuples; \
             set URING_PLAY_SOURCE_PORT_BASE and URING_PLAY_SOURCE_PORT_STRIDE when the kernel's ephemeral choice is not enough"
        );
        println!(
            "path-plan-tcp-note: this is the path for public IP, inter-region, non-EFA peers, and any deployment where RDMA CM/libfabric cannot form a fabric"
        );
    } else {
        println!(
            "path-plan-rdma-note: use TCP mux as a negotiated fallback for peers outside the RDMA fabric or across regions"
        );
    }

    Ok(())
}

fn rdma_rxe_add(netdev: &str, name: Option<&str>) -> io::Result<()> {
    let netdev_path = Path::new("/sys/class/net").join(netdev);
    if !netdev_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("netdev {netdev:?} does not exist"),
        ));
    }
    if !rdma_kernel_module_available("rdma_rxe") {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "rdma_rxe module is not present for this kernel",
        ));
    }

    let name = name
        .map(|value| value.to_string())
        .unwrap_or_else(|| "rxe0".to_string());
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "RXE device name may only contain ASCII letters, digits, '_' and '-'",
        ));
    }
    if Path::new("/sys/class/infiniband").join(&name).exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("RDMA device {name:?} already exists"),
        ));
    }

    let modprobe = command_report("sudo", &["-n", "modprobe", "rdma_rxe"])?;
    if modprobe.status != Some(0) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("sudo -n modprobe rdma_rxe failed: {}", modprobe.stderr),
        ));
    }
    let add = command_report(
        "sudo",
        &[
            "-n", "rdma", "link", "add", &name, "type", "rxe", "netdev", netdev,
        ],
    )?;
    if add.status != Some(0) {
        return Err(io::Error::other(format!(
            "sudo -n rdma link add failed: {}",
            if add.stderr.is_empty() {
                add.stdout
            } else {
                add.stderr
            }
        )));
    }

    println!("rdma-rxe-add: created {name} on netdev {netdev}");
    rdma_probe(Some(netdev))
}

fn rdma_rxe_create_quiet(netdev: &str, name: &str) -> io::Result<()> {
    let netdev_path = Path::new("/sys/class/net").join(netdev);
    if !netdev_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("netdev {netdev:?} does not exist"),
        ));
    }
    let modprobe = command_report("sudo", &["-n", "modprobe", "rdma_rxe"])?;
    if modprobe.status != Some(0) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("sudo -n modprobe rdma_rxe failed: {}", modprobe.stderr),
        ));
    }
    let add = command_report(
        "sudo",
        &[
            "-n", "rdma", "link", "add", name, "type", "rxe", "netdev", netdev,
        ],
    )?;
    if add.status != Some(0) {
        return Err(io::Error::other(format!(
            "sudo -n rdma link add failed: {}",
            if add.stderr.is_empty() {
                add.stdout
            } else {
                add.stderr
            }
        )));
    }
    Ok(())
}

fn rdma_rxe_del(name: &str) -> io::Result<()> {
    if !Path::new("/sys/class/infiniband").join(name).exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("RDMA device {name:?} does not exist"),
        ));
    }
    let del = command_report("sudo", &["-n", "rdma", "link", "del", name])?;
    if del.status != Some(0) {
        return Err(io::Error::other(format!(
            "sudo -n rdma link del failed: {}",
            if del.stderr.is_empty() {
                del.stdout
            } else {
                del.stderr
            }
        )));
    }
    println!("rdma-rxe-del: removed {name}");
    Ok(())
}

fn print_command_report_lines(label: &str, report: &CommandReport) {
    let status = report
        .status
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());
    println!("{label}: status={status}");
    if report.stdout.is_empty() && report.stderr.is_empty() {
        println!("{label}: <no output>");
        return;
    }
    for line in report.stdout.lines() {
        println!("{label}: stdout: {line}");
    }
    for line in report.stderr.lines() {
        println!("{label}: stderr: {line}");
    }
}

fn print_command_report_lines_limited(label: &str, report: &CommandReport, max_lines: usize) {
    let status = report
        .status
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());
    println!("{label}: status={status}");
    let mut printed = 0usize;
    for line in report.stdout.lines() {
        if printed >= max_lines {
            println!("{label}: stdout: ... truncated");
            break;
        }
        println!("{label}: stdout: {line}");
        printed += 1;
    }
    for line in report.stderr.lines() {
        if printed >= max_lines {
            println!("{label}: stderr: ... truncated");
            break;
        }
        println!("{label}: stderr: {line}");
        printed += 1;
    }
    if printed == 0 {
        println!("{label}: <no output>");
    }
}

fn wait_child_report(
    mut child: std::process::Child,
    timeout: Duration,
) -> io::Result<CommandReport> {
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            return Ok(CommandReport {
                status: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            return Ok(CommandReport {
                status: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                stderr: format!(
                    "{}{}timed out after {:.3}s",
                    String::from_utf8_lossy(&output.stderr).trim(),
                    if output.stderr.is_empty() { "" } else { "\n" },
                    timeout.as_secs_f64()
                ),
            });
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn run_pingpong_pair(
    label: &str,
    program: &str,
    server_args: &[String],
    client_args: &[String],
) -> io::Result<()> {
    let server = Command::new(program)
        .args(server_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    thread::sleep(Duration::from_millis(300));

    let client_output = Command::new(program).args(client_args).output()?;
    let client = CommandReport {
        status: client_output.status.code(),
        stdout: String::from_utf8_lossy(&client_output.stdout)
            .trim()
            .to_string(),
        stderr: String::from_utf8_lossy(&client_output.stderr)
            .trim()
            .to_string(),
    };
    let server = wait_child_report(server, Duration::from_secs(5))?;

    print_command_report_lines(&format!("{label}-server"), &server);
    print_command_report_lines(&format!("{label}-client"), &client);

    if server.status != Some(0) || client.status != Some(0) {
        return Err(io::Error::other(format!(
            "{label} failed: server_status={:?} client_status={:?}",
            server.status, client.status
        )));
    }
    Ok(())
}

fn command_report_strings(program: &str, args: &[String]) -> io::Result<CommandReport> {
    let output = Command::new(program).args(args).output()?;
    Ok(CommandReport {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

fn libfabric_endpoint_pingpong(value: &str) -> io::Result<&'static str> {
    match value {
        "" | "rdm" | "FI_EP_RDM" | "fi_ep_rdm" => Ok("rdm"),
        "msg" | "FI_EP_MSG" | "fi_ep_msg" => Ok("msg"),
        "dgram" | "datagram" | "FI_EP_DGRAM" | "fi_ep_dgram" => Ok("dgram"),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown libfabric endpoint {other:?}; use rdm, msg, or dgram"),
        )),
    }
}

fn libfabric_endpoint_fi_info(value: &str) -> io::Result<&'static str> {
    match libfabric_endpoint_pingpong(value)? {
        "rdm" => Ok("FI_EP_RDM"),
        "msg" => Ok("FI_EP_MSG"),
        "dgram" => Ok("FI_EP_DGRAM"),
        _ => unreachable!(),
    }
}

fn libfabric_provider_role(provider: &str) -> &'static str {
    match provider {
        "efa" => "AWS EFA SRD fast path",
        "efa-direct" => "AWS EFA direct data-plane profile",
        "verbs" | "verbs;ofi_rxm" | "verbs;ofi_rxd" => "RDMA provider path for ConnectX/RXE",
        "tcp" | "sockets" => "portable libfabric-over-sockets fallback",
        "shm" => "same-host shared-memory path",
        _ => "provider-specific libfabric path",
    }
}

fn libfabric_lane_object(provider: &str, endpoint: &str) -> &'static str {
    match provider {
        "efa" | "efa-direct" => "fi_endpoint_efa",
        "verbs" | "verbs;ofi_rxm" | "verbs;ofi_rxd" => "fi_endpoint_verbs",
        "tcp" | "sockets" => "fi_endpoint_tcp",
        "shm" => "fi_endpoint_shm",
        _ => match endpoint {
            "rdm" => "fi_endpoint_rdm",
            "msg" => "fi_endpoint_msg",
            "dgram" => "fi_endpoint_dgram",
            _ => "fi_endpoint",
        },
    }
}

fn libfabric_plan(
    provider: &str,
    endpoint_arg: &str,
    peers: usize,
    lanes_per_peer: usize,
    workers: usize,
) -> io::Result<()> {
    if peers == 0 || lanes_per_peer == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "peers and lanes-per-peer must be greater than zero",
        ));
    }
    let endpoint = libfabric_endpoint_pingpong(endpoint_arg)?;
    let total_lanes = peers.checked_mul(lanes_per_peer).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "peers times lanes overflows")
    })?;
    let workers = tcp_bench_auto_workers(workers, total_lanes);
    println!(
        "libfabric-plan: provider={provider} endpoint={endpoint} peers={peers} \
         lanes_per_peer={lanes_per_peer} total_lanes={total_lanes} workers={workers}"
    );
    println!(
        "libfabric-plan: role={} lane_object={} api=fi_endpoint+fi_cq+fi_mr",
        libfabric_provider_role(provider),
        libfabric_lane_object(provider, endpoint),
    );
    for peer in 0..peers {
        for lane in 0..lanes_per_peer {
            let global = peer * lanes_per_peer + lane;
            let worker = global % workers;
            println!(
                "libfabric-lane: peer={peer} lane={lane} global={global} worker={worker} \
                 endpoint={global} cq={worker} mr_region={worker} wal_shard={global}"
            );
        }
    }
    println!(
        "libfabric-plan-note: keep TCP mux as negotiated fallback for public IP, inter-region, \
         and peers where provider={provider} is unavailable"
    );
    Ok(())
}

fn libfabric_smoke(
    provider: &str,
    endpoint_arg: &str,
    addr: &str,
    iters: usize,
    size: usize,
    port: u16,
    domain: Option<&str>,
) -> io::Result<()> {
    if iters == 0 || size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "iters and size must be greater than zero",
        ));
    }
    let endpoint = libfabric_endpoint_pingpong(endpoint_arg)?;
    let fi_info_endpoint = libfabric_endpoint_fi_info(endpoint_arg)?;
    let mut info_args = vec![
        "-p".to_string(),
        provider.to_string(),
        "-t".to_string(),
        fi_info_endpoint.to_string(),
    ];
    if let Some(domain) = domain.filter(|value| !value.is_empty()) {
        info_args.push("-d".to_string());
        info_args.push(domain.to_string());
    }
    let info = command_report_strings("fi_info", &info_args)?;
    print_command_report_lines_limited("libfabric-smoke-fi-info", &info, 48);

    let port_arg = port.to_string();
    let iters_arg = iters.to_string();
    let size_arg = size.to_string();
    let mut server_args = vec![
        "-p".to_string(),
        provider.to_string(),
        "-e".to_string(),
        endpoint.to_string(),
        "-I".to_string(),
        iters_arg.clone(),
        "-S".to_string(),
        size_arg.clone(),
        "-B".to_string(),
        port_arg.clone(),
    ];
    if let Some(domain) = domain.filter(|value| !value.is_empty()) {
        server_args.push("-d".to_string());
        server_args.push(domain.to_string());
    }
    let mut client_args = vec![
        "-p".to_string(),
        provider.to_string(),
        "-e".to_string(),
        endpoint.to_string(),
        "-I".to_string(),
        iters_arg,
        "-S".to_string(),
        size_arg,
        "-P".to_string(),
        port_arg,
    ];
    if let Some(domain) = domain.filter(|value| !value.is_empty()) {
        client_args.push("-d".to_string());
        client_args.push(domain.to_string());
    }
    client_args.push(addr.to_string());

    println!(
        "libfabric-smoke: provider={provider} endpoint={endpoint} addr={addr} \
         iters={iters} size={size} port={port} domain={}",
        domain.unwrap_or("auto")
    );
    run_pingpong_pair(
        "libfabric-smoke-fi-pingpong",
        "fi_pingpong",
        &server_args,
        &client_args,
    )?;
    println!("libfabric-smoke: ok");
    Ok(())
}

fn rdma_rxe_smoke(
    netdev: &str,
    requested_device: Option<&str>,
    gid_idx: u32,
    iters: usize,
    size: usize,
    port: u16,
) -> io::Result<()> {
    if iters == 0 || size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "iters and size must be greater than zero",
        ));
    }
    let mut created_device = None::<String>;
    let device = if let Some(device) = requested_device {
        if rdma_device_exists(device) {
            device.to_string()
        } else if let Some(existing) = rdma_device_for_netdev(netdev) {
            println!(
                "rdma-rxe-smoke: requested device {device:?} not present; using existing {existing:?} on netdev {netdev}"
            );
            existing
        } else {
            rdma_rxe_create_quiet(netdev, device)?;
            created_device = Some(device.to_string());
            device.to_string()
        }
    } else if let Some(existing) = rdma_device_for_netdev(netdev) {
        existing
    } else {
        let device = "rxe0".to_string();
        rdma_rxe_create_quiet(netdev, &device)?;
        created_device = Some(device.clone());
        device
    };

    println!(
        "rdma-rxe-smoke: netdev={netdev} device={device} gid_idx={gid_idx} iters={iters} size={size} port={port}"
    );
    print_command_report(
        "rdma-rxe-smoke-ibv-devinfo",
        "ibv_devinfo",
        &["-d", &device],
    );
    print_command_report("rdma-rxe-smoke-fi-info", "fi_info", &["-p", "verbs"]);

    let port_arg = port.to_string();
    let gid_arg = gid_idx.to_string();
    let iters_arg = iters.to_string();
    let size_arg = size.to_string();
    let server_args = vec![
        "-d".to_string(),
        device.clone(),
        "-g".to_string(),
        gid_arg.clone(),
        "-s".to_string(),
        size_arg.clone(),
        "-n".to_string(),
        iters_arg.clone(),
        "-p".to_string(),
        port_arg.clone(),
    ];
    let client_args = {
        let mut args = server_args.clone();
        args.push(if gid_idx == 2 {
            "::1".to_string()
        } else {
            "127.0.0.1".to_string()
        });
        args
    };

    let result = run_pingpong_pair(
        "rdma-rxe-smoke-ibv-rc-pingpong",
        "ibv_rc_pingpong",
        &server_args,
        &client_args,
    );

    if let Some(device) = created_device {
        if let Err(err) = rdma_rxe_del(&device) {
            println!("rdma-rxe-smoke-cleanup: failed to remove {device}: {err}");
        }
    }

    result?;
    println!("rdma-rxe-smoke: ok");
    Ok(())
}

fn register_ifq_inner(ifname: &str, rxq: u32, verbose: bool) -> io::Result<()> {
    let zcrx = if verbose {
        query_zcrx()?
    } else {
        query_zcrx_raw()?.0
    };
    let ifindex = if_nametoindex(ifname)?;
    let page_size = page_size()?;
    let rq_entries = 128usize;
    let rq_size = align_up(
        align_up(zcrx.rq_hdr_size as usize, zcrx.rq_hdr_alignment as usize)
            + rq_entries * size_of::<IoUringZcrxRqe>(),
        page_size,
    );
    let area_size = 16 * 1024 * 1024usize;

    let rq_ptr = mmap_rw(rq_size)?;
    let area_ptr = mmap_rw(area_size)?;

    let mut params = IoUringParams {
        flags: IORING_SETUP_DEFER_TASKRUN_U32
            | IORING_SETUP_CQE32
            | IORING_SETUP_SINGLE_ISSUER
            | IORING_SETUP_SUBMIT_ALL,
        cq_entries: 256,
        ..IoUringParams::default()
    };
    let ring_fd = io_uring_setup(128, &mut params)?;

    let mut area = IoUringZcrxAreaReg {
        addr: area_ptr as u64,
        len: area_size as u64,
        ..IoUringZcrxAreaReg::default()
    };
    let mut rq_region = IoUringRegionDesc {
        user_addr: rq_ptr as u64,
        size: rq_size as u64,
        flags: IORING_MEM_REGION_TYPE_USER,
        ..IoUringRegionDesc::default()
    };
    let mut ifq = IoUringZcrxIfqReg {
        if_idx: ifindex,
        if_rxq: rxq,
        rq_entries: rq_entries as u32,
        area_ptr: &mut area as *mut IoUringZcrxAreaReg as u64,
        region_ptr: &mut rq_region as *mut IoUringRegionDesc as u64,
        ..IoUringZcrxIfqReg::default()
    };

    let register_result = io_uring_register(
        ring_fd,
        IORING_REGISTER_ZCRX_IFQ,
        &mut ifq as *mut IoUringZcrxIfqReg as *mut libc::c_void,
        1,
    );

    let close_result = unsafe { libc::close(ring_fd) };
    let rq_unmap_result = unsafe { libc::munmap(rq_ptr, rq_size) };
    let area_unmap_result = unsafe { libc::munmap(area_ptr, area_size) };

    register_result?;
    if close_result != 0 {
        return Err(io::Error::last_os_error());
    }
    if rq_unmap_result != 0 {
        return Err(io::Error::last_os_error());
    }
    if area_unmap_result != 0 {
        return Err(io::Error::last_os_error());
    }

    if verbose {
        println!("registered ZCRX IFQ:");
        println!("  interface: {ifname} ifindex={ifindex} rxq={rxq}");
        println!("  zcrx_id: {}", ifq.zcrx_id);
        println!("  rq_entries: {}", ifq.rq_entries);
        println!("  rq_size: {rq_size}");
        println!("  area_size: {area_size}");
        println!("  area_token: 0x{:x}", area.rq_area_token);
        println!(
            "  refill offsets: head={} tail={} rqes={}",
            ifq.offsets.head, ifq.offsets.tail, ifq.offsets.rqes
        );
    }

    Ok(())
}

fn register_ifq(ifname: &str, rxq: u32) -> io::Result<()> {
    register_ifq_inner(ifname, rxq, true)
}

fn stress_register_ifq(ifname: &str, iterations: u32, rxq: u32) -> io::Result<()> {
    let mut eexist_retries = 0u32;
    let mut max_eexist_retries = 0u32;

    for i in 0..iterations {
        let mut iter_eexist_retries = 0u32;

        loop {
            match register_ifq_inner(ifname, rxq, false) {
                Ok(()) => break,
                Err(err)
                    if err.raw_os_error() == Some(libc::EEXIST) && iter_eexist_retries < 100 =>
                {
                    iter_eexist_retries += 1;
                    eexist_retries += 1;
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => {
                    return Err(io::Error::new(
                        err.kind(),
                        format!("iteration {} failed to register ZCRX IFQ: {err}", i + 1),
                    ));
                }
            }
        }

        max_eexist_retries = max_eexist_retries.max(iter_eexist_retries);

        if (i + 1) % 25 == 0 || i + 1 == iterations {
            println!("stress-register-ifq: completed {}/{}", i + 1, iterations);
        }
    }

    println!(
        "stress-register-ifq: ok iterations={iterations} ifname={ifname} rxq={rxq} \
         eexist_retries={eexist_retries} max_eexist_retries={max_eexist_retries}"
    );
    Ok(())
}

fn pattern_byte(seq: usize) -> u8 {
    ((seq as u64).wrapping_mul(1_000_000_001) ^ 0xdead_beef) as u8
}

fn fill_pattern(buf: &mut [u8], base_seq: usize) {
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte = pattern_byte(base_seq + i);
    }
}

fn verify_pattern(buf: &[u8], base_seq: usize) -> io::Result<()> {
    for (i, actual) in buf.iter().copied().enumerate() {
        let expected = pattern_byte(base_seq + i);
        if actual != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "payload mismatch at byte {}: expected={} actual={}",
                    base_seq + i,
                    expected,
                    actual
                ),
            ));
        }
    }

    Ok(())
}

fn verify_fixed_byte(buf: &[u8], expected: u8, base_seq: usize) -> io::Result<()> {
    for (i, actual) in buf.iter().copied().enumerate() {
        if actual != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "payload mismatch at byte {}: expected={} actual={}",
                    base_seq + i,
                    expected,
                    actual
                ),
            ));
        }
    }

    Ok(())
}

fn recv_zc_server(
    ifname: &str,
    rxq: u32,
    port: u16,
    expected_bytes: usize,
    fixed_byte: Option<u8>,
) -> io::Result<()> {
    if expected_bytes > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recv-zc-server expected_bytes must fit in u32",
        ));
    }

    let listener = TcpListener::bind(("0.0.0.0", port))?;
    println!("recv-zc-server: listening on 0.0.0.0:{port}");

    let mut ring = RawRing::new(128, 512)?;
    ring.register_napi_from_env("recv-zc-server")?;
    let mut zcrx = ZcrxContext::register(ring.fd(), ifname, rxq, true, fixed_byte)?;
    if let Some(byte) = fixed_byte {
        println!("recv-zc-server: prefilled ZCRX area with byte={byte}");
    }

    let (stream, peer_addr) = listener.accept()?;
    println!("recv-zc-server: accepted {peer_addr}");

    let fd = stream.into_raw_fd();
    let result = recv_zc_stream(&mut ring, &mut zcrx, fd, expected_bytes, fixed_byte);
    let close_result = unsafe { libc::close(fd) };
    if close_result != 0 && result.is_ok() {
        return Err(io::Error::last_os_error());
    }

    result
}

fn recv_zc_stream(
    ring: &mut RawRing,
    zcrx: &mut ZcrxContext,
    fd: i32,
    expected_bytes: usize,
    fixed_byte: Option<u8>,
) -> io::Result<()> {
    ring.submit_recv_zc(fd, zcrx.zcrx_id, expected_bytes as u32)?;

    let mut received = 0usize;
    loop {
        let cqe = ring.wait_cqe()?;
        if cqe.user_data != RECV_ZC_USER_DATA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected CQE user_data=0x{:x}", cqe.user_data),
            ));
        }

        if (cqe.flags & IORING_CQE_F_MORE) == 0 {
            if cqe.res != 0 {
                return Err(if cqe.res < 0 {
                    io::Error::from_raw_os_error(-cqe.res)
                } else {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected final recv_zc result {}", cqe.res),
                    )
                });
            }
            if received != expected_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("received {received}/{expected_bytes} bytes before final CQE"),
                ));
            }

            println!("recv-zc-server: ok received={received} bytes");
            return Ok(());
        }

        if cqe.res < 0 {
            return Err(io::Error::from_raw_os_error(-cqe.res));
        }

        let data = zcrx.data_for_cqe(&cqe)?;
        if let Some(byte) = fixed_byte {
            verify_fixed_byte(data, byte, received)?;
        } else {
            verify_pattern(data, received)?;
        }
        received += data.len();
        if received > expected_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("received more than expected: {received}/{expected_bytes}"),
            ));
        }
        zcrx.return_buffer(&cqe)?;

        if received == expected_bytes || received % (64 * 1024) == 0 {
            println!("recv-zc-server: received {received}/{expected_bytes}");
        }
    }
}

fn tcp_send(addr: &str, port: u16, bytes: usize, fixed_byte: Option<u8>) -> io::Result<()> {
    let mut stream = TcpStream::connect((addr, port))?;
    set_tcp_nodelay_from_env(&stream)?;

    let mut sent = 0usize;
    let mut buf = vec![0u8; 4096];
    while sent < bytes {
        let len = (bytes - sent).min(buf.len());
        if let Some(byte) = fixed_byte {
            buf[..len].fill(byte);
        } else {
            fill_pattern(&mut buf[..len], sent);
        }
        stream.write_all(&buf[..len])?;
        sent += len;
    }
    stream.shutdown(Shutdown::Write)?;

    println!("tcp-send: ok sent={sent} bytes to {addr}:{port}");
    Ok(())
}

fn tcp_sink_server(
    bind: &str,
    port: u16,
    connections: usize,
    expected_bytes: usize,
) -> io::Result<()> {
    let listener = TcpListener::bind((bind, port))?;
    println!(
        "tcp-sink-server: listening on {bind}:{port} connections={connections} \
         expected_bytes={expected_bytes}"
    );

    let started = Instant::now();
    let mut total = 0usize;
    let mut buf = vec![0u8; 64 * 1024];
    for conn_idx in 0..connections {
        let (mut stream, peer_addr) = listener.accept()?;
        let mut received = 0usize;
        loop {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            verify_pattern(&buf[..n], received)?;
            received += n;
            if received > expected_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "connection {} from {peer_addr} sent too many bytes: \
                         {received}/{expected_bytes}",
                        conn_idx + 1
                    ),
                ));
            }
        }
        if received != expected_bytes {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "connection {} from {peer_addr} sent {received}/{expected_bytes} bytes",
                    conn_idx + 1
                ),
            ));
        }
        total += received;
        println!(
            "tcp-sink-server: connection {}/{} from {peer_addr} ok bytes={received}",
            conn_idx + 1,
            connections
        );
    }

    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "tcp-sink-server: ok total_bytes={total} seconds={elapsed:.6} MiBps={:.2}",
        (total as f64 / (1024.0 * 1024.0)) / elapsed.max(f64::MIN_POSITIVE)
    );
    Ok(())
}

fn set_socket_bench_buffers(fd: i32) {
    let value: libc::c_int = 16 * 1024 * 1024;
    unsafe {
        let _ = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &value as *const _ as *const libc::c_void,
            std::mem::size_of_val(&value) as libc::socklen_t,
        );
        let _ = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &value as *const _ as *const libc::c_void,
            std::mem::size_of_val(&value) as libc::socklen_t,
        );
    }
}

fn set_socket_recv_buffer(fd: i32, bytes: usize) -> io::Result<()> {
    let value = libc::c_int::try_from(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("socket receive buffer size {bytes} exceeds c_int"),
        )
    })?;
    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &value as *const _ as *const libc::c_void,
            std::mem::size_of_val(&value) as libc::socklen_t,
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_tcp_bench_buffers(stream: &TcpStream) {
    set_socket_bench_buffers(stream.as_raw_fd());
}

fn ready_handshake_enabled() -> bool {
    env_truthy("URING_PLAY_READY_HANDSHAKE")
}

fn start_handshake_enabled() -> bool {
    env_truthy("URING_PLAY_START_HANDSHAKE")
}

fn send_tcp_control_byte(
    streams: &[TcpStream],
    byte: u8,
    label: &str,
    phase: &str,
) -> io::Result<()> {
    for stream in streams {
        let mut stream = stream;
        stream.write_all(&[byte])?;
    }
    println!("{label}: {phase}=sent streams={}", streams.len());
    Ok(())
}

fn recv_tcp_control_byte(
    streams: &mut [TcpStream],
    expected: u8,
    label: &str,
    phase: &str,
) -> io::Result<()> {
    for stream in streams.iter_mut() {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte)?;
        if byte[0] != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{label}: {phase} byte mismatch: got 0x{:02x} expected 0x{expected:02x}",
                    byte[0],
                ),
            ));
        }
    }
    println!("{label}: {phase}=received streams={}", streams.len());
    Ok(())
}

fn maybe_send_ready_handshake(streams: &[TcpStream], label: &str) -> io::Result<()> {
    if !ready_handshake_enabled() {
        return Ok(());
    }
    send_tcp_control_byte(streams, TCP_READY_BYTE, label, "ready_handshake")
}

fn maybe_recv_ready_handshake(streams: &mut [TcpStream], label: &str) -> io::Result<()> {
    if !ready_handshake_enabled() {
        return Ok(());
    }
    recv_tcp_control_byte(streams, TCP_READY_BYTE, label, "ready_handshake")
}

fn ready_handshake_filter_rejects_enabled() -> bool {
    env_truthy("URING_PLAY_READY_HANDSHAKE_FILTER_REJECTS")
}

fn start_handshake_filter_rejects_enabled() -> bool {
    env_truthy("URING_PLAY_START_HANDSHAKE_FILTER_REJECTS")
}

fn maybe_filter_ready_handshake(
    streams: Vec<TcpStream>,
    label: &str,
) -> io::Result<Vec<TcpStream>> {
    if !ready_handshake_enabled() {
        return Ok(streams);
    }
    if !ready_handshake_filter_rejects_enabled() {
        let mut streams = streams;
        maybe_recv_ready_handshake(&mut streams, label)?;
        return Ok(streams);
    }

    let total_streams = streams.len();
    let mut ready_streams = Vec::with_capacity(total_streams);
    let mut rejected_streams = 0usize;
    for mut stream in streams {
        let mut byte = [0u8; 1];
        match stream.read_exact(&mut byte) {
            Ok(()) if byte[0] == TCP_READY_BYTE => ready_streams.push(stream),
            Ok(()) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{label}: receiver-ready handshake byte mismatch: got 0x{:02x} expected 0x{TCP_READY_BYTE:02x}",
                        byte[0]
                    ),
                ));
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                rejected_streams += 1;
            }
            Err(err) => return Err(err),
        }
    }
    if ready_streams.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!("{label}: receiver rejected all ready-handshake streams"),
        ));
    }
    println!(
        "{label}: ready_handshake=received streams={} rejected_streams={rejected_streams} total_streams={total_streams}",
        ready_streams.len()
    );
    Ok(ready_streams)
}

fn maybe_run_client_start_handshake(
    mut streams: Vec<TcpStream>,
    label: &str,
) -> io::Result<Vec<TcpStream>> {
    if !start_handshake_enabled() {
        return maybe_filter_ready_handshake(streams, label);
    }
    if start_handshake_filter_rejects_enabled() {
        let total_streams = streams.len();
        let mut ready_streams = Vec::with_capacity(total_streams);
        let mut rejected_streams = 0usize;
        for mut stream in streams {
            let mut byte = [0u8; 1];
            match stream.read_exact(&mut byte) {
                Ok(()) if byte[0] == TCP_READY_BYTE => ready_streams.push(stream),
                Ok(()) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{label}: start_handshake_ready byte mismatch: got 0x{:02x} expected 0x{TCP_READY_BYTE:02x}",
                            byte[0]
                        ),
                    ));
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    rejected_streams += 1;
                }
                Err(err) => return Err(err),
            }
        }
        if ready_streams.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("{label}: receiver rejected all start-handshake streams"),
            ));
        }
        println!(
            "{label}: start_handshake_ready=received streams={} rejected_streams={rejected_streams} total_streams={total_streams}",
            ready_streams.len()
        );
        send_tcp_control_byte(&ready_streams, TCP_ACK_BYTE, label, "start_handshake_ack")?;
        recv_tcp_control_byte(
            &mut ready_streams,
            TCP_START_BYTE,
            label,
            "start_handshake_go",
        )?;
        return Ok(ready_streams);
    }
    recv_tcp_control_byte(&mut streams, TCP_READY_BYTE, label, "start_handshake_ready")?;
    send_tcp_control_byte(&streams, TCP_ACK_BYTE, label, "start_handshake_ack")?;
    recv_tcp_control_byte(&mut streams, TCP_START_BYTE, label, "start_handshake_go")?;
    Ok(streams)
}

fn set_udp_bench_buffers(socket: &UdpSocket) {
    set_socket_bench_buffers(socket.as_raw_fd());
}

fn tcp_nodelay_enabled() -> bool {
    env_enabled_or("URING_PLAY_TCP_NODELAY", true)
}

fn set_tcp_nodelay_from_env(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(tcp_nodelay_enabled())
}

#[derive(Clone, Copy)]
struct SocketLocality {
    incoming_cpu: i32,
    incoming_napi_id: i32,
}

impl Default for SocketLocality {
    fn default() -> Self {
        Self {
            incoming_cpu: -1,
            incoming_napi_id: 0,
        }
    }
}

impl SocketLocality {
    fn incoming_cpu_known(self) -> bool {
        self.incoming_cpu >= 0
    }

    fn incoming_napi_known(self) -> bool {
        self.incoming_napi_id > 0
    }
}

fn getsockopt_i32(fd: i32, optname: libc::c_int) -> io::Result<i32> {
    let mut value: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            optname,
            &mut value as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        )
    };
    if ret != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

fn observe_socket_locality(stream: &TcpStream) -> SocketLocality {
    let fd = stream.as_raw_fd();
    SocketLocality {
        incoming_cpu: getsockopt_i32(fd, libc::SO_INCOMING_CPU).unwrap_or(-1),
        incoming_napi_id: getsockopt_i32(fd, libc::SO_INCOMING_NAPI_ID).unwrap_or(0),
    }
}

fn socket_locality_label(locality: SocketLocality) -> String {
    let cpu = if locality.incoming_cpu_known() {
        locality.incoming_cpu.to_string()
    } else {
        "unknown".to_string()
    };
    let napi = if locality.incoming_napi_known() {
        locality.incoming_napi_id.to_string()
    } else {
        "unknown".to_string()
    };
    format!("incoming_cpu={cpu} incoming_napi_id={napi}")
}

fn print_tcp_bench_result(label: &str, total: usize, elapsed: Duration) {
    let secs = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
    let gibps = (total as f64 / (1024.0 * 1024.0 * 1024.0)) / secs;
    let gbit = (total as f64 * 8.0 / 1_000_000_000.0) / secs;
    println!("{label}: bytes={total} seconds={secs:.6} GiBps={gibps:.3} Gbitps={gbit:.3}");
}

#[derive(Clone, Copy, Default)]
struct ThreadContextSwitches {
    voluntary: u64,
    involuntary: u64,
    migrations: u64,
}

#[derive(Clone, Copy, Default)]
struct ThreadAffinity {
    target_cpu: i32,
    applied: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ZcrxConsumeMode {
    None,
    ChecksumScalar,
    ChecksumAvx2,
    LenStreamSkip,
}

impl ZcrxConsumeMode {
    fn from_env() -> io::Result<Self> {
        match env::var("URING_PLAY_ZCRX_CONSUME")
            .unwrap_or_else(|_| "none".to_string())
            .as_str()
        {
            "" | "none" | "off" | "0" => Ok(Self::None),
            "checksum" | "checksum-auto" => {
                if checksum_avx2_available() {
                    Ok(Self::ChecksumAvx2)
                } else {
                    Ok(Self::ChecksumScalar)
                }
            }
            "checksum-scalar" | "scalar" => Ok(Self::ChecksumScalar),
            "checksum-avx2" | "avx2" => {
                if checksum_avx2_available() {
                    Ok(Self::ChecksumAvx2)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "URING_PLAY_ZCRX_CONSUME=checksum-avx2 requested but AVX2 is unavailable",
                    ))
                }
            }
            "lenstream-skip" | "length-skip" | "len32-skip" | "frame-skip" => {
                Ok(Self::LenStreamSkip)
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown URING_PLAY_ZCRX_CONSUME={other:?}; use none, checksum, checksum-scalar, checksum-avx2, or lenstream-skip"
                ),
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ChecksumScalar => "checksum-scalar",
            Self::ChecksumAvx2 => "checksum-avx2",
            Self::LenStreamSkip => "lenstream-skip",
        }
    }

    fn is_lenstream(self) -> bool {
        matches!(self, Self::LenStreamSkip)
    }
}

struct ZcrxWorkerStats {
    rxq: u32,
    streams: usize,
    bytes: usize,
    consumed_bytes: usize,
    skipped_bytes: usize,
    frames: usize,
    checksum: u64,
    wall: Duration,
    cpu: Duration,
    target_cpu: i32,
    affinity_applied: bool,
    start_cpu: i32,
    end_cpu: i32,
    voluntary_switches: u64,
    involuntary_switches: u64,
    migrations: u64,
}

#[derive(Clone, Copy, Default)]
struct SoftnetStat {
    processed: u64,
    dropped: u64,
    time_squeeze: u64,
}

fn current_tid() -> i64 {
    unsafe { libc::syscall(libc::SYS_gettid) as i64 }
}

fn current_cpu() -> i32 {
    unsafe { libc::sched_getcpu() }
}

fn checksum_avx2_available() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        std::arch::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        false
    }
}

fn checksum_bytes_scalar(data: &[u8]) -> u64 {
    data.iter()
        .fold(0u64, |sum, byte| sum.wrapping_add(*byte as u64))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn checksum_bytes_avx2_inner(data: &[u8]) -> u64 {
    use std::arch::x86_64::{
        __m256i, _mm256_add_epi64, _mm256_loadu_si256, _mm256_sad_epu8, _mm256_setzero_si256,
        _mm256_storeu_si256,
    };

    let mut offset = 0usize;
    let mut acc = _mm256_setzero_si256();
    let zero = _mm256_setzero_si256();

    while offset + 32 <= data.len() {
        let chunk = unsafe { _mm256_loadu_si256(data.as_ptr().add(offset) as *const __m256i) };
        let sums = _mm256_sad_epu8(chunk, zero);
        acc = _mm256_add_epi64(acc, sums);
        offset += 32;
    }

    let mut lanes = [0u64; 4];
    unsafe { _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, acc) };
    lanes
        .into_iter()
        .fold(0u64, |sum, lane| sum.wrapping_add(lane))
        .wrapping_add(checksum_bytes_scalar(&data[offset..]))
}

#[cfg(target_arch = "x86")]
#[target_feature(enable = "avx2")]
unsafe fn checksum_bytes_avx2_inner(data: &[u8]) -> u64 {
    use std::arch::x86::{
        __m256i, _mm256_add_epi64, _mm256_loadu_si256, _mm256_sad_epu8, _mm256_setzero_si256,
        _mm256_storeu_si256,
    };

    let mut offset = 0usize;
    let mut acc = _mm256_setzero_si256();
    let zero = _mm256_setzero_si256();

    while offset + 32 <= data.len() {
        let chunk = unsafe { _mm256_loadu_si256(data.as_ptr().add(offset) as *const __m256i) };
        let sums = _mm256_sad_epu8(chunk, zero);
        acc = _mm256_add_epi64(acc, sums);
        offset += 32;
    }

    let mut lanes = [0u64; 4];
    unsafe { _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, acc) };
    lanes
        .into_iter()
        .fold(0u64, |sum, lane| sum.wrapping_add(lane))
        .wrapping_add(checksum_bytes_scalar(&data[offset..]))
}

fn checksum_bytes(mode: ZcrxConsumeMode, data: &[u8]) -> u64 {
    match mode {
        ZcrxConsumeMode::None => 0,
        ZcrxConsumeMode::ChecksumScalar => checksum_bytes_scalar(data),
        ZcrxConsumeMode::ChecksumAvx2 => {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            {
                unsafe { checksum_bytes_avx2_inner(data) }
            }
            #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
            {
                checksum_bytes_scalar(data)
            }
        }
        ZcrxConsumeMode::LenStreamSkip => 0,
    }
}

#[derive(Clone)]
struct LenStreamParser {
    header: [u8; LENSTREAM_HEADER_LEN],
    header_len: usize,
    remaining_payload: usize,
    max_payload_len: usize,
}

#[derive(Default)]
struct LenStreamConsumeDelta {
    header_bytes: usize,
    payload_bytes: usize,
    frames: usize,
    checksum: u64,
}

impl LenStreamParser {
    fn new(max_payload_len: usize) -> Self {
        Self {
            header: [0; LENSTREAM_HEADER_LEN],
            header_len: 0,
            remaining_payload: 0,
            max_payload_len,
        }
    }

    fn consume(&mut self, data: &[u8]) -> io::Result<LenStreamConsumeDelta> {
        let mut offset = 0usize;
        let mut delta = LenStreamConsumeDelta::default();

        while offset < data.len() {
            if self.remaining_payload != 0 {
                let skip = self.remaining_payload.min(data.len() - offset);
                self.remaining_payload -= skip;
                delta.payload_bytes += skip;
                offset += skip;
                continue;
            }

            let take = (LENSTREAM_HEADER_LEN - self.header_len).min(data.len() - offset);
            for byte in &data[offset..offset + take] {
                delta.checksum = delta.checksum.wrapping_add(*byte as u64);
            }
            self.header[self.header_len..self.header_len + take]
                .copy_from_slice(&data[offset..offset + take]);
            self.header_len += take;
            delta.header_bytes += take;
            offset += take;

            if self.header_len == LENSTREAM_HEADER_LEN {
                let payload_len = u32::from_le_bytes(self.header) as usize;
                if payload_len > self.max_payload_len {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "length stream payload too large: {payload_len} > {}",
                            self.max_payload_len
                        ),
                    ));
                }
                self.remaining_payload = payload_len;
                self.header_len = 0;
                delta.frames += 1;
                delta.checksum = delta.checksum.wrapping_add(payload_len as u64);
            }
        }

        Ok(delta)
    }

    fn finish(&self, conn: usize) -> io::Result<()> {
        if self.header_len == 0 && self.remaining_payload == 0 {
            return Ok(());
        }

        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "connection {conn} ended mid length stream: header_bytes={} remaining_payload={}",
                self.header_len, self.remaining_payload
            ),
        ))
    }
}

fn online_cpu_count() -> usize {
    thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .max(1)
}

fn configured_affinity_target_cpu(index: usize) -> usize {
    if let Ok(cpu_list) = env::var("URING_PLAY_PIN_CPU_LIST")
        && let Ok(cpus) = parse_cpu_list(&cpu_list)
        && !cpus.is_empty()
    {
        return cpus[index % cpus.len()];
    }

    let base = env_usize_or("URING_PLAY_PIN_BASE_CPU", 0);
    let stride = env_usize_or("URING_PLAY_PIN_STRIDE", 1).max(1);
    let count = env_usize_or("URING_PLAY_PIN_CPU_COUNT", online_cpu_count()).max(1);
    base + index.saturating_mul(stride) % count
}

fn affinity_target_cpu(index: usize) -> Option<usize> {
    env_truthy("URING_PLAY_PIN_CPUS").then(|| configured_affinity_target_cpu(index))
}

fn set_current_thread_affinity(cpu: usize) -> io::Result<()> {
    if cpu >= libc::CPU_SETSIZE as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cpu {cpu} exceeds CPU_SETSIZE"),
        ));
    }

    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(
            0,
            std::mem::size_of::<libc::cpu_set_t>(),
            &set as *const libc::cpu_set_t,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}

fn pin_current_thread_to(label: &str, index: usize, cpu: usize) -> ThreadAffinity {
    match set_current_thread_affinity(cpu) {
        Ok(()) => {
            println!("{label}: affinity_index={index} target_cpu={cpu} status=ok");
            ThreadAffinity {
                target_cpu: cpu as i32,
                applied: true,
            }
        }
        Err(err) => {
            println!("{label}: affinity_index={index} target_cpu={cpu} status=error error={err}");
            ThreadAffinity {
                target_cpu: cpu as i32,
                applied: false,
            }
        }
    }
}

fn pin_current_thread(label: &str, index: usize) -> ThreadAffinity {
    pin_current_thread_to(label, index, configured_affinity_target_cpu(index))
}

fn maybe_pin_current_thread(label: &str, index: usize) -> ThreadAffinity {
    let Some(cpu) = affinity_target_cpu(index) else {
        return ThreadAffinity {
            target_cpu: -1,
            applied: false,
        };
    };

    pin_current_thread_to(label, index, cpu)
}

fn pin_current_thread_if_requested(label: &str, index: usize, pin: bool) -> ThreadAffinity {
    if pin {
        pin_current_thread(label, index)
    } else {
        ThreadAffinity {
            target_cpu: -1,
            applied: false,
        }
    }
}

fn pin_current_thread_if_requested_to_cpu(
    label: &str,
    index: usize,
    pin: bool,
    cpu: Option<usize>,
) -> ThreadAffinity {
    if !pin {
        return ThreadAffinity {
            target_cpu: -1,
            applied: false,
        };
    }

    match cpu {
        Some(cpu) => pin_current_thread_to(label, index, cpu),
        None => pin_current_thread(label, index),
    }
}

fn thread_cpu_time() -> io::Result<Duration> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

fn read_thread_context_switches(tid: i64) -> io::Result<ThreadContextSwitches> {
    let status = fs::read_to_string(format!("/proc/self/task/{tid}/status"))?;
    let mut switches = ThreadContextSwitches::default();

    for line in status.lines() {
        if let Some(value) = line.strip_prefix("voluntary_ctxt_switches:") {
            switches.voluntary = value.trim().parse::<u64>().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
            switches.involuntary = value.trim().parse::<u64>().unwrap_or(0);
        }
    }

    if let Ok(sched) = fs::read_to_string(format!("/proc/self/task/{tid}/sched")) {
        for line in sched.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.trim().ends_with("nr_migrations") {
                switches.migrations = value
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(0);
            }
        }
    }

    Ok(switches)
}

fn read_softirq_per_cpu_counts() -> io::Result<BTreeMap<String, Vec<u64>>> {
    let softirqs = fs::read_to_string("/proc/softirqs")?;
    let mut counts = BTreeMap::new();

    for line in softirqs.lines().skip(1) {
        let Some((name, values)) = line.split_once(':') else {
            continue;
        };
        let cpu_values = values
            .split_whitespace()
            .filter_map(|value| value.parse::<u64>().ok())
            .collect::<Vec<_>>();
        counts.insert(name.trim().to_string(), cpu_values);
    }

    Ok(counts)
}

fn sum_softirq_counts(snapshot: &BTreeMap<String, Vec<u64>>) -> BTreeMap<String, u64> {
    snapshot
        .iter()
        .map(|(name, counts)| (name.clone(), counts.iter().copied().sum()))
        .collect()
}

fn print_softirq_delta(label: &str, before: &BTreeMap<String, u64>, after: &BTreeMap<String, u64>) {
    let names = ["NET_RX", "NET_TX", "TIMER", "HRTIMER", "SCHED", "RCU"];
    let mut parts = Vec::with_capacity(names.len());

    for name in names {
        let delta = after
            .get(name)
            .copied()
            .unwrap_or(0)
            .saturating_sub(before.get(name).copied().unwrap_or(0));
        parts.push(format!("{name}={delta}"));
    }

    println!("{label}: {}", parts.join(" "));
}

fn per_cpu_delta(before: Option<&Vec<u64>>, after: Option<&Vec<u64>>) -> Vec<u64> {
    let before = before.map(Vec::as_slice).unwrap_or(&[]);
    let after = after.map(Vec::as_slice).unwrap_or(&[]);
    let len = before.len().max(after.len());
    (0..len)
        .map(|idx| {
            after
                .get(idx)
                .copied()
                .unwrap_or(0)
                .saturating_sub(before.get(idx).copied().unwrap_or(0))
        })
        .collect()
}

fn format_cpu_deltas(deltas: &[u64]) -> String {
    let mut active = deltas
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, value)| *value > 0)
        .collect::<Vec<_>>();

    if active.is_empty() {
        return "none".to_string();
    }

    if active.len() > 32 {
        active.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        active.truncate(8);
    }

    active
        .into_iter()
        .map(|(cpu, value)| format!("cpu{cpu}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn print_softirq_per_cpu_delta(
    label: &str,
    before: &BTreeMap<String, Vec<u64>>,
    after: &BTreeMap<String, Vec<u64>>,
) {
    let names = ["NET_RX", "NET_TX", "TIMER", "HRTIMER", "SCHED", "RCU"];

    for name in names {
        let deltas = per_cpu_delta(before.get(name), after.get(name));
        let total = deltas.iter().copied().sum::<u64>();
        let active_cpus = deltas.iter().filter(|value| **value > 0).count();
        let max_cpu = deltas
            .iter()
            .copied()
            .enumerate()
            .max_by(|a, b| a.1.cmp(&b.1))
            .filter(|(_, value)| *value > 0);
        let (max_cpu, max_delta) = max_cpu
            .map(|(cpu, value)| (format!("cpu{cpu}"), value))
            .unwrap_or_else(|| ("none".to_string(), 0));
        println!(
            "{label}: name={name} total={total} active_cpus={active_cpus} \
             max_cpu={max_cpu} max_delta={max_delta} cpus={}",
            format_cpu_deltas(&deltas)
        );
    }
}

fn parse_softnet_hex(value: Option<&str>) -> u64 {
    value
        .and_then(|value| u64::from_str_radix(value, 16).ok())
        .unwrap_or(0)
}

fn read_softnet_stat() -> io::Result<Vec<SoftnetStat>> {
    let softnet = fs::read_to_string("/proc/net/softnet_stat")?;
    let mut stats = Vec::new();

    for line in softnet.lines() {
        let mut fields = line.split_whitespace();
        stats.push(SoftnetStat {
            processed: parse_softnet_hex(fields.next()),
            dropped: parse_softnet_hex(fields.next()),
            time_squeeze: parse_softnet_hex(fields.next()),
        });
    }

    Ok(stats)
}

fn softnet_delta(before: Option<&SoftnetStat>, after: Option<&SoftnetStat>) -> SoftnetStat {
    let before = before.copied().unwrap_or_default();
    let after = after.copied().unwrap_or_default();
    SoftnetStat {
        processed: after.processed.saturating_sub(before.processed),
        dropped: after.dropped.saturating_sub(before.dropped),
        time_squeeze: after.time_squeeze.saturating_sub(before.time_squeeze),
    }
}

fn print_softnet_delta(label: &str, before: &[SoftnetStat], after: &[SoftnetStat]) {
    let len = before.len().max(after.len());
    let mut processed = Vec::with_capacity(len);
    let mut dropped = Vec::with_capacity(len);
    let mut time_squeeze = Vec::with_capacity(len);

    for idx in 0..len {
        let delta = softnet_delta(before.get(idx), after.get(idx));
        processed.push(delta.processed);
        dropped.push(delta.dropped);
        time_squeeze.push(delta.time_squeeze);
    }

    let processed_total = processed.iter().copied().sum::<u64>();
    let dropped_total = dropped.iter().copied().sum::<u64>();
    let time_squeeze_total = time_squeeze.iter().copied().sum::<u64>();
    let active_cpus = processed.iter().filter(|value| **value > 0).count();
    let max_processed_cpu = processed
        .iter()
        .copied()
        .enumerate()
        .max_by(|a, b| a.1.cmp(&b.1))
        .filter(|(_, value)| *value > 0);
    let (max_processed_cpu, max_processed) = max_processed_cpu
        .map(|(cpu, value)| (format!("cpu{cpu}"), value))
        .unwrap_or_else(|| ("none".to_string(), 0));

    println!(
        "{label}: processed={processed_total} dropped={dropped_total} \
         time_squeeze={time_squeeze_total} active_cpus={active_cpus} \
         max_processed_cpu={max_processed_cpu} max_processed={max_processed} \
         processed_cpus={}",
        format_cpu_deltas(&processed)
    );

    if dropped_total > 0 {
        println!("{label}-dropped-cpus: {}", format_cpu_deltas(&dropped));
    }
    if time_squeeze_total > 0 {
        println!(
            "{label}-time-squeeze-cpus: {}",
            format_cpu_deltas(&time_squeeze)
        );
    }
}

fn tcp_bench_port(base_port: u16, lane: usize) -> io::Result<u16> {
    let lane = u16::try_from(lane)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many TCP ports"))?;
    base_port.checked_add(lane).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "base port plus port count exceeds u16",
        )
    })
}

fn tcp_bench_total_connections(ports: usize, connections_per_port: usize) -> io::Result<usize> {
    if ports == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ports must be greater than zero",
        ));
    }
    if connections_per_port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "connections per port must be greater than zero",
        ));
    }
    ports.checked_mul(connections_per_port).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "port count times connections per port overflows",
        )
    })
}

#[derive(Clone, Copy)]
struct TcpSourcePortPlan {
    base: Option<u16>,
    stride: usize,
}

impl TcpSourcePortPlan {
    fn from_env() -> io::Result<Self> {
        let base = match env::var("URING_PLAY_SOURCE_PORT_BASE") {
            Ok(value) if !value.trim().is_empty() => {
                let port = value.trim().parse::<u16>().map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("URING_PLAY_SOURCE_PORT_BASE={value:?} is invalid: {err}"),
                    )
                })?;
                if port == 0 { None } else { Some(port) }
            }
            _ => None,
        };
        let stride = env_usize_or("URING_PLAY_SOURCE_PORT_STRIDE", 1).max(1);
        Ok(Self { base, stride })
    }

    fn source_port(self, global_connection_index: usize) -> io::Result<Option<u16>> {
        let Some(base) = self.base else {
            return Ok(None);
        };
        let offset = global_connection_index
            .checked_mul(self.stride)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "source port index times stride overflows",
                )
            })?;
        let port = (base as usize)
            .checked_add(offset)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source port overflows"))?;
        u16::try_from(port)
            .map(Some)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source port exceeds 65535"))
    }

    fn label(self) -> String {
        match self.base {
            Some(base) => format!("base={base}:stride={}", self.stride),
            None => "kernel-ephemeral".to_string(),
        }
    }
}

fn sockaddr_in_for(addr: std::net::SocketAddrV4) -> libc::sockaddr_in {
    libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: addr.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_be_bytes(addr.ip().octets()).to_be(),
        },
        sin_zero: [0; 8],
    }
}

fn sockaddr_in6_for(addr: std::net::SocketAddrV6) -> libc::sockaddr_in6 {
    libc::sockaddr_in6 {
        sin6_family: libc::AF_INET6 as libc::sa_family_t,
        sin6_port: addr.port().to_be(),
        sin6_flowinfo: addr.flowinfo(),
        sin6_addr: libc::in6_addr {
            s6_addr: addr.ip().octets(),
        },
        sin6_scope_id: addr.scope_id(),
    }
}

fn tcp_listener_reuseaddr(bind: &str, port: u16) -> io::Result<TcpListener> {
    let mut last_error = None;
    for addr in (bind, port).to_socket_addrs()? {
        let domain = match addr {
            SocketAddr::V4(_) => libc::AF_INET,
            SocketAddr::V6(_) => libc::AF_INET6,
        };
        let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            last_error = Some(io::Error::last_os_error());
            continue;
        }

        let result = (|| -> io::Result<()> {
            let reuse: libc::c_int = 1;
            let ret = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_REUSEADDR,
                    &reuse as *const libc::c_int as *const libc::c_void,
                    size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }

            match addr {
                SocketAddr::V4(addr) => {
                    let addr = sockaddr_in_for(addr);
                    let ret = unsafe {
                        libc::bind(
                            fd,
                            &addr as *const libc::sockaddr_in as *const libc::sockaddr,
                            size_of::<libc::sockaddr_in>() as libc::socklen_t,
                        )
                    };
                    if ret != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                SocketAddr::V6(addr) => {
                    let addr = sockaddr_in6_for(addr);
                    let ret = unsafe {
                        libc::bind(
                            fd,
                            &addr as *const libc::sockaddr_in6 as *const libc::sockaddr,
                            size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                        )
                    };
                    if ret != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
            }

            if unsafe { libc::listen(fd, libc::SOMAXCONN) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })();

        match result {
            Ok(()) => return Ok(unsafe { TcpListener::from_raw_fd(fd) }),
            Err(err) => {
                unsafe {
                    libc::close(fd);
                }
                last_error = Some(err);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("no socket address resolved for {bind}:{port}"),
        )
    }))
}

fn connect_tcp_bound(remote: SocketAddr, source_port: u16) -> io::Result<TcpStream> {
    let domain = match remote {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    };
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let result = (|| -> io::Result<()> {
        let reuse: libc::c_int = 1;
        unsafe {
            let _ = libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &reuse as *const libc::c_int as *const libc::c_void,
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }

        match remote {
            SocketAddr::V4(remote_v4) => {
                let local = sockaddr_in_for(std::net::SocketAddrV4::new(
                    std::net::Ipv4Addr::UNSPECIFIED,
                    source_port,
                ));
                let ret = unsafe {
                    libc::bind(
                        fd,
                        &local as *const libc::sockaddr_in as *const libc::sockaddr,
                        size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                };
                if ret != 0 {
                    return Err(io::Error::last_os_error());
                }
                let remote = sockaddr_in_for(remote_v4);
                let ret = unsafe {
                    libc::connect(
                        fd,
                        &remote as *const libc::sockaddr_in as *const libc::sockaddr,
                        size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                };
                if ret != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            SocketAddr::V6(remote_v6) => {
                let local = sockaddr_in6_for(std::net::SocketAddrV6::new(
                    std::net::Ipv6Addr::UNSPECIFIED,
                    source_port,
                    0,
                    0,
                ));
                let ret = unsafe {
                    libc::bind(
                        fd,
                        &local as *const libc::sockaddr_in6 as *const libc::sockaddr,
                        size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    )
                };
                if ret != 0 {
                    return Err(io::Error::last_os_error());
                }
                let remote = sockaddr_in6_for(remote_v6);
                let ret = unsafe {
                    libc::connect(
                        fd,
                        &remote as *const libc::sockaddr_in6 as *const libc::sockaddr,
                        size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    )
                };
                if ret != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }

        Ok(())
    })();

    if let Err(err) = result {
        unsafe {
            libc::close(fd);
        }
        return Err(err);
    }

    Ok(unsafe { TcpStream::from_raw_fd(fd) })
}

fn tcp_bench_connect(addr: &str, port: u16, source_port: Option<u16>) -> io::Result<TcpStream> {
    let Some(source_port) = source_port else {
        return TcpStream::connect((addr, port));
    };
    let remote = (addr, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no remote socket address"))?;
    connect_tcp_bound(remote, source_port)
}

fn udp_bench_connect(addr: &str, port: u16, source_port: Option<u16>) -> io::Result<UdpSocket> {
    let remote = (addr, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no remote socket address"))?;
    let local_port = source_port.unwrap_or(0);
    let local = match remote {
        SocketAddr::V4(_) => SocketAddr::V4(std::net::SocketAddrV4::new(
            std::net::Ipv4Addr::UNSPECIFIED,
            local_port,
        )),
        SocketAddr::V6(_) => SocketAddr::V6(std::net::SocketAddrV6::new(
            std::net::Ipv6Addr::UNSPECIFIED,
            local_port,
            0,
            0,
        )),
    };
    let socket = UdpSocket::bind(local)?;
    socket.connect(remote)?;
    set_udp_bench_buffers(&socket);
    Ok(socket)
}

fn tcp_bench_server(
    bind: &str,
    port: u16,
    connections: usize,
    expected_bytes: usize,
) -> io::Result<()> {
    let listener = TcpListener::bind((bind, port))?;
    println!(
        "tcp-bench-server: listening on {bind}:{port} connections={connections} \
         expected_bytes={expected_bytes}"
    );

    let mut streams = Vec::with_capacity(connections);
    for _ in 0..connections {
        let (stream, peer_addr) = listener.accept()?;
        set_tcp_bench_buffers(&stream);
        streams.push((stream, peer_addr));
    }

    let started = Instant::now();
    let mut handles = Vec::with_capacity(connections);
    for (mut stream, peer_addr) in streams {
        handles.push(thread::spawn(move || -> io::Result<usize> {
            let mut received = 0usize;
            let mut buf = vec![0u8; 1024 * 1024];
            while received < expected_bytes {
                let n = stream.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                received += n;
            }
            if received != expected_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("{peer_addr} sent {received}/{expected_bytes} bytes"),
                ));
            }
            Ok(received)
        }));
    }

    let mut total = 0usize;
    for handle in handles {
        total += handle.join().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "tcp bench server thread panicked")
        })??;
    }
    print_tcp_bench_result("tcp-bench-server", total, started.elapsed());
    Ok(())
}

fn tcp_bench_mux_server(
    bind: &str,
    base_port: u16,
    ports: usize,
    connections_per_port: usize,
    expected_bytes: usize,
) -> io::Result<()> {
    let total_connections = tcp_bench_total_connections(ports, connections_per_port)?;
    let mut listeners = Vec::with_capacity(ports);
    for lane in 0..ports {
        let port = tcp_bench_port(base_port, lane)?;
        listeners.push((lane, port, TcpListener::bind((bind, port))?));
    }

    println!(
        "tcp-bench-mux-server: listening on {bind}:{base_port}.. ports={ports} \
         connections_per_port={connections_per_port} total_connections={total_connections} \
         expected_bytes={expected_bytes}"
    );

    let mut accept_handles = Vec::with_capacity(ports);
    for (lane, port, listener) in listeners {
        accept_handles.push(thread::spawn(
            move || -> io::Result<Vec<(TcpStream, std::net::SocketAddr, usize, u16, usize)>> {
                let mut streams = Vec::with_capacity(connections_per_port);
                for conn_index in 0..connections_per_port {
                    let (stream, peer_addr) = listener.accept()?;
                    set_tcp_bench_buffers(&stream);
                    streams.push((stream, peer_addr, lane, port, conn_index));
                }
                Ok(streams)
            },
        ));
    }

    let mut streams = Vec::with_capacity(total_connections);
    for handle in accept_handles {
        streams.extend(handle.join().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "tcp bench mux accept thread panicked")
        })??);
    }

    let started = Instant::now();
    let mut handles = Vec::with_capacity(total_connections);
    for (mut stream, peer_addr, lane, port, conn_index) in streams {
        handles.push(thread::spawn(move || -> io::Result<usize> {
            let mut received = 0usize;
            let mut buf = vec![0u8; 1024 * 1024];
            while received < expected_bytes {
                let n = stream.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                received += n;
            }
            if received != expected_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "{peer_addr} lane={lane} port={port} conn={conn_index} \
                         sent {received}/{expected_bytes} bytes"
                    ),
                ));
            }
            Ok(received)
        }));
    }

    let mut total = 0usize;
    for handle in handles {
        total += handle.join().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "tcp bench mux server thread panicked")
        })??;
    }
    print_tcp_bench_result("tcp-bench-mux-server", total, started.elapsed());
    Ok(())
}

fn tcp_bench_send(
    addr: &str,
    port: u16,
    connections: usize,
    bytes_per_connection: usize,
    chunk_bytes: usize,
) -> io::Result<()> {
    let started = Instant::now();
    let chunk_bytes = chunk_bytes.max(4096);
    let source_ports = TcpSourcePortPlan::from_env()?;
    println!(
        "tcp-bench-send: addr={addr} port={port} connections={connections} \
         bytes_per_connection={bytes_per_connection} chunk_bytes={chunk_bytes} \
         source_ports={}",
        source_ports.label()
    );
    let mut handles = Vec::with_capacity(connections);
    for conn_index in 0..connections {
        let addr = addr.to_string();
        let source_port = source_ports.source_port(conn_index)?;
        handles.push(thread::spawn(move || -> io::Result<usize> {
            let mut stream = tcp_bench_connect(addr.as_str(), port, source_port)?;
            set_tcp_nodelay_from_env(&stream)?;
            set_tcp_bench_buffers(&stream);
            let buf = vec![0u8; chunk_bytes];
            let mut sent = 0usize;
            while sent < bytes_per_connection {
                let len = (bytes_per_connection - sent).min(buf.len());
                stream.write_all(&buf[..len])?;
                sent += len;
            }
            stream.shutdown(Shutdown::Write)?;
            Ok(sent)
        }));
    }

    let mut total = 0usize;
    for handle in handles {
        total += handle.join().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "tcp bench client thread panicked")
        })??;
    }
    print_tcp_bench_result("tcp-bench-send", total, started.elapsed());
    Ok(())
}

fn tcp_bench_mux_send(
    addr: &str,
    base_port: u16,
    ports: usize,
    connections_per_port: usize,
    bytes_per_connection: usize,
    chunk_bytes: usize,
) -> io::Result<()> {
    let total_connections = tcp_bench_total_connections(ports, connections_per_port)?;
    let started = Instant::now();
    let chunk_bytes = chunk_bytes.max(4096);
    let source_ports = TcpSourcePortPlan::from_env()?;
    println!(
        "tcp-bench-mux-send: addr={addr} base_port={base_port} ports={ports} \
         connections_per_port={connections_per_port} total_connections={total_connections} \
         bytes_per_connection={bytes_per_connection} chunk_bytes={chunk_bytes} \
         source_ports={}",
        source_ports.label()
    );
    let mut handles = Vec::with_capacity(total_connections);
    for lane in 0..ports {
        let port = tcp_bench_port(base_port, lane)?;
        for conn_index in 0..connections_per_port {
            let addr = addr.to_string();
            let global_index = lane
                .checked_mul(connections_per_port)
                .and_then(|base| base.checked_add(conn_index))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "connection index overflow")
                })?;
            let source_port = source_ports.source_port(global_index)?;
            handles.push(thread::spawn(move || -> io::Result<usize> {
                let mut stream =
                    tcp_bench_connect(addr.as_str(), port, source_port).map_err(|err| {
                        io::Error::new(
                            err.kind(),
                            format!("connect {addr}:{port} lane={lane} conn={conn_index}: {err}"),
                        )
                    })?;
                set_tcp_nodelay_from_env(&stream)?;
                set_tcp_bench_buffers(&stream);
                let buf = vec![0u8; chunk_bytes];
                let mut sent = 0usize;
                while sent < bytes_per_connection {
                    let len = (bytes_per_connection - sent).min(buf.len());
                    stream.write_all(&buf[..len])?;
                    sent += len;
                }
                stream.shutdown(Shutdown::Write)?;
                Ok(sent)
            }));
        }
    }

    let mut total = 0usize;
    for handle in handles {
        total += handle.join().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "tcp bench mux client thread panicked")
        })??;
    }
    print_tcp_bench_result("tcp-bench-mux-send", total, started.elapsed());
    Ok(())
}

fn tcp_bench_u32_len(value: usize, name: &str) -> io::Result<u32> {
    u32::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must fit in u32"),
        )
    })
}

fn tcp_bench_auto_workers(requested: usize, total_connections: usize) -> usize {
    if requested > 0 {
        return requested.min(total_connections.max(1));
    }

    let available = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    total_connections.clamp(1, available)
}

fn tcp_bench_mux_bind_listeners(
    bind: &str,
    base_port: u16,
    ports: usize,
) -> io::Result<Vec<(usize, u16, TcpListener)>> {
    let mut listeners = Vec::with_capacity(ports);
    for lane in 0..ports {
        let port = tcp_bench_port(base_port, lane)?;
        listeners.push((lane, port, tcp_listener_reuseaddr(bind, port)?));
    }
    Ok(listeners)
}

struct TcpBenchAcceptedStream {
    lane: usize,
    port: u16,
    conn_index: usize,
    peer_addr: SocketAddr,
    locality: SocketLocality,
    stream: TcpStream,
}

#[derive(Clone, Copy)]
struct TcpBenchStreamMeta {
    lane: usize,
    port: u16,
    conn_index: usize,
    peer_addr: SocketAddr,
    locality: SocketLocality,
}

impl TcpBenchAcceptedStream {
    fn meta(&self) -> TcpBenchStreamMeta {
        TcpBenchStreamMeta {
            lane: self.lane,
            port: self.port,
            conn_index: self.conn_index,
            peer_addr: self.peer_addr,
            locality: self.locality,
        }
    }
}

fn maybe_clone_ready_handshake_streams(
    streams: &[TcpBenchAcceptedStream],
) -> io::Result<Vec<TcpStream>> {
    if !ready_handshake_enabled() {
        return Ok(Vec::new());
    }
    clone_tcp_bench_control_streams(streams)
}

fn clone_tcp_bench_control_streams(
    streams: &[TcpBenchAcceptedStream],
) -> io::Result<Vec<TcpStream>> {
    streams
        .iter()
        .map(|stream| stream.stream.try_clone())
        .collect()
}

fn recv_tcp_control_byte_from_accepted(
    streams: &mut [TcpBenchAcceptedStream],
    expected: u8,
    label: &str,
    phase: &str,
) -> io::Result<()> {
    for accepted in streams.iter_mut() {
        let mut stream = &accepted.stream;
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte)?;
        if byte[0] != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{label}: {phase} byte mismatch from peer={} lane={} conn={}: got 0x{:02x} expected 0x{expected:02x}",
                    accepted.peer_addr, accepted.lane, accepted.conn_index, byte[0],
                ),
            ));
        }
    }
    println!("{label}: {phase}=received streams={}", streams.len());
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TcpMuxShardPolicy {
    RoundRobin,
    PortLane,
    IncomingCpu,
    IncomingNapi,
    IncomingNapiBase,
    Observed,
}

impl TcpMuxShardPolicy {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "" | "observed" | "auto" => Ok(Self::Observed),
            "round-robin" | "rr" => Ok(Self::RoundRobin),
            "port-lane" | "lane" | "port" => Ok(Self::PortLane),
            "incoming-cpu" | "cpu" => Ok(Self::IncomingCpu),
            "incoming-napi" | "napi" => Ok(Self::IncomingNapi),
            "incoming-napi-base" | "napi-base" | "rxq-napi-base" => Ok(Self::IncomingNapiBase),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown shard policy {other:?}; use observed, round-robin, port-lane, incoming-cpu, incoming-napi, or incoming-napi-base"
                ),
            )),
        }
    }

    fn from_env_or(name: &str, default: Self) -> io::Result<Self> {
        match env::var(name) {
            Ok(value) => Self::parse(value.trim()),
            Err(_) => Ok(default),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::RoundRobin => "round-robin",
            Self::PortLane => "port-lane",
            Self::IncomingCpu => "incoming-cpu",
            Self::IncomingNapi => "incoming-napi",
            Self::IncomingNapiBase => "incoming-napi-base",
            Self::Observed => "observed",
        }
    }

    fn pinned_worker_for_incoming_cpu(
        cpu: i32,
        workers: usize,
        pin_requested: bool,
    ) -> Option<usize> {
        if !pin_requested && !env_truthy("URING_PLAY_PIN_CPUS") {
            return None;
        }

        let cpu = cpu as usize;
        (0..workers).find(|worker| configured_affinity_target_cpu(*worker) == cpu)
    }

    fn worker_for_incoming_cpu(cpu: i32, workers: usize, pin_requested: bool) -> usize {
        if let Some(worker) = Self::pinned_worker_for_incoming_cpu(cpu, workers, pin_requested) {
            return worker;
        }

        let cpu = cpu as usize;
        cpu % workers
    }

    fn worker_for_rxq(rxq: usize, workers: usize) -> usize {
        let rxq_base = env_usize_or("URING_PLAY_ZCRX_RXQ", 0);
        if rxq >= rxq_base && rxq < rxq_base + workers {
            rxq - rxq_base
        } else {
            rxq % workers
        }
    }

    fn worker_for_incoming_napi_map(napi_id: i32, workers: usize) -> Option<usize> {
        let map = env::var("URING_PLAY_ZCRX_NAPI_RXQ_MAP").ok()?;
        for entry in map.split(',') {
            let entry = entry.trim();
            let Some((napi, rxq)) = entry.split_once(':').or_else(|| entry.split_once('=')) else {
                continue;
            };
            let Ok(napi) = napi.trim().parse::<i32>() else {
                continue;
            };
            if napi != napi_id {
                continue;
            }
            let Ok(rxq) = rxq.trim().parse::<usize>() else {
                continue;
            };
            return Some(Self::worker_for_rxq(rxq, workers));
        }
        None
    }

    fn worker_for_incoming_napi_base(napi_id: i32, workers: usize) -> Option<usize> {
        if let Some(worker) = Self::worker_for_incoming_napi_map(napi_id, workers) {
            return Some(worker);
        }

        let base = env_usize_or("URING_PLAY_ZCRX_NAPI_BASE", 0);
        if base == 0 {
            return None;
        }

        let napi = usize::try_from(napi_id).ok()?;
        let rxq = napi.checked_sub(base)?;
        Some(Self::worker_for_rxq(rxq, workers))
    }

    fn choose_worker_with_pin(
        self,
        accepted: &TcpBenchAcceptedStream,
        index: usize,
        workers: usize,
        pin_requested: bool,
    ) -> usize {
        debug_assert!(workers > 0);
        match self {
            Self::RoundRobin => index % workers,
            Self::PortLane => accepted.lane % workers,
            Self::IncomingCpu => accepted
                .locality
                .incoming_cpu_known()
                .then_some(Self::worker_for_incoming_cpu(
                    accepted.locality.incoming_cpu,
                    workers,
                    pin_requested,
                ))
                .unwrap_or(accepted.lane % workers),
            Self::IncomingNapi => accepted
                .locality
                .incoming_napi_known()
                .then_some(accepted.locality.incoming_napi_id as usize % workers)
                .or_else(|| {
                    if accepted.locality.incoming_cpu_known() {
                        Self::pinned_worker_for_incoming_cpu(
                            accepted.locality.incoming_cpu,
                            workers,
                            pin_requested,
                        )
                    } else {
                        None
                    }
                })
                .unwrap_or(accepted.lane % workers),
            Self::IncomingNapiBase => accepted
                .locality
                .incoming_napi_known()
                .then(|| {
                    Self::worker_for_incoming_napi_base(accepted.locality.incoming_napi_id, workers)
                })
                .flatten()
                .or_else(|| {
                    if accepted.locality.incoming_cpu_known() {
                        Self::pinned_worker_for_incoming_cpu(
                            accepted.locality.incoming_cpu,
                            workers,
                            pin_requested,
                        )
                    } else {
                        None
                    }
                })
                .unwrap_or(accepted.lane % workers),
            Self::Observed => accepted
                .locality
                .incoming_napi_known()
                .then_some(accepted.locality.incoming_napi_id as usize % workers)
                .or_else(|| {
                    if accepted.locality.incoming_cpu_known() {
                        Self::pinned_worker_for_incoming_cpu(
                            accepted.locality.incoming_cpu,
                            workers,
                            pin_requested,
                        )
                    } else {
                        None
                    }
                })
                .unwrap_or(accepted.lane % workers),
        }
    }

    fn choose_worker(
        self,
        accepted: &TcpBenchAcceptedStream,
        index: usize,
        workers: usize,
    ) -> usize {
        self.choose_worker_with_pin(accepted, index, workers, false)
    }
}

fn tcp_bench_mux_accept_tagged_listeners(
    listeners: Vec<(usize, u16, TcpListener)>,
    ports: usize,
    connections_per_port: usize,
) -> io::Result<Vec<TcpBenchAcceptedStream>> {
    let total_connections = tcp_bench_total_connections(ports, connections_per_port)?;
    let mut accept_handles = Vec::with_capacity(ports);
    for (lane, port, listener) in listeners {
        accept_handles.push(thread::spawn(
            move || -> io::Result<Vec<TcpBenchAcceptedStream>> {
                let mut streams = Vec::with_capacity(connections_per_port);
                for conn_index in 0..connections_per_port {
                    let (stream, peer_addr) = listener.accept()?;
                    set_tcp_nodelay_from_env(&stream)?;
                    set_tcp_bench_buffers(&stream);
                    let locality = observe_socket_locality(&stream);
                    println!(
                        "tcp-bench-uring-mux-server: accepted peer={peer_addr} lane={lane} \
                     port={port} conn={conn_index} {}",
                        socket_locality_label(locality)
                    );
                    streams.push(TcpBenchAcceptedStream {
                        lane,
                        port,
                        conn_index,
                        peer_addr,
                        locality,
                        stream,
                    });
                }
                Ok(streams)
            },
        ));
    }

    let mut streams = Vec::with_capacity(total_connections);
    for handle in accept_handles {
        streams.extend(handle.join().map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "tcp bench uring mux accept thread panicked",
            )
        })??);
    }
    Ok(streams)
}

fn tcp_bench_partition_accepted_streams(
    streams: Vec<TcpBenchAcceptedStream>,
    workers: usize,
    policy: TcpMuxShardPolicy,
    label: &str,
) -> Vec<Vec<TcpStream>> {
    tcp_bench_partition_accepted_streams_with_pin(streams, workers, policy, label, false)
}

fn tcp_bench_partition_accepted_streams_with_pin(
    streams: Vec<TcpBenchAcceptedStream>,
    workers: usize,
    policy: TcpMuxShardPolicy,
    label: &str,
    pin_requested: bool,
) -> Vec<Vec<TcpStream>> {
    let mut shards = (0..workers).map(|_| Vec::new()).collect::<Vec<_>>();
    let mut counts = vec![0usize; workers];
    for (idx, accepted) in streams.into_iter().enumerate() {
        let worker = policy.choose_worker_with_pin(&accepted, idx, workers, pin_requested);
        println!(
            "{label}-mux-assignment: worker={worker} policy={} peer={} listener_lane={} \
             listener_port={} conn={} {}",
            policy.label(),
            accepted.peer_addr,
            accepted.lane,
            accepted.port,
            accepted.conn_index,
            socket_locality_label(accepted.locality)
        );
        counts[worker] += 1;
        shards[worker].push(accepted.stream);
    }
    let counts = counts
        .iter()
        .enumerate()
        .map(|(worker, count)| format!("{worker}:{count}"))
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "{label}-mux-assignment-summary: policy={} workers={workers} streams_by_worker={counts}",
        policy.label()
    );
    shards
}

struct TcpWalZcrxWorkerStream {
    meta: TcpBenchStreamMeta,
    stream: TcpStream,
}

fn tcp_wal_zcrx_partition_accepted_streams_with_pin(
    streams: Vec<TcpBenchAcceptedStream>,
    workers: usize,
    policy: TcpMuxShardPolicy,
    label: &str,
    pin_requested: bool,
) -> Vec<Vec<TcpWalZcrxWorkerStream>> {
    let mut shards = (0..workers).map(|_| Vec::new()).collect::<Vec<_>>();
    let mut counts = vec![0usize; workers];
    for (idx, accepted) in streams.into_iter().enumerate() {
        let worker = policy.choose_worker_with_pin(&accepted, idx, workers, pin_requested);
        println!(
            "{label}-mux-assignment: worker={worker} policy={} peer={} listener_lane={} \
             listener_port={} conn={} {}",
            policy.label(),
            accepted.peer_addr,
            accepted.lane,
            accepted.port,
            accepted.conn_index,
            socket_locality_label(accepted.locality)
        );
        counts[worker] += 1;
        shards[worker].push(TcpWalZcrxWorkerStream {
            meta: accepted.meta(),
            stream: accepted.stream,
        });
    }
    let counts = counts
        .iter()
        .enumerate()
        .map(|(worker, count)| format!("{worker}:{count}"))
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "{label}-mux-assignment-summary: policy={} workers={workers} streams_by_worker={counts}",
        policy.label()
    );
    shards
}

fn tcp_mux_counts_label(counts: &[usize]) -> String {
    counts
        .iter()
        .enumerate()
        .map(|(worker, count)| format!("{worker}:{count}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn tcp_mux_balance_score(counts: &[usize], expected_workers: usize) -> (usize, usize, usize) {
    let active = counts.iter().filter(|count| **count > 0).count();
    let missing = expected_workers.saturating_sub(active);
    let max_count = counts.iter().copied().max().unwrap_or(0);
    let sum_squares = counts
        .iter()
        .map(|count| count.saturating_mul(*count))
        .sum();
    (missing, max_count, sum_squares)
}

fn tcp_wal_zcrx_selected_connections_per_port(connections_per_port: usize) -> io::Result<usize> {
    let selected = env_usize_or(
        "URING_PLAY_TCP_WAL_ZCRX_SELECT_PER_PORT",
        connections_per_port,
    );
    if selected == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "URING_PLAY_TCP_WAL_ZCRX_SELECT_PER_PORT must be greater than zero",
        ));
    }
    if selected > connections_per_port {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "URING_PLAY_TCP_WAL_ZCRX_SELECT_PER_PORT={selected} exceeds connections_per_port={connections_per_port}"
            ),
        ));
    }
    Ok(selected)
}

fn tcp_bench_mux_selected_connections_per_port(connections_per_port: usize) -> io::Result<usize> {
    let selected = env_usize_or("URING_PLAY_TCP_MUX_SELECT_PER_PORT", connections_per_port);
    if selected == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "URING_PLAY_TCP_MUX_SELECT_PER_PORT must be greater than zero",
        ));
    }
    if selected > connections_per_port {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "URING_PLAY_TCP_MUX_SELECT_PER_PORT={selected} exceeds connections_per_port={connections_per_port}"
            ),
        ));
    }
    Ok(selected)
}

fn tcp_mux_combination_count_capped(n: usize, k: usize, cap: usize) -> usize {
    if k > n {
        return 0;
    }
    let k = k.min(n - k);
    let mut count = 1usize;
    for idx in 1..=k {
        let numerator = n + 1 - idx;
        count = match count.checked_mul(numerator) {
            Some(value) => value / idx,
            None => return cap.saturating_add(1),
        };
        if count > cap {
            return cap.saturating_add(1);
        }
    }
    count
}

fn tcp_mux_combination_product_capped(choice_count: usize, lanes: usize, cap: usize) -> usize {
    let mut count = 1usize;
    for _ in 0..lanes {
        count = match count.checked_mul(choice_count) {
            Some(value) => value,
            None => return cap.saturating_add(1),
        };
        if count > cap {
            return cap.saturating_add(1);
        }
    }
    count
}

fn tcp_mux_conn_index_combinations(n: usize, k: usize) -> Vec<Vec<usize>> {
    fn search(
        start: usize,
        remaining: usize,
        n: usize,
        chosen: &mut Vec<usize>,
        combinations: &mut Vec<Vec<usize>>,
    ) {
        if remaining == 0 {
            combinations.push(chosen.clone());
            return;
        }

        let end = n.saturating_sub(remaining);
        for index in start..=end {
            chosen.push(index);
            search(index + 1, remaining - 1, n, chosen, combinations);
            chosen.pop();
        }
    }

    if k > n {
        return Vec::new();
    }
    let mut combinations = Vec::new();
    let mut chosen = Vec::with_capacity(k);
    search(0, k, n, &mut chosen, &mut combinations);
    combinations
}

fn tcp_mux_find_best_conn_combination(
    conn_counts: &[Vec<usize>],
    selected_connections_per_port: usize,
    expected_workers: usize,
) -> Option<(Vec<usize>, Vec<usize>)> {
    fn search(
        start: usize,
        remaining: usize,
        conn_counts: &[Vec<usize>],
        expected_workers: usize,
        counts: &mut [usize],
        chosen: &mut Vec<usize>,
        best: &mut Option<((usize, usize, usize, Vec<usize>), Vec<usize>)>,
    ) {
        if remaining == 0 {
            let (missing, max_count, sum_squares) = tcp_mux_balance_score(counts, expected_workers);
            let score = (missing, max_count, sum_squares, chosen.clone());
            if best
                .as_ref()
                .map(|(best_score, _)| score < *best_score)
                .unwrap_or(true)
            {
                *best = Some((score, counts.to_vec()));
            }
            return;
        }

        let end = conn_counts.len().saturating_sub(remaining);
        for conn_index in start..=end {
            for (worker, count) in conn_counts[conn_index].iter().copied().enumerate() {
                counts[worker] += count;
            }
            chosen.push(conn_index);
            search(
                conn_index + 1,
                remaining - 1,
                conn_counts,
                expected_workers,
                counts,
                chosen,
                best,
            );
            chosen.pop();
            for (worker, count) in conn_counts[conn_index].iter().copied().enumerate() {
                counts[worker] -= count;
            }
        }
    }

    let workers = conn_counts.first().map(Vec::len).unwrap_or(0);
    let mut counts = vec![0usize; workers];
    let mut chosen = Vec::with_capacity(selected_connections_per_port);
    let mut best = None;
    search(
        0,
        selected_connections_per_port,
        conn_counts,
        expected_workers,
        &mut counts,
        &mut chosen,
        &mut best,
    );
    best.map(|((_, _, _, chosen), counts)| (chosen, counts))
}

fn tcp_mux_find_best_lane_conn_combinations(
    lane_conn_counts: &[Vec<Vec<usize>>],
    selected_connections_per_lane: usize,
    expected_workers: usize,
    combination_limit: usize,
) -> Option<(Vec<Vec<usize>>, Vec<usize>, &'static str, usize)> {
    let lanes = lane_conn_counts.len();
    let connections_per_lane = lane_conn_counts.first()?.len();
    let workers = lane_conn_counts.first()?.first()?.len();
    if selected_connections_per_lane == 0 || selected_connections_per_lane > connections_per_lane {
        return None;
    }

    let conn_combinations =
        tcp_mux_conn_index_combinations(connections_per_lane, selected_connections_per_lane);
    let lane_choices = lane_conn_counts
        .iter()
        .map(|conn_counts| {
            conn_combinations
                .iter()
                .map(|indices| {
                    let mut counts = vec![0usize; workers];
                    for conn_index in indices {
                        let per_worker = conn_counts.get(*conn_index)?;
                        if per_worker.len() != workers {
                            return None;
                        }
                        for (worker, count) in per_worker.iter().copied().enumerate() {
                            counts[worker] += count;
                        }
                    }
                    Some((indices.clone(), counts))
                })
                .collect::<Option<Vec<_>>>()
        })
        .collect::<Option<Vec<_>>>()?;

    let combination_count =
        tcp_mux_combination_product_capped(conn_combinations.len(), lanes, combination_limit);
    if combination_count <= combination_limit {
        fn search(
            lane: usize,
            lane_choices: &[Vec<(Vec<usize>, Vec<usize>)>],
            expected_workers: usize,
            counts: &mut [usize],
            chosen: &mut Vec<Vec<usize>>,
            best: &mut Option<((usize, usize, usize, Vec<Vec<usize>>), Vec<usize>)>,
        ) {
            if lane == lane_choices.len() {
                let (missing, max_count, sum_squares) =
                    tcp_mux_balance_score(counts, expected_workers);
                let score = (missing, max_count, sum_squares, chosen.clone());
                if best
                    .as_ref()
                    .map(|(best_score, _)| score < *best_score)
                    .unwrap_or(true)
                {
                    *best = Some((score, counts.to_vec()));
                }
                return;
            }

            for (indices, lane_counts) in &lane_choices[lane] {
                for (worker, count) in lane_counts.iter().copied().enumerate() {
                    counts[worker] += count;
                }
                chosen.push(indices.clone());
                search(
                    lane + 1,
                    lane_choices,
                    expected_workers,
                    counts,
                    chosen,
                    best,
                );
                chosen.pop();
                for (worker, count) in lane_counts.iter().copied().enumerate() {
                    counts[worker] -= count;
                }
            }
        }

        let mut counts = vec![0usize; workers];
        let mut chosen = Vec::with_capacity(lanes);
        let mut best = None;
        search(
            0,
            &lane_choices,
            expected_workers,
            &mut counts,
            &mut chosen,
            &mut best,
        );
        return best.map(|((_, _, _, chosen), counts)| {
            (chosen, counts, "per-lane-exhaustive", combination_count)
        });
    }

    let mut aggregate_counts = vec![0usize; workers];
    let mut selected = Vec::with_capacity(lanes);
    for choices in lane_choices {
        let mut best: Option<((usize, usize, usize, Vec<usize>), Vec<usize>, Vec<usize>)> = None;
        for (indices, lane_counts) in choices {
            let mut counts = aggregate_counts.clone();
            for (worker, count) in lane_counts.iter().copied().enumerate() {
                counts[worker] += count;
            }
            let (missing, max_count, sum_squares) =
                tcp_mux_balance_score(&counts, expected_workers);
            let score = (missing, max_count, sum_squares, indices.clone());
            if best
                .as_ref()
                .map(|(best_score, _, _)| score < *best_score)
                .unwrap_or(true)
            {
                best = Some((score, indices, counts));
            }
        }
        let (_, indices, counts) = best?;
        selected.push(indices);
        aggregate_counts = counts;
    }

    Some((
        selected,
        aggregate_counts,
        "per-lane-greedy",
        combination_count,
    ))
}

fn tcp_mux_lane_selection_label(selection: &[Vec<usize>]) -> String {
    selection
        .iter()
        .enumerate()
        .map(|(lane, indices)| {
            let indices = indices
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join("+");
            format!("{lane}:{indices}")
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn tcp_bench_select_live_conn_indices_per_lane(
    streams: Vec<TcpBenchAcceptedStream>,
    ports: usize,
    connections_per_port: usize,
    selected_connections_per_port: usize,
    workers: usize,
    policy: TcpMuxShardPolicy,
    label: &str,
    pin_requested: bool,
) -> io::Result<Vec<TcpBenchAcceptedStream>> {
    if selected_connections_per_port == connections_per_port {
        return Ok(streams);
    }

    let total_connections = tcp_bench_total_connections(ports, connections_per_port)?;
    let selected_total_connections =
        tcp_bench_total_connections(ports, selected_connections_per_port)?;
    let mut grid = (0..total_connections)
        .map(|_| None)
        .collect::<Vec<Option<TcpBenchAcceptedStream>>>();

    for stream in streams {
        if stream.lane >= ports || stream.conn_index >= connections_per_port {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{label}: accepted stream out of candidate grid lane={} conn={} ports={ports} connections_per_port={connections_per_port}",
                    stream.lane, stream.conn_index
                ),
            ));
        }
        let index = stream
            .lane
            .checked_mul(connections_per_port)
            .and_then(|base| base.checked_add(stream.conn_index))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "accepted stream index overflow",
                )
            })?;
        if grid[index].is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{label}: duplicate accepted stream lane={} conn={}",
                    stream.lane, stream.conn_index
                ),
            ));
        }
        grid[index] = Some(stream);
    }

    if grid.iter().any(Option::is_none) {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("{label}: accepted fewer streams than expected for live 5-tuple selection"),
        ));
    }

    let expected_workers = workers.min(selected_total_connections).max(1);
    let lane_conn_counts = (0..ports)
        .map(|lane| {
            (0..connections_per_port)
                .map(|conn_index| {
                    let mut counts = vec![0usize; workers];
                    let grid_index = lane * connections_per_port + conn_index;
                    let accepted = grid[grid_index].as_ref().expect("grid was validated");
                    let worker =
                        policy.choose_worker_with_pin(accepted, grid_index, workers, pin_requested);
                    counts[worker] += 1;
                    counts
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let combination_limit = env_usize_or("URING_PLAY_TCP_WAL_ZCRX_SELECT_COMBO_LIMIT", 1_000_000);
    let select_mode =
        env::var("URING_PLAY_TCP_WAL_ZCRX_SELECT_MODE").unwrap_or_else(|_| "global".to_string());
    let (selected_conn_indices_by_lane, aggregate_counts, selection_mode, combination_count) =
        match select_mode.as_str() {
            "per-lane" | "per_lane" => tcp_mux_find_best_lane_conn_combinations(
                &lane_conn_counts,
                selected_connections_per_port,
                expected_workers,
                combination_limit,
            )
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{label}: no source-port candidate combination found"),
                )
            })?,
            "global" => {
                let conn_counts = (0..connections_per_port)
                    .map(|conn_index| {
                        let mut counts = vec![0usize; workers];
                        for lane_counts in &lane_conn_counts {
                            for (worker, count) in
                                lane_counts[conn_index].iter().copied().enumerate()
                            {
                                counts[worker] += count;
                            }
                        }
                        counts
                    })
                    .collect::<Vec<_>>();
                let combination_count = tcp_mux_combination_count_capped(
                    connections_per_port,
                    selected_connections_per_port,
                    combination_limit,
                );
                let (mut selected_conn_indices, aggregate_counts, selection_mode) =
                    if combination_count <= combination_limit {
                        let (selected, counts) = tcp_mux_find_best_conn_combination(
                            &conn_counts,
                            selected_connections_per_port,
                            expected_workers,
                        )
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("{label}: no source-port candidate combination found"),
                            )
                        })?;
                        (selected, counts, "global-exhaustive")
                    } else {
                        let mut selected_conn_indices =
                            Vec::with_capacity(selected_connections_per_port);
                        let mut selected = vec![false; connections_per_port];
                        let mut aggregate_counts = vec![0usize; workers];
                        for _ in 0..selected_connections_per_port {
                            let mut best: Option<(
                                (usize, usize, usize, usize),
                                usize,
                                Vec<usize>,
                            )> = None;
                            for conn_index in 0..connections_per_port {
                                if selected[conn_index] {
                                    continue;
                                }
                                let mut counts = aggregate_counts.clone();
                                for (worker, count) in
                                    conn_counts[conn_index].iter().copied().enumerate()
                                {
                                    counts[worker] += count;
                                }
                                let (missing, max_count, sum_squares) =
                                    tcp_mux_balance_score(&counts, expected_workers);
                                let score = (missing, max_count, sum_squares, conn_index);
                                if best
                                    .as_ref()
                                    .map(|(best_score, _, _)| score < *best_score)
                                    .unwrap_or(true)
                                {
                                    best = Some((score, conn_index, counts));
                                }
                            }
                            let Some((_score, conn_index, counts)) = best else {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    format!("{label}: no source-port candidates left to select"),
                                ));
                            };
                            selected[conn_index] = true;
                            selected_conn_indices.push(conn_index);
                            aggregate_counts = counts;
                        }
                        (selected_conn_indices, aggregate_counts, "global-greedy")
                    };
                selected_conn_indices.sort_unstable();
                (
                    vec![selected_conn_indices; ports],
                    aggregate_counts,
                    selection_mode,
                    combination_count,
                )
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "unknown URING_PLAY_TCP_WAL_ZCRX_SELECT_MODE={other:?}; use global or per-lane"
                    ),
                ));
            }
        };
    let mut selected_streams = Vec::with_capacity(selected_total_connections);
    for lane in 0..ports {
        for conn_index in &selected_conn_indices_by_lane[lane] {
            let grid_index = lane * connections_per_port + *conn_index;
            selected_streams.push(grid[grid_index].take().expect("selected stream missing"));
        }
    }

    println!(
        "{label}-live-select: policy={} ports={ports} candidate_connections_per_port={connections_per_port} \
         selected_connections_per_port={selected_connections_per_port} selected_conn_indices_by_lane={} \
         workers={workers} streams_by_worker={} selection_mode={selection_mode} combinations={} rejected_streams={}",
        policy.label(),
        tcp_mux_lane_selection_label(&selected_conn_indices_by_lane),
        tcp_mux_counts_label(&aggregate_counts),
        combination_count.min(combination_limit),
        total_connections.saturating_sub(selected_streams.len())
    );
    Ok(selected_streams)
}

#[derive(Clone, Copy)]
struct TcpBenchConnectSpec {
    lane: usize,
    port: u16,
    conn_index: usize,
    source_port: Option<u16>,
}

fn tcp_bench_mux_connect_specs(
    base_port: u16,
    ports: usize,
    connections_per_port: usize,
    source_ports: TcpSourcePortPlan,
) -> io::Result<Vec<TcpBenchConnectSpec>> {
    let total_connections = tcp_bench_total_connections(ports, connections_per_port)?;
    let mut specs = Vec::with_capacity(total_connections);
    for lane in 0..ports {
        let port = tcp_bench_port(base_port, lane)?;
        for conn_index in 0..connections_per_port {
            let global_index = lane
                .checked_mul(connections_per_port)
                .and_then(|base| base.checked_add(conn_index))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "connection index overflow")
                })?;
            specs.push(TcpBenchConnectSpec {
                lane,
                port,
                conn_index,
                source_port: source_ports.source_port(global_index)?,
            });
        }
    }
    Ok(specs)
}

fn tcp_bench_partition_connect_specs(
    specs: Vec<TcpBenchConnectSpec>,
    workers: usize,
) -> Vec<Vec<TcpBenchConnectSpec>> {
    let mut shards = (0..workers).map(|_| Vec::new()).collect::<Vec<_>>();
    for (idx, spec) in specs.into_iter().enumerate() {
        shards[idx % workers].push(spec);
    }
    shards
}

fn tcp_bench_connect_worker_streams(
    addr: &str,
    specs: &[TcpBenchConnectSpec],
) -> io::Result<Vec<TcpStream>> {
    let mut streams = Vec::with_capacity(specs.len());
    for spec in specs {
        let stream = tcp_bench_connect(addr, spec.port, spec.source_port).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "connect {addr}:{} lane={} conn={}: {err}",
                    spec.port, spec.lane, spec.conn_index
                ),
            )
        })?;
        set_tcp_nodelay_from_env(&stream)?;
        set_tcp_bench_buffers(&stream);
        streams.push(stream);
    }
    Ok(streams)
}

fn uring_recv_worker(
    streams: Vec<TcpStream>,
    expected_bytes: usize,
    recv_bytes: usize,
    ring_entries: u32,
) -> io::Result<usize> {
    if streams.is_empty() || expected_bytes == 0 {
        return Ok(0);
    }

    let recv_bytes = tcp_bench_u32_len(recv_bytes.max(4096), "recv bytes")?;
    let ring_entries = ring_entries.max((streams.len() as u32).saturating_mul(2).max(64));
    let mut ring = RawRing::new(ring_entries, ring_entries.saturating_mul(2))?;
    ring.register_napi_from_env("uring-recv-worker")?;
    let mut buffers = (0..streams.len())
        .map(|_| vec![0u8; recv_bytes as usize])
        .collect::<Vec<_>>();
    let mut received = vec![0usize; streams.len()];
    let mut complete = 0usize;

    for (idx, stream) in streams.iter().enumerate() {
        let len = (expected_bytes - received[idx]).min(recv_bytes as usize);
        ring.queue_recv(
            stream.as_raw_fd(),
            buffers[idx].as_mut_ptr(),
            len as u32,
            0,
            idx as u64,
        )?;
    }
    ring.submit_pending()?;

    while complete < streams.len() {
        let cqe = ring.wait_cqe()?;
        let idx = cqe.user_data as usize;
        if idx >= streams.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected recv CQE user_data={}", cqe.user_data),
            ));
        }
        if cqe.res < 0 {
            return Err(io::Error::from_raw_os_error(-cqe.res));
        }
        if cqe.res == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{idx} sent {}/{} bytes", received[idx], expected_bytes),
            ));
        }

        received[idx] += cqe.res as usize;
        if received[idx] > expected_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{idx} sent too many bytes: {}/{}",
                    received[idx], expected_bytes
                ),
            ));
        }

        if received[idx] == expected_bytes {
            complete += 1;
            continue;
        }

        let len = (expected_bytes - received[idx]).min(recv_bytes as usize);
        ring.queue_recv(
            streams[idx].as_raw_fd(),
            buffers[idx].as_mut_ptr(),
            len as u32,
            0,
            idx as u64,
        )?;
    }

    Ok(received.into_iter().sum())
}

fn uring_zcrx_recv_worker(
    rxq: u32,
    affinity: ThreadAffinity,
    consume_mode: ZcrxConsumeMode,
    ring: &mut RawRing,
    zcrx: &mut ZcrxContext,
    streams: Vec<TcpStream>,
    expected_bytes: usize,
    recv_bytes: usize,
) -> io::Result<ZcrxWorkerStats> {
    let tid = current_tid();
    let start_wall = Instant::now();
    let start_thread_cpu = thread_cpu_time().unwrap_or_default();
    let start_cpu = current_cpu();
    let start_switches = read_thread_context_switches(tid).unwrap_or_default();
    let stream_count = streams.len();

    if streams.is_empty() || expected_bytes == 0 {
        let end_thread_cpu = thread_cpu_time().unwrap_or(start_thread_cpu);
        let end_cpu = current_cpu();
        let end_switches = read_thread_context_switches(tid).unwrap_or(start_switches);
        return Ok(ZcrxWorkerStats {
            rxq,
            streams: stream_count,
            bytes: 0,
            consumed_bytes: 0,
            skipped_bytes: 0,
            frames: 0,
            checksum: 0,
            wall: start_wall.elapsed(),
            cpu: end_thread_cpu.saturating_sub(start_thread_cpu),
            target_cpu: affinity.target_cpu,
            affinity_applied: affinity.applied,
            start_cpu,
            end_cpu,
            voluntary_switches: end_switches
                .voluntary
                .saturating_sub(start_switches.voluntary),
            involuntary_switches: end_switches
                .involuntary
                .saturating_sub(start_switches.involuntary),
            migrations: end_switches
                .migrations
                .saturating_sub(start_switches.migrations),
        });
    }

    let recv_bytes = if recv_bytes == 0 {
        expected_bytes
    } else {
        recv_bytes.min(expected_bytes)
    };
    let recv_len = tcp_bench_u32_len(recv_bytes, "recv bytes")?;
    let mut received = vec![0usize; streams.len()];
    let mut complete = vec![false; streams.len()];
    let mut empty_final_progress = vec![usize::MAX; streams.len()];
    let mut complete_count = 0usize;
    let mut consumed_bytes = 0usize;
    let mut skipped_bytes = 0usize;
    let mut frames = 0usize;
    let mut checksum = 0u64;
    let lenstream_max_payload =
        env_usize_or("URING_PLAY_LENSTREAM_MAX_PAYLOAD_BYTES", 64 * 1024 * 1024);
    let mut lenstream_parsers = if consume_mode.is_lenstream() {
        Some(
            (0..streams.len())
                .map(|_| LenStreamParser::new(lenstream_max_payload))
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };

    for (idx, stream) in streams.iter().enumerate() {
        ring.queue_recv_zc(stream.as_raw_fd(), zcrx.zcrx_id, recv_len, idx as u64)?;
    }
    ring.submit_pending()?;

    while complete_count < streams.len() {
        let cqe = ring.wait_cqe()?;
        let idx = cqe.user_data as usize;
        if idx >= streams.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected recv_zc CQE user_data={}", cqe.user_data),
            ));
        }

        if (cqe.flags & IORING_CQE_F_MORE) == 0 {
            if cqe.res < 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "rxq={rxq} conn={idx} final recv_zc error res={} flags=0x{:x}: {}",
                        cqe.res,
                        cqe.flags,
                        io::Error::from_raw_os_error(-cqe.res)
                    ),
                ));
            }
            if cqe.res != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "rxq={rxq} conn={idx} unexpected final recv_zc result {} flags=0x{:x}",
                        cqe.res, cqe.flags
                    ),
                ));
            }
            if received[idx] != expected_bytes {
                if empty_final_progress[idx] == received[idx] {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("{idx} received {}/{} bytes", received[idx], expected_bytes),
                    ));
                }
                empty_final_progress[idx] = received[idx];
                ring.queue_recv_zc(streams[idx].as_raw_fd(), zcrx.zcrx_id, recv_len, idx as u64)?;
                ring.submit_pending()?;
                continue;
            }
            if !complete[idx] {
                complete[idx] = true;
                complete_count += 1;
            }
            continue;
        }

        if cqe.res < 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "rxq={rxq} conn={idx} recv_zc error res={} flags=0x{:x}: {}",
                    cqe.res,
                    cqe.flags,
                    io::Error::from_raw_os_error(-cqe.res)
                ),
            ));
        }
        if complete[idx] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("recv_zc data after final CQE for connection {idx}"),
            ));
        }

        received[idx] += cqe.res as usize;
        if received[idx] > expected_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{idx} received too many bytes: {}/{}",
                    received[idx], expected_bytes
                ),
            ));
        }
        match consume_mode {
            ZcrxConsumeMode::None => {}
            ZcrxConsumeMode::ChecksumScalar | ZcrxConsumeMode::ChecksumAvx2 => {
                let data = zcrx.data_for_cqe(&cqe)?;
                consumed_bytes += data.len();
                checksum = checksum.wrapping_add(checksum_bytes(consume_mode, data));
            }
            ZcrxConsumeMode::LenStreamSkip => {
                let data = zcrx.data_for_cqe(&cqe)?;
                let parser = lenstream_parsers
                    .as_mut()
                    .and_then(|parsers| parsers.get_mut(idx))
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "length stream parser missing for connection",
                        )
                    })?;
                let delta = parser.consume(data)?;
                consumed_bytes += delta.header_bytes;
                skipped_bytes += delta.payload_bytes;
                frames += delta.frames;
                checksum = checksum.wrapping_add(delta.checksum);
            }
        }
        zcrx.return_buffer(&cqe)?;
    }

    if let Some(parsers) = &lenstream_parsers {
        for (idx, parser) in parsers.iter().enumerate() {
            parser.finish(idx)?;
        }
    }

    let bytes = received.into_iter().sum();
    let end_thread_cpu = thread_cpu_time().unwrap_or(start_thread_cpu);
    let end_cpu = current_cpu();
    let end_switches = read_thread_context_switches(tid).unwrap_or(start_switches);

    Ok(ZcrxWorkerStats {
        rxq,
        streams: stream_count,
        bytes,
        consumed_bytes,
        skipped_bytes,
        frames,
        checksum,
        wall: start_wall.elapsed(),
        cpu: end_thread_cpu.saturating_sub(start_thread_cpu),
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
        start_cpu,
        end_cpu,
        voluntary_switches: end_switches
            .voluntary
            .saturating_sub(start_switches.voluntary),
        involuntary_switches: end_switches
            .involuntary
            .saturating_sub(start_switches.involuntary),
        migrations: end_switches
            .migrations
            .saturating_sub(start_switches.migrations),
    })
}

#[derive(Clone, Copy)]
struct UringSendOp {
    conn: usize,
    len: usize,
    zc: bool,
}

struct UringSendSlot {
    op: Option<UringSendOp>,
    conn: Option<usize>,
    zc_notif_expected: bool,
    zc_notif_done: bool,
}

struct UringSendConn {
    fd: i32,
    fixed_file: bool,
    scheduled: usize,
    completed: usize,
    in_flight: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UringSendMode {
    Send,
    SendZc,
    SendZcFixed,
}

impl UringSendMode {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "send" => Ok(Self::Send),
            "send-zc" | "zc" => Ok(Self::SendZc),
            "send-zc-fixed" | "zc-fixed" | "fixed-zc" => Ok(Self::SendZcFixed),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown uring send mode {other:?}; use send, send-zc, or send-zc-fixed"),
            )),
        }
    }

    fn uses_zc(self) -> bool {
        matches!(self, Self::SendZc | Self::SendZcFixed)
    }

    fn uses_fixed_buffer(self) -> bool {
        matches!(self, Self::SendZcFixed)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::SendZc => "send-zc",
            Self::SendZcFixed => "send-zc-fixed",
        }
    }
}

fn uring_send_fixed_file_enabled() -> bool {
    env_enabled_or("URING_PLAY_TCP_SEND_FIXED_FILE", false)
}

#[derive(Clone, Copy)]
enum SendPayloadPattern {
    Fill {
        byte: u8,
    },
    LenStream {
        payload_len: usize,
        payload_byte: u8,
    },
}

impl SendPayloadPattern {
    fn from_env(chunk_len: usize) -> io::Result<Self> {
        let pattern = env::var("URING_PLAY_SEND_PATTERN")
            .unwrap_or_else(|_| "fill".to_string())
            .to_ascii_lowercase();
        match pattern.as_str() {
            "" | "fill" | "byte" => Ok(Self::Fill {
                byte: env_u8_or("URING_PLAY_SEND_FILL_BYTE", 0)?,
            }),
            "lenstream" | "length-stream" | "len32" => {
                let default_payload_len =
                    chunk_len.checked_sub(LENSTREAM_HEADER_LEN).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "length stream chunk must be at least 4 bytes",
                        )
                    })?;
                let payload_len =
                    env_usize_or("URING_PLAY_LENSTREAM_PAYLOAD_BYTES", default_payload_len);
                if payload_len > u32::MAX as usize {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("length stream payload too large for u32 header: {payload_len}"),
                    ));
                }
                Ok(Self::LenStream {
                    payload_len,
                    payload_byte: env_u8_or("URING_PLAY_LENSTREAM_PAYLOAD_BYTE", 0x5a)?,
                })
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown URING_PLAY_SEND_PATTERN={other:?}; use fill or lenstream"),
            )),
        }
    }

    fn label(self) -> String {
        match self {
            Self::Fill { byte } => format!("fill:0x{byte:02x}"),
            Self::LenStream {
                payload_len,
                payload_byte,
            } => format!("lenstream:payload={payload_len}:byte=0x{payload_byte:02x}"),
        }
    }

    fn validate(self, bytes_per_connection: usize, chunk_len: usize) -> io::Result<()> {
        let Self::LenStream { payload_len, .. } = self else {
            return Ok(());
        };
        let record_len = LENSTREAM_HEADER_LEN
            .checked_add(payload_len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "record length overflow"))?;
        if record_len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "length stream record length must be nonzero",
            ));
        }
        if chunk_len % record_len != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "length stream chunk length {chunk_len} must be a multiple of record length {record_len}"
                ),
            ));
        }
        if bytes_per_connection % record_len != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "length stream bytes_per_connection {bytes_per_connection} must be a multiple of record length {record_len}"
                ),
            ));
        }
        Ok(())
    }

    fn fill(self, buf: &mut [u8]) {
        match self {
            Self::Fill { byte } => buf.fill(byte),
            Self::LenStream {
                payload_len,
                payload_byte,
            } => {
                let record_len = LENSTREAM_HEADER_LEN + payload_len;
                let header = (payload_len as u32).to_le_bytes();
                for record in buf.chunks_exact_mut(record_len) {
                    record[..LENSTREAM_HEADER_LEN].copy_from_slice(&header);
                    record[LENSTREAM_HEADER_LEN..].fill(payload_byte);
                }
            }
        }
    }
}

#[derive(Default)]
struct UringSendStats {
    worker: usize,
    streams: usize,
    bytes: usize,
    zc_notifications: usize,
    zc_copied_notifications: usize,
    wall: Duration,
    cpu: Duration,
    target_cpu: i32,
    affinity_applied: bool,
    start_cpu: i32,
    end_cpu: i32,
    voluntary_switches: u64,
    involuntary_switches: u64,
    migrations: u64,
}

const TCP_WAL_RECV_USER_DATA: u64 = 1u64 << 63;
const TCP_WAL_WRITE_USER_DATA: u64 = 1u64 << 62;
const TCP_WAL_SLOT_MASK: u64 = TCP_WAL_WRITE_USER_DATA - 1;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TcpWalCqeKind {
    Recv,
    Write,
}

fn tcp_wal_recv_user_data(slot: usize) -> u64 {
    TCP_WAL_RECV_USER_DATA | slot as u64
}

fn tcp_wal_write_user_data(slot: usize) -> u64 {
    TCP_WAL_WRITE_USER_DATA | slot as u64
}

fn tcp_wal_decode_user_data(user_data: u64) -> io::Result<(TcpWalCqeKind, usize)> {
    let slot = (user_data & TCP_WAL_SLOT_MASK) as usize;
    if (user_data & TCP_WAL_RECV_USER_DATA) != 0 {
        Ok((TcpWalCqeKind::Recv, slot))
    } else if (user_data & TCP_WAL_WRITE_USER_DATA) != 0 {
        Ok((TcpWalCqeKind::Write, slot))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected TCP WAL CQE user_data={user_data}"),
        ))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TcpWalSlotStage {
    Free,
    Recv,
    Write,
}

struct TcpWalSlot {
    stage: TcpWalSlotStage,
    conn: usize,
    filled: usize,
    file_offset: u64,
}

impl Default for TcpWalSlot {
    fn default() -> Self {
        Self {
            stage: TcpWalSlotStage::Free,
            conn: 0,
            filled: 0,
            file_offset: 0,
        }
    }
}

struct TcpWalConn {
    fd: i32,
    fixed_file: bool,
    received: usize,
    recv_in_flight: bool,
}

#[derive(Clone, Copy)]
struct TcpWalWorkerResult {
    worker: usize,
    streams: usize,
    received_bytes: usize,
    written_bytes: usize,
    chunks: usize,
    elapsed: Duration,
    target_cpu: i32,
    affinity_applied: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TcpWalPipelineMode {
    SameCore,
    Split,
    Zcrx,
}

impl TcpWalPipelineMode {
    fn from_env() -> io::Result<Self> {
        match env::var("URING_PLAY_TCP_WAL_MODE")
            .unwrap_or_else(|_| "same-core".to_string())
            .as_str()
        {
            "" | "same-core" | "same" | "coupled" => Ok(Self::SameCore),
            "split" | "split-rx-wal" | "decoupled" => Ok(Self::Split),
            "zcrx" | "recv-zc" | "zero-copy-rx" => Ok(Self::Zcrx),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown URING_PLAY_TCP_WAL_MODE={other:?}; use same-core, split, or zcrx"),
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::SameCore => "same-core",
            Self::Split => "split",
            Self::Zcrx => "zcrx",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TcpWalWriteMode {
    Null,
    Slot,
    Write,
    WriteFixed,
    WriteFixedFile,
}

impl TcpWalWriteMode {
    fn from_env() -> io::Result<Self> {
        match env::var("URING_PLAY_TCP_WAL_WRITE_MODE")
            .unwrap_or_else(|_| "fixed-file".to_string())
            .as_str()
        {
            "" | "fixed-file" | "fixedfile" | "fixedbufs-registerfiles" | "fast" => {
                Ok(Self::WriteFixedFile)
            }
            "null" | "sink" | "discard" | "none" => Ok(Self::Null),
            "fixed" | "write-fixed" | "fixedbuf" | "fixedbufs" => Ok(Self::WriteFixed),
            "write" | "plain" | "normal" => Ok(Self::Write),
            "slot" | "slots" | "io-slot" | "io-slots" => Ok(Self::Slot),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown URING_PLAY_TCP_WAL_WRITE_MODE={other:?}; use fixed-file, fixed, write, slot, or null"
                ),
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Slot => "slot",
            Self::Write => "write",
            Self::WriteFixed => "fixed",
            Self::WriteFixedFile => "fixed-file",
        }
    }

    fn needs_registered_buffers(self) -> bool {
        matches!(self, Self::Slot | Self::WriteFixed | Self::WriteFixedFile)
    }

    fn needs_registered_files(self) -> bool {
        matches!(self, Self::Slot | Self::WriteFixedFile)
    }

    fn needs_io_slots(self) -> bool {
        matches!(self, Self::Slot)
    }

    fn is_null(self) -> bool {
        matches!(self, Self::Null)
    }
}

fn tcp_wal_fixed_recv_enabled() -> bool {
    env_enabled_or("URING_PLAY_TCP_WAL_FIXED_RECV", true)
}

fn fixed_file_index_i32(index: usize, label: &str) -> io::Result<i32> {
    i32::try_from(index).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} fixed-file index {index} does not fit in i32"),
        )
    })
}

#[derive(Clone, Copy)]
struct TcpWalWriteDesc {
    rx_worker: usize,
    buffer_index: usize,
    file_offset: u64,
    len: u32,
}

struct TcpWalSplitRxResult {
    worker: usize,
    streams: usize,
    received_bytes: usize,
    submitted_bytes: usize,
    chunks: usize,
    elapsed: Duration,
    target_cpu: i32,
    affinity_applied: bool,
}

struct TcpWalSplitWriterResult {
    writer: usize,
    written_bytes: usize,
    chunks: usize,
    elapsed: Duration,
    target_cpu: i32,
    affinity_applied: bool,
}

const TCP_WAL_ZCRX_WRITE_USER_DATA: u64 = 1u64 << 61;
const TCP_WAL_ZCRX_WRITE_SLOT_MASK: u64 = TCP_WAL_ZCRX_WRITE_USER_DATA - 1;
const TCP_WAL_ZCRX_TRANSIENT_RECV_RETRY_LIMIT: usize = 32768;

#[derive(Clone, Copy)]
struct TcpWalZcrxWriteSlot {
    frame_slot: usize,
    conn: usize,
    len: u32,
    logical_len: u32,
    logical_chunks: usize,
    file_offset: u64,
    zcrx_slot: Option<usize>,
}

#[derive(Clone, Copy)]
struct TcpWalZcrxFrameSlot {
    cqe: Option<IoUringCqe32>,
    conn: usize,
    pending_writes: usize,
}

struct TcpWalZcrxWorkerAssignment {
    streams: Vec<TcpWalZcrxWorkerStream>,
    wal_region_base: u64,
    wal_region_end: u64,
}

#[derive(Default)]
struct TcpWalZcrxQueuedWrites {
    consumed: bool,
    writes: usize,
    direct_frames: usize,
    direct_bytes: usize,
    bounce_frames: usize,
    bounce_bytes: usize,
    coalesce_frames: usize,
    coalesce_bytes: usize,
}

#[derive(Clone, Copy, Default)]
struct TcpWalZcrxCoalesceConn {
    write_slot: Option<usize>,
    filled: usize,
}

#[derive(Default)]
struct TcpWalZcrxWorkerCounters {
    direct_enabled: bool,
    recv_multishot: bool,
    max_active_recvs: usize,
    recv_flush_calls: usize,
    recv_flush_queued: usize,
    recv_data_cqes: usize,
    recv_final_cqes: usize,
    recv_empty_finals: usize,
    transient_recv_errors: usize,
    transient_recv_deferred: usize,
    transient_recv_backoffs: usize,
    pending_frame_requeues: usize,
    pending_frame_max: usize,
    direct_queue_attempts: usize,
    direct_stall_write_slots: usize,
    direct_stall_frame_slots: usize,
    direct_stall_busy_slots: usize,
    direct_stall_sq_space: usize,
    direct_padded_frames: usize,
    direct_padded_bytes: usize,
    direct_multisegment_frames: usize,
    direct_segment_writes: usize,
    direct_fallback_to_bounce: usize,
    direct_slot_busy_guard: bool,
    direct_busy_bounce: bool,
    direct_busy_bounce_attempts: usize,
    direct_stall_coalesce: bool,
    direct_stall_coalesce_attempts: usize,
    bounce_queue_attempts: usize,
    bounce_stall_write_slots: usize,
    bounce_stall_frame_slots: usize,
    bounce_stall_sq_space: usize,
    coalesce_appends: usize,
    coalesce_completed_chunks: usize,
    coalesce_fixed_writes: usize,
    coalesce_flushes: usize,
    coalesce_nt_copy: bool,
    coalesce_nt_copy_calls: usize,
    coalesce_nt_copy_bytes: usize,
}

#[derive(Clone, Copy, Default)]
struct TcpWalZcrxTimingCounters {
    enabled: bool,
    queue_write_calls: u64,
    queue_write_ns: u128,
    coalesce_flush_calls: u64,
    coalesce_flush_ns: u128,
    wait_cqe_calls: u64,
    wait_cqe_ns: u128,
    complete_cqe_calls: u64,
    complete_cqe_ns: u128,
}

impl TcpWalZcrxTimingCounters {
    fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    fn record(slot: &mut u128, calls: &mut u64, started: Option<Instant>) {
        if let Some(started) = started {
            *slot = slot.saturating_add(started.elapsed().as_nanos());
            *calls = calls.saturating_add(1);
        }
    }

    fn record_queue_write(&mut self, started: Option<Instant>) {
        Self::record(
            &mut self.queue_write_ns,
            &mut self.queue_write_calls,
            started,
        );
    }

    fn record_coalesce_flush(&mut self, started: Option<Instant>) {
        Self::record(
            &mut self.coalesce_flush_ns,
            &mut self.coalesce_flush_calls,
            started,
        );
    }

    fn record_wait_cqe(&mut self, started: Option<Instant>) {
        Self::record(&mut self.wait_cqe_ns, &mut self.wait_cqe_calls, started);
    }

    fn record_complete_cqe(&mut self, started: Option<Instant>) {
        Self::record(
            &mut self.complete_cqe_ns,
            &mut self.complete_cqe_calls,
            started,
        );
    }
}

#[derive(Clone, Default)]
struct TcpWalZcrxConnCounters {
    received_bytes: usize,
    recv_data_cqes: usize,
    full_chunk_cqes: usize,
    partial_cqes: usize,
    min_cqe_len: usize,
    max_cqe_len: usize,
    direct_frames: usize,
    direct_bytes: usize,
    coalesce_frames: usize,
    coalesce_bytes: usize,
    pending_frame_requeues: usize,
}

struct TcpWalZcrxConnResult {
    meta: TcpBenchStreamMeta,
    counters: TcpWalZcrxConnCounters,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TcpWalZcrxDirectRange {
    aligned_offset: usize,
    physical_len: usize,
    first_buf_index: usize,
    segments: usize,
}

struct TcpWalZcrxWorkerResult {
    worker: usize,
    rxq: u32,
    planned_nvme_queue: usize,
    streams: usize,
    wal_region_base: u64,
    wal_region_end: u64,
    slot_stride: usize,
    fixed_buffers: usize,
    zcrx_area_base: usize,
    zcrx_area_bytes: usize,
    zcrx_area_alignment: usize,
    zcrx_rx_buf_len: u32,
    zcrx_area_memory_policy: &'static str,
    write_pipeline: usize,
    frame_pipeline: usize,
    coalesce_batch_chunks: usize,
    coalesce_write_bytes: usize,
    received_bytes: usize,
    written_bytes: usize,
    wal_bytes: usize,
    direct_frames: usize,
    direct_bytes: usize,
    bounce_frames: usize,
    bounce_bytes: usize,
    coalesce_frames: usize,
    coalesce_bytes: usize,
    frames: usize,
    chunks: usize,
    elapsed: Duration,
    cpu: Duration,
    target_cpu: i32,
    affinity_applied: bool,
    start_cpu: i32,
    end_cpu: i32,
    voluntary_switches: u64,
    involuntary_switches: u64,
    migrations: u64,
    worker_numa_node: Option<i32>,
    counters: TcpWalZcrxWorkerCounters,
    timing: TcpWalZcrxTimingCounters,
    conn_results: Vec<TcpWalZcrxConnResult>,
}

fn tcp_wal_zcrx_write_user_data(slot: usize) -> u64 {
    TCP_WAL_ZCRX_WRITE_USER_DATA | slot as u64
}

fn tcp_wal_zcrx_write_slot(user_data: u64) -> Option<usize> {
    ((user_data & TCP_WAL_ZCRX_WRITE_USER_DATA) != 0)
        .then_some((user_data & TCP_WAL_ZCRX_WRITE_SLOT_MASK) as usize)
}

fn tcp_wal_zcrx_record_conn_recv(
    conn_counters: &mut [TcpWalZcrxConnCounters],
    conn: usize,
    len: usize,
    chunk_bytes: usize,
) -> io::Result<()> {
    let counters = conn_counters.get_mut(conn).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ZCRX WAL missing per-connection counters for conn={conn}"),
        )
    })?;
    if counters.recv_data_cqes == 0 || len < counters.min_cqe_len {
        counters.min_cqe_len = len;
    }
    counters.max_cqe_len = counters.max_cqe_len.max(len);
    counters.recv_data_cqes = counters.recv_data_cqes.saturating_add(1);
    counters.received_bytes = counters.received_bytes.saturating_add(len);
    if len == chunk_bytes {
        counters.full_chunk_cqes = counters.full_chunk_cqes.saturating_add(1);
    } else {
        counters.partial_cqes = counters.partial_cqes.saturating_add(1);
    }
    Ok(())
}

fn tcp_wal_zcrx_record_conn_queued(
    conn_counters: &mut [TcpWalZcrxConnCounters],
    conn: usize,
    queued_writes: &TcpWalZcrxQueuedWrites,
) -> io::Result<()> {
    let counters = conn_counters.get_mut(conn).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ZCRX WAL missing per-connection counters for conn={conn}"),
        )
    })?;
    counters.direct_frames = counters
        .direct_frames
        .saturating_add(queued_writes.direct_frames);
    counters.direct_bytes = counters
        .direct_bytes
        .saturating_add(queued_writes.direct_bytes);
    counters.coalesce_frames = counters
        .coalesce_frames
        .saturating_add(queued_writes.coalesce_frames);
    counters.coalesce_bytes = counters
        .coalesce_bytes
        .saturating_add(queued_writes.coalesce_bytes);
    Ok(())
}

fn tcp_wal_zcrx_record_conn_requeue(
    conn_counters: &mut [TcpWalZcrxConnCounters],
    conn: usize,
) -> io::Result<()> {
    let counters = conn_counters.get_mut(conn).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ZCRX WAL missing per-connection counters for conn={conn}"),
        )
    })?;
    counters.pending_frame_requeues = counters.pending_frame_requeues.saturating_add(1);
    Ok(())
}

fn tcp_wal_zcrx_validate_slot_stride(
    slot_stride: usize,
    required_alignment: usize,
) -> io::Result<()> {
    if slot_stride == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "URING_PLAY_ZCRX_SLOT_STRIDE must be non-zero",
        ));
    }
    if slot_stride < required_alignment || slot_stride % required_alignment != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "URING_PLAY_ZCRX_SLOT_STRIDE={slot_stride} must be a multiple of direct-I/O alignment {required_alignment}"
            ),
        ));
    }
    if slot_stride > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "URING_PLAY_ZCRX_SLOT_STRIDE must fit in u32-sized slot write lengths",
        ));
    }
    Ok(())
}

fn tcp_wal_zcrx_slot_stride_from_env(required_alignment: usize) -> io::Result<(usize, bool)> {
    let min_stride = page_size()?.max(required_alignment);
    match env_size_opt("URING_PLAY_ZCRX_SLOT_STRIDE")? {
        Some(slot_stride) => {
            tcp_wal_zcrx_validate_slot_stride(slot_stride, required_alignment)?;
            Ok((slot_stride, true))
        }
        None => Ok((min_stride, false)),
    }
}

fn tcp_wal_zcrx_initial_slot_stride(
    requested_slot_stride: usize,
    configured: bool,
    zcrx_rx_buf_len: u32,
    required_alignment: usize,
) -> usize {
    if configured {
        requested_slot_stride
    } else {
        requested_slot_stride.max(align_up(zcrx_rx_buf_len as usize, required_alignment))
    }
}

fn tcp_wal_zcrx_auto_slot_stride(
    area_size: usize,
    mut slot_stride: usize,
    required_alignment: usize,
    configured: bool,
    reserved_fixed_buffers: usize,
) -> io::Result<usize> {
    tcp_wal_zcrx_validate_slot_stride(slot_stride, required_alignment)?;
    if reserved_fixed_buffers >= IORING_MAX_REG_BUFFERS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "ZCRX WAL reserved {reserved_fixed_buffers} fixed buffers, leaving no room \
                 under io_uring registered-buffer limit {IORING_MAX_REG_BUFFERS}"
            ),
        ));
    }
    if configured {
        if area_size % slot_stride != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "URING_PLAY_ZCRX_SLOT_STRIDE={slot_stride} does not divide ZCRX area size {area_size}"
                ),
            ));
        }
        let fixed_buffers = area_size / slot_stride;
        if fixed_buffers + reserved_fixed_buffers > IORING_MAX_REG_BUFFERS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "URING_PLAY_ZCRX_SLOT_STRIDE={slot_stride} maps the ZCRX area to \
                     {fixed_buffers} fixed buffers plus {reserved_fixed_buffers} reserved \
                     buffers, exceeding io_uring limit {IORING_MAX_REG_BUFFERS}"
                ),
            ));
        }
        return Ok(slot_stride);
    }

    while area_size % slot_stride != 0
        || area_size / slot_stride + reserved_fixed_buffers > IORING_MAX_REG_BUFFERS
        || area_size / slot_stride > u16::MAX as usize
    {
        slot_stride = slot_stride.checked_mul(2).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "auto ZCRX slot stride overflow",
            )
        })?;
        tcp_wal_zcrx_validate_slot_stride(slot_stride, required_alignment)?;
    }
    Ok(slot_stride)
}

fn make_observed_wal_regions(
    stream_counts: &[usize],
    bytes_per_connection: usize,
    wal_region_base: u64,
    wal_region_end: u64,
) -> io::Result<Vec<WalRegionPlan>> {
    if wal_region_end < wal_region_base {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("WAL region end {wal_region_end} precedes base {wal_region_base}"),
        ));
    }

    let mut next_offset = wal_region_base;
    let mut regions = Vec::with_capacity(stream_counts.len());
    for (worker, stream_count) in stream_counts.iter().copied().enumerate() {
        let len_bytes = stream_count
            .checked_mul(bytes_per_connection)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("ZCRX WAL worker {worker} region length overflow"),
                )
            })?;
        let end = next_offset.checked_add(len_bytes as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("ZCRX WAL worker {worker} region end overflow"),
            )
        })?;
        if end > wal_region_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "ZCRX WAL worker {worker} region [{next_offset}..{end}) exceeds \
                     configured WAL region end {wal_region_end}"
                ),
            ));
        }
        regions.push(WalRegionPlan {
            worker,
            base_offset: next_offset,
            len_bytes,
        });
        next_offset = end;
    }
    Ok(regions)
}

fn tcp_wal_zcrx_direct_segment_range(
    offset: usize,
    len: usize,
    required_alignment: usize,
    slot_stride: usize,
    fixed_buffer_count: usize,
) -> io::Result<Option<TcpWalZcrxDirectRange>> {
    if slot_stride < required_alignment || slot_stride % required_alignment != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "ZCRX WAL slot_stride={slot_stride} is not compatible with alignment {required_alignment}"
            ),
        ));
    }

    let logical_end = offset
        .checked_add(len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ZCRX WAL frame end overflow"))?;
    let alignment_prefix = offset % required_alignment;
    let aligned_offset = offset - alignment_prefix;
    let physical_len = align_up(
        logical_end.checked_sub(aligned_offset).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "ZCRX WAL aligned range underflow",
            )
        })?,
        required_alignment,
    );
    let physical_end = aligned_offset.checked_add(physical_len).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "ZCRX WAL physical range overflow",
        )
    })?;
    if physical_end > fixed_buffer_count.saturating_mul(slot_stride) {
        return Ok(None);
    }

    let first_buf_index = aligned_offset / slot_stride;
    let end_buf_index = physical_end.checked_add(slot_stride - 1).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "ZCRX WAL buffer index overflow")
    })? / slot_stride;
    let segments = end_buf_index
        .checked_sub(first_buf_index)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ZCRX WAL segment underflow"))?;
    if segments == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ZCRX WAL frame has zero aligned segments",
        ));
    }
    if end_buf_index > fixed_buffer_count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "ZCRX WAL frame range offset={aligned_offset} len={physical_len} maps to fixed buffers \
                 [{first_buf_index}..{end_buf_index}), \
                 but only {fixed_buffer_count} buffers are registered"
            ),
        ));
    }

    Ok(Some(TcpWalZcrxDirectRange {
        aligned_offset,
        physical_len,
        first_buf_index,
        segments,
    }))
}

fn tcp_wal_zcrx_direct_full_chunk_segment_range(
    offset: usize,
    len: usize,
    chunk_bytes: usize,
    required_alignment: usize,
    slot_stride: usize,
    fixed_buffer_count: usize,
) -> io::Result<Option<TcpWalZcrxDirectRange>> {
    if len != chunk_bytes || len % required_alignment != 0 || offset % required_alignment != 0 {
        return Ok(None);
    }
    let range = tcp_wal_zcrx_direct_segment_range(
        offset,
        len,
        required_alignment,
        slot_stride,
        fixed_buffer_count,
    )?;
    Ok(range.filter(|range| range.physical_len == len))
}

fn tcp_wal_zcrx_direct_slots_busy(
    first_buf_index: usize,
    segments: usize,
    busy_slots: &[bool],
) -> io::Result<bool> {
    let end = first_buf_index.checked_add(segments).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "ZCRX WAL direct segment index overflow",
        )
    })?;
    let busy_range = busy_slots.get(first_buf_index..end).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "ZCRX WAL direct segment range [{first_buf_index}..{end}) exceeds \
                 {} tracked ZCRX slots",
                busy_slots.len()
            ),
        )
    })?;
    Ok(busy_range.iter().any(|busy| *busy))
}

fn tcp_wal_zcrx_is_transient_recv_error(res: i32) -> bool {
    matches!(-res, libc::ENOMEM | libc::ENOBUFS | libc::EAGAIN)
}

fn tcp_wal_zcrx_transient_recv_backoff(retry: usize) {
    if retry <= 16 {
        thread::yield_now();
        return;
    }
    let sleep_us = retry.saturating_sub(16).min(200) as u64;
    thread::sleep(Duration::from_micros(sleep_us));
}

fn tcp_wal_zcrx_direct_fits_without_recycling(
    stream_count: usize,
    bytes_per_connection: usize,
    zcrx_area_size: usize,
    recv_len: u32,
    write_pipeline: usize,
) -> io::Result<bool> {
    let expected_worker_bytes =
        stream_count
            .checked_mul(bytes_per_connection)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ZCRX WAL expected worker byte count overflow",
                )
            })?;
    let recycle_headroom = (recv_len as usize)
        .saturating_mul(write_pipeline.max(stream_count).max(1))
        .saturating_mul(2);
    Ok(expected_worker_bytes.saturating_add(recycle_headroom) < zcrx_area_size)
}

fn tcp_wal_zcrx_worker_area_size(
    stream_count: usize,
    bytes_per_connection: usize,
    recv_len: u32,
    write_pipeline: usize,
) -> io::Result<usize> {
    let configured = zcrx_env_area_size();
    if !env_enabled_or("URING_PLAY_TCP_WAL_ZCRX_AUTO_AREA", true) {
        return Ok(configured);
    }
    let expected_worker_bytes =
        stream_count
            .checked_mul(bytes_per_connection)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ZCRX WAL expected worker byte count overflow",
                )
            })?;
    let recycle_headroom = (recv_len as usize)
        .saturating_mul(write_pipeline.max(stream_count).max(1))
        .saturating_mul(4);
    Ok(configured.max(align_up(
        expected_worker_bytes.saturating_add(recycle_headroom),
        1024 * 1024,
    )))
}

fn tcp_wal_zcrx_queue_recv_for_conn(
    ring: &mut RawRing,
    conns: &mut [TcpWalConn],
    zcrx_id: u32,
    recv_len: u32,
    conn_idx: usize,
    recv_multishot: bool,
) -> io::Result<()> {
    if conns[conn_idx].recv_in_flight {
        return Ok(());
    }
    ring.queue_recv_zc_with_mode(
        conns[conn_idx].fd,
        zcrx_id,
        recv_len,
        conn_idx as u64,
        recv_multishot,
    )?;
    conns[conn_idx].recv_in_flight = true;
    Ok(())
}

fn tcp_wal_zcrx_flush_deferred_recvs(
    ring: &mut RawRing,
    conns: &mut [TcpWalConn],
    complete: &[bool],
    deferred_recvs: &mut [bool],
    zcrx_id: u32,
    recv_len: u32,
    max_active_recvs: usize,
    recv_multishot: bool,
    counters: &mut TcpWalZcrxWorkerCounters,
) -> io::Result<()> {
    counters.recv_flush_calls = counters.recv_flush_calls.saturating_add(1);
    let max_active_recvs = max_active_recvs.max(1);
    let mut active_recvs = conns
        .iter()
        .enumerate()
        .filter(|(idx, conn)| !complete[*idx] && conn.recv_in_flight)
        .count();
    let mut queued = false;
    for conn_idx in 0..deferred_recvs.len() {
        if !deferred_recvs[conn_idx] || complete[conn_idx] {
            continue;
        }
        if active_recvs >= max_active_recvs {
            break;
        }
        if ring.sq_available() == 0 {
            ring.submit_pending()?;
        }
        tcp_wal_zcrx_queue_recv_for_conn(ring, conns, zcrx_id, recv_len, conn_idx, recv_multishot)?;
        deferred_recvs[conn_idx] = false;
        counters.recv_flush_queued = counters.recv_flush_queued.saturating_add(1);
        active_recvs += 1;
        queued = true;
    }
    if queued {
        ring.submit_pending()?;
    }
    Ok(())
}

fn tcp_wal_zcrx_allocate_wal(
    next_wal_offset: &mut u64,
    len: usize,
    wal_region_end: u64,
    required_alignment: usize,
    label: &str,
) -> io::Result<u64> {
    let file_offset = *next_wal_offset;
    if file_offset % required_alignment as u64 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("ZCRX WAL file offset {file_offset} is not aligned to {required_alignment}"),
        ));
    }
    let write_end = file_offset
        .checked_add(len as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ZCRX WAL offset overflow"))?;
    if write_end > wal_region_end {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "ZCRX WAL {label} write [{file_offset}..{write_end}) exceeds configured \
                 WAL region end {wal_region_end}"
            ),
        ));
    }
    *next_wal_offset = write_end;
    Ok(file_offset)
}

#[allow(clippy::too_many_arguments)]
fn tcp_wal_zcrx_queue_coalesced_chunk(
    ring: &mut RawRing,
    fd: i32,
    bounce_buffers: &FixedSendBuffers,
    inflight: &mut [Option<TcpWalZcrxWriteSlot>],
    frame_slots: &mut [Option<TcpWalZcrxFrameSlot>],
    free_frame_slots: &mut Vec<usize>,
    next_wal_offset: &mut u64,
    wal_region_end: u64,
    required_alignment: usize,
    conn: usize,
    write_slot: usize,
    bounce_fixed_base: usize,
    coalesce_fixed: bool,
    write_bytes: usize,
    chunk_bytes: usize,
    counters: &mut TcpWalZcrxWorkerCounters,
) -> io::Result<TcpWalZcrxQueuedWrites> {
    counters.bounce_queue_attempts = counters.bounce_queue_attempts.saturating_add(1);
    if free_frame_slots.is_empty() {
        counters.bounce_stall_frame_slots = counters.bounce_stall_frame_slots.saturating_add(1);
        return Ok(TcpWalZcrxQueuedWrites::default());
    }
    if ring.sq_available() == 0 {
        ring.submit_pending()?;
        if ring.sq_available() == 0 {
            counters.bounce_stall_sq_space = counters.bounce_stall_sq_space.saturating_add(1);
            return Ok(TcpWalZcrxQueuedWrites::default());
        }
    }
    if write_bytes == 0
        || chunk_bytes == 0
        || write_bytes % required_alignment != 0
        || write_bytes > bounce_buffers.stride()
        || write_bytes % chunk_bytes != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "ZCRX WAL coalesced write_bytes={write_bytes} chunk_bytes={chunk_bytes} is not compatible with \
                 alignment {required_alignment} and bounce stride {}",
                bounce_buffers.stride()
            ),
        ));
    }
    let write_len_u32 = tcp_bench_u32_len(write_bytes, "ZCRX WAL coalesced write length")?;
    let logical_chunks = write_bytes / chunk_bytes;
    let frame_slot = free_frame_slots
        .pop()
        .expect("free_frame_slots was checked above");
    let dst = bounce_buffers.ptr(write_slot);

    let file_offset = tcp_wal_zcrx_allocate_wal(
        next_wal_offset,
        write_bytes,
        wal_region_end,
        required_alignment,
        "coalesced",
    )?;

    frame_slots[frame_slot] = Some(TcpWalZcrxFrameSlot {
        cqe: None,
        conn,
        pending_writes: 1,
    });
    if coalesce_fixed {
        let fixed_index = bounce_fixed_base.checked_add(write_slot).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "ZCRX WAL coalesced fixed buffer index overflow",
            )
        })?;
        let fixed_index = u16::try_from(fixed_index).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("ZCRX WAL coalesced fixed buffer index {fixed_index} exceeds u16"),
            )
        })?;
        ring.queue_write_fixed(
            fd,
            dst.cast_const(),
            write_len_u32,
            file_offset,
            fixed_index,
            tcp_wal_zcrx_write_user_data(write_slot),
        )?;
        counters.coalesce_fixed_writes = counters.coalesce_fixed_writes.saturating_add(1);
    } else {
        ring.queue_write(
            fd,
            dst.cast_const(),
            write_len_u32,
            file_offset,
            tcp_wal_zcrx_write_user_data(write_slot),
        )?;
    }
    inflight[write_slot] = Some(TcpWalZcrxWriteSlot {
        frame_slot,
        conn,
        len: write_len_u32,
        logical_len: write_len_u32,
        logical_chunks,
        file_offset,
        zcrx_slot: None,
    });
    counters.coalesce_completed_chunks = counters
        .coalesce_completed_chunks
        .saturating_add(logical_chunks);

    Ok(TcpWalZcrxQueuedWrites {
        consumed: true,
        writes: 1,
        direct_frames: 0,
        direct_bytes: 0,
        bounce_frames: 0,
        bounce_bytes: 0,
        coalesce_frames: 1,
        coalesce_bytes: write_bytes,
    })
}

fn tcp_wal_zcrx_copy_coalesced_bytes(
    dst: *mut u8,
    src: *const u8,
    len: usize,
    nt_copy: bool,
    nt_min_bytes: usize,
    counters: &mut TcpWalZcrxWorkerCounters,
) {
    if nt_copy && len >= nt_min_bytes {
        copy_non_temporal(src, dst, len);
        counters.coalesce_nt_copy_calls = counters.coalesce_nt_copy_calls.saturating_add(1);
        counters.coalesce_nt_copy_bytes = counters.coalesce_nt_copy_bytes.saturating_add(len);
    } else {
        unsafe { ptr::copy_nonoverlapping(src, dst, len) };
    }
}

#[allow(clippy::too_many_arguments)]
fn tcp_wal_zcrx_queue_coalesced_frame(
    ring: &mut RawRing,
    fd: i32,
    zcrx: &mut ZcrxContext,
    bounce_buffers: &FixedSendBuffers,
    bounce_stride: usize,
    inflight: &mut [Option<TcpWalZcrxWriteSlot>],
    free_write_slots: &mut Vec<usize>,
    frame_slots: &mut [Option<TcpWalZcrxFrameSlot>],
    free_frame_slots: &mut Vec<usize>,
    next_wal_offset: &mut u64,
    wal_region_end: u64,
    required_alignment: usize,
    chunk_bytes: usize,
    coalesce_conns: &mut [TcpWalZcrxCoalesceConn],
    bounce_fixed_base: usize,
    coalesce_fixed: bool,
    coalesce_write_bytes: usize,
    coalesce_nt_copy: bool,
    coalesce_nt_min_bytes: usize,
    conn: usize,
    cqe: IoUringCqe32,
    counters: &mut TcpWalZcrxWorkerCounters,
) -> io::Result<TcpWalZcrxQueuedWrites> {
    let coalesce_conn = coalesce_conns.get_mut(conn).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ZCRX WAL missing coalescer for connection {conn}"),
        )
    })?;
    let len = cqe.res as usize;
    if chunk_bytes == 0
        || coalesce_write_bytes == 0
        || coalesce_write_bytes % chunk_bytes != 0
        || coalesce_write_bytes > bounce_stride
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "ZCRX WAL coalesce write_bytes={coalesce_write_bytes} chunk_bytes={chunk_bytes} \
                 is incompatible with bounce stride {bounce_stride}"
            ),
        ));
    }
    if coalesce_conn.write_slot.is_none() {
        let Some(write_slot) = free_write_slots.pop() else {
            counters.bounce_stall_write_slots = counters.bounce_stall_write_slots.saturating_add(1);
            return Ok(TcpWalZcrxQueuedWrites::default());
        };
        coalesce_conn.write_slot = Some(write_slot);
        coalesce_conn.filled = 0;
    }
    let write_slot = coalesce_conn
        .write_slot
        .expect("coalesce write slot was assigned above");
    let needed = coalesce_write_bytes
        .checked_sub(coalesce_conn.filled)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "ZCRX WAL coalesce fill exceeds batch size",
            )
        })?;
    if len >= needed {
        if free_frame_slots.is_empty() {
            counters.bounce_stall_frame_slots = counters.bounce_stall_frame_slots.saturating_add(1);
            return Ok(TcpWalZcrxQueuedWrites::default());
        }
        if ring.sq_available() == 0 {
            ring.submit_pending()?;
            if ring.sq_available() == 0 {
                counters.bounce_stall_sq_space = counters.bounce_stall_sq_space.saturating_add(1);
                return Ok(TcpWalZcrxQueuedWrites::default());
            }
        }
    }
    let remainder = len.saturating_sub(needed);
    let next_write_slot = if remainder > 0 {
        let Some(next_write_slot) = free_write_slots.pop() else {
            counters.bounce_stall_write_slots = counters.bounce_stall_write_slots.saturating_add(1);
            return Ok(TcpWalZcrxQueuedWrites::default());
        };
        Some(next_write_slot)
    } else {
        None
    };

    let data = zcrx.data_for_cqe(&cqe)?;
    let copy_to_current = len.min(needed);
    tcp_wal_zcrx_copy_coalesced_bytes(
        unsafe { bounce_buffers.ptr(write_slot).add(coalesce_conn.filled) },
        data.as_ptr(),
        copy_to_current,
        coalesce_nt_copy,
        coalesce_nt_min_bytes,
        counters,
    );
    if let Some(next_write_slot) = next_write_slot {
        tcp_wal_zcrx_copy_coalesced_bytes(
            bounce_buffers.ptr(next_write_slot),
            unsafe { data.as_ptr().add(copy_to_current) },
            remainder,
            coalesce_nt_copy,
            coalesce_nt_min_bytes,
            counters,
        );
    }
    coalesce_conn.filled += copy_to_current;
    counters.coalesce_appends = counters.coalesce_appends.saturating_add(1);
    zcrx.return_buffer(&cqe)?;

    if coalesce_conn.filled < coalesce_write_bytes {
        return Ok(TcpWalZcrxQueuedWrites {
            consumed: true,
            ..Default::default()
        });
    }
    coalesce_conn.write_slot = next_write_slot;
    coalesce_conn.filled = remainder;
    let mut queued = tcp_wal_zcrx_queue_coalesced_chunk(
        ring,
        fd,
        bounce_buffers,
        inflight,
        frame_slots,
        free_frame_slots,
        next_wal_offset,
        wal_region_end,
        required_alignment,
        conn,
        write_slot,
        bounce_fixed_base,
        coalesce_fixed,
        coalesce_write_bytes,
        chunk_bytes,
        counters,
    )?;
    queued.consumed = true;
    Ok(queued)
}

#[allow(clippy::too_many_arguments)]
fn tcp_wal_zcrx_queue_write(
    ring: &mut RawRing,
    fd: i32,
    zcrx: &mut ZcrxContext,
    slot_ids: &[io_slots::IoSlotId],
    zcrx_slot_busy: &mut [bool],
    bounce_buffers: &FixedSendBuffers,
    bounce_stride: usize,
    inflight: &mut [Option<TcpWalZcrxWriteSlot>],
    free_write_slots: &mut Vec<usize>,
    frame_slots: &mut [Option<TcpWalZcrxFrameSlot>],
    free_frame_slots: &mut Vec<usize>,
    next_wal_offset: &mut u64,
    wal_region_end: u64,
    required_alignment: usize,
    slot_stride: usize,
    slot_busy_guard: bool,
    busy_bounce: bool,
    stall_coalesce: bool,
    chunk_bytes: usize,
    coalesce_conns: &mut [TcpWalZcrxCoalesceConn],
    bounce_fixed_base: usize,
    coalesce_fixed: bool,
    coalesce_write_bytes: usize,
    coalesce_nt_copy: bool,
    coalesce_nt_min_bytes: usize,
    conn: usize,
    cqe: IoUringCqe32,
    counters: &mut TcpWalZcrxWorkerCounters,
) -> io::Result<TcpWalZcrxQueuedWrites> {
    let offset = zcrx.offset_for_cqe(&cqe)?;
    let len = cqe.res as usize;
    let coalesce_conn = coalesce_conns.get_mut(conn).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ZCRX WAL missing coalescer for connection {conn}"),
        )
    })?;
    let direct_allowed = !slot_ids.is_empty() && coalesce_conn.write_slot.is_none();
    let Some(direct_range) = (if direct_allowed {
        tcp_wal_zcrx_direct_full_chunk_segment_range(
            offset,
            len,
            chunk_bytes,
            required_alignment,
            slot_stride,
            slot_ids.len(),
        )?
    } else {
        None
    }) else {
        counters.direct_fallback_to_bounce = counters.direct_fallback_to_bounce.saturating_add(1);
        return tcp_wal_zcrx_queue_coalesced_frame(
            ring,
            fd,
            zcrx,
            bounce_buffers,
            bounce_stride,
            inflight,
            free_write_slots,
            frame_slots,
            free_frame_slots,
            next_wal_offset,
            wal_region_end,
            required_alignment,
            chunk_bytes,
            coalesce_conns,
            bounce_fixed_base,
            coalesce_fixed,
            coalesce_write_bytes,
            coalesce_nt_copy,
            coalesce_nt_min_bytes,
            conn,
            cqe,
            counters,
        );
    };
    let first_buf_index = direct_range.first_buf_index;
    let segments = direct_range.segments;

    counters.direct_queue_attempts = counters.direct_queue_attempts.saturating_add(1);
    if direct_range.physical_len != len {
        counters.direct_padded_frames = counters.direct_padded_frames.saturating_add(1);
        counters.direct_padded_bytes = counters
            .direct_padded_bytes
            .saturating_add(direct_range.physical_len.saturating_sub(len));
    }
    if segments > 1 {
        counters.direct_multisegment_frames = counters.direct_multisegment_frames.saturating_add(1);
    }
    if free_write_slots.len() < segments {
        counters.direct_stall_write_slots = counters.direct_stall_write_slots.saturating_add(1);
        if stall_coalesce {
            counters.direct_stall_coalesce_attempts =
                counters.direct_stall_coalesce_attempts.saturating_add(1);
            return tcp_wal_zcrx_queue_coalesced_frame(
                ring,
                fd,
                zcrx,
                bounce_buffers,
                bounce_stride,
                inflight,
                free_write_slots,
                frame_slots,
                free_frame_slots,
                next_wal_offset,
                wal_region_end,
                required_alignment,
                chunk_bytes,
                coalesce_conns,
                bounce_fixed_base,
                coalesce_fixed,
                coalesce_write_bytes,
                coalesce_nt_copy,
                coalesce_nt_min_bytes,
                conn,
                cqe,
                counters,
            );
        }
        return Ok(TcpWalZcrxQueuedWrites::default());
    }
    if free_frame_slots.is_empty() {
        counters.direct_stall_frame_slots = counters.direct_stall_frame_slots.saturating_add(1);
        if stall_coalesce {
            counters.direct_stall_coalesce_attempts =
                counters.direct_stall_coalesce_attempts.saturating_add(1);
            return tcp_wal_zcrx_queue_coalesced_frame(
                ring,
                fd,
                zcrx,
                bounce_buffers,
                bounce_stride,
                inflight,
                free_write_slots,
                frame_slots,
                free_frame_slots,
                next_wal_offset,
                wal_region_end,
                required_alignment,
                chunk_bytes,
                coalesce_conns,
                bounce_fixed_base,
                coalesce_fixed,
                coalesce_write_bytes,
                coalesce_nt_copy,
                coalesce_nt_min_bytes,
                conn,
                cqe,
                counters,
            );
        }
        return Ok(TcpWalZcrxQueuedWrites::default());
    }
    if slot_busy_guard && tcp_wal_zcrx_direct_slots_busy(first_buf_index, segments, zcrx_slot_busy)?
    {
        counters.direct_stall_busy_slots = counters.direct_stall_busy_slots.saturating_add(1);
        if busy_bounce {
            counters.direct_busy_bounce_attempts =
                counters.direct_busy_bounce_attempts.saturating_add(1);
            return tcp_wal_zcrx_queue_coalesced_frame(
                ring,
                fd,
                zcrx,
                bounce_buffers,
                bounce_stride,
                inflight,
                free_write_slots,
                frame_slots,
                free_frame_slots,
                next_wal_offset,
                wal_region_end,
                required_alignment,
                chunk_bytes,
                coalesce_conns,
                bounce_fixed_base,
                coalesce_fixed,
                coalesce_write_bytes,
                coalesce_nt_copy,
                coalesce_nt_min_bytes,
                conn,
                cqe,
                counters,
            );
        }
        return Ok(TcpWalZcrxQueuedWrites::default());
    }
    if ring.sq_available() < segments {
        ring.submit_pending()?;
        if ring.sq_available() < segments {
            counters.direct_stall_sq_space = counters.direct_stall_sq_space.saturating_add(1);
            if stall_coalesce {
                counters.direct_stall_coalesce_attempts =
                    counters.direct_stall_coalesce_attempts.saturating_add(1);
                return tcp_wal_zcrx_queue_coalesced_frame(
                    ring,
                    fd,
                    zcrx,
                    bounce_buffers,
                    bounce_stride,
                    inflight,
                    free_write_slots,
                    frame_slots,
                    free_frame_slots,
                    next_wal_offset,
                    wal_region_end,
                    required_alignment,
                    chunk_bytes,
                    coalesce_conns,
                    bounce_fixed_base,
                    coalesce_fixed,
                    coalesce_write_bytes,
                    coalesce_nt_copy,
                    coalesce_nt_min_bytes,
                    conn,
                    cqe,
                    counters,
                );
            }
            return Ok(TcpWalZcrxQueuedWrites::default());
        }
    }

    let file_offset = tcp_wal_zcrx_allocate_wal(
        next_wal_offset,
        direct_range.physical_len,
        wal_region_end,
        required_alignment,
        "direct",
    )?;

    let frame_slot = free_frame_slots
        .pop()
        .expect("free_frame_slots was checked above");
    frame_slots[frame_slot] = Some(TcpWalZcrxFrameSlot {
        cqe: Some(cqe),
        conn,
        pending_writes: segments,
    });
    let logical_end = offset
        .checked_add(len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ZCRX WAL frame end overflow"))?;
    let mut remaining = direct_range.physical_len;
    let mut frame_offset = 0usize;
    for segment in 0..segments {
        let write_slot = free_write_slots
            .pop()
            .expect("free_write_slots length was checked above");
        let buf_index = first_buf_index + segment;
        let slot_id = slot_ids[buf_index];
        let segment_area_offset = direct_range
            .aligned_offset
            .checked_add(frame_offset)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ZCRX WAL segment offset overflow",
                )
            })?;
        let buf_offset = segment_area_offset % slot_stride;
        let segment_len = remaining.min(slot_stride - buf_offset);
        if segment_len == 0 || segment_len % required_alignment != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ZCRX WAL segment len={segment_len} buf_offset={buf_offset} is not aligned to {required_alignment}"
                ),
            ));
        }
        let segment_len_u32 = tcp_bench_u32_len(segment_len, "ZCRX WAL segment length")?;
        let segment_file_offset = file_offset + frame_offset as u64;
        let segment_end = segment_area_offset
            .checked_add(segment_len)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "ZCRX WAL segment end overflow")
            })?;
        let logical_overlap_start = segment_area_offset.max(offset);
        let logical_overlap_end = segment_end.min(logical_end);
        let logical_len = logical_overlap_end.saturating_sub(logical_overlap_start);
        ring.queue_slot_rw(
            slot_id,
            buf_offset as u64,
            segment_file_offset,
            segment_len_u32,
            io_slots::SlotRw::Write,
            tcp_wal_zcrx_write_user_data(write_slot),
        )?;
        if slot_busy_guard {
            zcrx_slot_busy[buf_index] = true;
        }
        inflight[write_slot] = Some(TcpWalZcrxWriteSlot {
            frame_slot,
            conn,
            len: segment_len_u32,
            logical_len: logical_len as u32,
            logical_chunks: usize::from(segment == 0),
            file_offset: segment_file_offset,
            zcrx_slot: slot_busy_guard.then_some(buf_index),
        });
        remaining -= segment_len;
        frame_offset += segment_len;
    }
    counters.direct_segment_writes = counters.direct_segment_writes.saturating_add(segments);
    Ok(TcpWalZcrxQueuedWrites {
        consumed: true,
        writes: segments,
        direct_frames: 1,
        direct_bytes: len,
        bounce_frames: 0,
        bounce_bytes: 0,
        coalesce_frames: 0,
        coalesce_bytes: 0,
    })
}

#[allow(clippy::too_many_arguments)]
fn tcp_wal_zcrx_queue_coalesced_flush(
    ring: &mut RawRing,
    fd: i32,
    bounce_buffers: &FixedSendBuffers,
    inflight: &mut [Option<TcpWalZcrxWriteSlot>],
    free_write_slots: &mut Vec<usize>,
    frame_slots: &mut [Option<TcpWalZcrxFrameSlot>],
    free_frame_slots: &mut Vec<usize>,
    next_wal_offset: &mut u64,
    wal_region_end: u64,
    required_alignment: usize,
    conn: usize,
    coalesce_conns: &mut [TcpWalZcrxCoalesceConn],
    bounce_fixed_base: usize,
    coalesce_fixed: bool,
    chunk_bytes: usize,
    counters: &mut TcpWalZcrxWorkerCounters,
) -> io::Result<TcpWalZcrxQueuedWrites> {
    let (write_slot, write_bytes) = {
        let coalesce_conn = coalesce_conns.get_mut(conn).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ZCRX WAL missing coalescer for connection {conn}"),
            )
        })?;
        if coalesce_conn.filled == 0 {
            if let Some(write_slot) = coalesce_conn.write_slot.take() {
                free_write_slots.push(write_slot);
            }
            return Ok(TcpWalZcrxQueuedWrites {
                consumed: true,
                ..Default::default()
            });
        }
        if chunk_bytes == 0 || coalesce_conn.filled % chunk_bytes != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ZCRX WAL conn={conn} cannot flush partial coalesce batch filled={} chunk_bytes={chunk_bytes}",
                    coalesce_conn.filled
                ),
            ));
        }
        let Some(write_slot) = coalesce_conn.write_slot else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ZCRX WAL conn={conn} has {} coalesced bytes without a write slot",
                    coalesce_conn.filled
                ),
            ));
        };
        (write_slot, coalesce_conn.filled)
    };

    let queued = tcp_wal_zcrx_queue_coalesced_chunk(
        ring,
        fd,
        bounce_buffers,
        inflight,
        frame_slots,
        free_frame_slots,
        next_wal_offset,
        wal_region_end,
        required_alignment,
        conn,
        write_slot,
        bounce_fixed_base,
        coalesce_fixed,
        write_bytes,
        chunk_bytes,
        counters,
    )?;
    if queued.consumed {
        let coalesce_conn = coalesce_conns
            .get_mut(conn)
            .expect("coalesce conn was checked above");
        coalesce_conn.write_slot = None;
        coalesce_conn.filled = 0;
        counters.coalesce_flushes = counters.coalesce_flushes.saturating_add(1);
    }
    Ok(queued)
}

#[allow(clippy::too_many_arguments)]
fn tcp_wal_zcrx_complete_write_cqe(
    ring: &mut RawRing,
    zcrx: &mut ZcrxContext,
    conns: &mut [TcpWalConn],
    complete: &[bool],
    deferred_recvs: &mut [bool],
    recv_len: u32,
    max_active_recvs: usize,
    recv_multishot: bool,
    cqe: IoUringCqe32,
    inflight: &mut [Option<TcpWalZcrxWriteSlot>],
    free_write_slots: &mut Vec<usize>,
    frame_slots: &mut [Option<TcpWalZcrxFrameSlot>],
    free_frame_slots: &mut Vec<usize>,
    zcrx_slot_busy: &mut [bool],
    inflight_writes: &mut usize,
    written_bytes: &mut usize,
    wal_bytes: &mut usize,
    chunks: &mut usize,
    counters: &mut TcpWalZcrxWorkerCounters,
) -> io::Result<bool> {
    let Some(write_slot) = tcp_wal_zcrx_write_slot(cqe.user_data) else {
        return Ok(false);
    };
    if write_slot >= inflight.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ZCRX WAL write CQE returned invalid slot {write_slot}"),
        ));
    }
    let desc = inflight[write_slot].take().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ZCRX WAL duplicate write CQE for slot {write_slot}"),
        )
    })?;
    *inflight_writes = inflight_writes.saturating_sub(1);
    if cqe.res < 0 {
        return Err(io::Error::from_raw_os_error(-cqe.res));
    }
    if cqe.res != desc.len as i32 {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!(
                "short ZCRX WAL write completion: conn={} offset={} res={} expected={}",
                desc.conn, desc.file_offset, cqe.res, desc.len
            ),
        ));
    }
    free_write_slots.push(write_slot);
    if let Some(zcrx_slot) = desc.zcrx_slot {
        let slot_busy = zcrx_slot_busy.get_mut(zcrx_slot).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ZCRX WAL write referenced invalid direct slot {zcrx_slot}"),
            )
        })?;
        if !*slot_busy {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ZCRX WAL direct slot {zcrx_slot} completed while not busy"),
            ));
        }
        *slot_busy = false;
    }
    *written_bytes = written_bytes
        .checked_add(desc.logical_len as usize)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "ZCRX WAL written byte overflow")
        })?;
    *wal_bytes = wal_bytes.checked_add(desc.len as usize).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "ZCRX WAL physical byte overflow",
        )
    })?;
    *chunks = chunks.saturating_add(desc.logical_chunks);
    let frame = frame_slots
        .get_mut(desc.frame_slot)
        .and_then(Option::as_mut)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ZCRX WAL write CQE referenced missing frame slot {}",
                    desc.frame_slot
                ),
            )
        })?;
    if frame.conn != desc.conn {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "ZCRX WAL write CQE conn mismatch: frame_conn={} write_conn={}",
                frame.conn, desc.conn
            ),
        ));
    }
    frame.pending_writes = frame.pending_writes.checked_sub(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "ZCRX WAL frame pending write underflow",
        )
    })?;
    if frame.pending_writes == 0 {
        let frame = frame_slots[desc.frame_slot]
            .take()
            .expect("frame slot was just checked");
        if let Some(cqe) = &frame.cqe {
            zcrx.return_buffer(cqe)?;
        }
        free_frame_slots.push(desc.frame_slot);
        tcp_wal_zcrx_flush_deferred_recvs(
            ring,
            conns,
            complete,
            deferred_recvs,
            zcrx.zcrx_id,
            recv_len,
            max_active_recvs,
            recv_multishot,
            counters,
        )?;
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn tcp_wal_zcrx_mark_conn_complete(
    ring: &mut RawRing,
    conns: &mut [TcpWalConn],
    complete: &mut [bool],
    deferred_recvs: &mut [bool],
    streams: &[TcpStream],
    complete_count: &mut usize,
    zcrx_id: u32,
    recv_len: u32,
    max_active_recvs: usize,
    recv_multishot: bool,
    counters: &mut TcpWalZcrxWorkerCounters,
    conn_idx: usize,
) -> io::Result<()> {
    if !complete[conn_idx] {
        complete[conn_idx] = true;
        *complete_count += 1;
        let _ = streams[conn_idx].shutdown(Shutdown::Read);
        tcp_wal_zcrx_flush_deferred_recvs(
            ring,
            conns,
            complete,
            deferred_recvs,
            zcrx_id,
            recv_len,
            max_active_recvs,
            recv_multishot,
            counters,
        )?;
    }
    Ok(())
}

fn tcp_wal_zcrx_conn_has_pending_frame(
    pending_frames: &VecDeque<(usize, IoUringCqe32)>,
    conn_idx: usize,
) -> bool {
    pending_frames
        .iter()
        .any(|(pending_conn, _)| *pending_conn == conn_idx)
}

fn tcp_wal_zcrx_schedule_coalesce_flush(
    pending_coalesce_flush: &mut VecDeque<usize>,
    coalesce_flush_pending: &mut [bool],
    conn_idx: usize,
) {
    if !coalesce_flush_pending[conn_idx] {
        coalesce_flush_pending[conn_idx] = true;
        pending_coalesce_flush.push_back(conn_idx);
    }
}

#[allow(clippy::too_many_arguments)]
fn tcp_wal_zcrx_worker(
    target: SlotWalTarget,
    worker: usize,
    planned_cpu: Option<usize>,
    planned_nvme_queue: usize,
    ifname: String,
    rxq: u32,
    streams_rx: mpsc::Receiver<TcpWalZcrxWorkerAssignment>,
    ready_tx: mpsc::Sender<io::Result<u32>>,
    bytes_per_connection: usize,
    chunk_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    required_alignment: usize,
    pin: bool,
) -> io::Result<TcpWalZcrxWorkerResult> {
    let affinity =
        pin_current_thread_if_requested_to_cpu("tcp-wal-zcrx-worker", worker, pin, planned_cpu);
    let preferred_numa_node = if affinity.target_cpu >= 0 {
        cpu_numa_node(affinity.target_cpu as usize)
    } else {
        let local_cpu = current_cpu();
        (local_cpu >= 0)
            .then(|| cpu_numa_node(local_cpu as usize))
            .flatten()
    };

    let recv_len = tcp_bench_u32_len(chunk_bytes, "ZCRX WAL recv chunk-bytes")?;
    let pipeline = pipeline.max(1);
    let coalesce_fixed = env_enabled_or("URING_PLAY_TCP_WAL_ZCRX_COALESCE_FIXED", false);
    let default_write_frame_pipeline = pipeline
        .saturating_mul(env_usize_or(
            "URING_PLAY_TCP_WAL_ZCRX_PIPELINE_MULTIPLIER",
            4,
        ))
        .min(env_usize_or(
            "URING_PLAY_TCP_WAL_ZCRX_MAX_WRITE_PIPELINE",
            4096,
        ))
        .max(pipeline);
    let write_frame_pipeline = env_size_opt("URING_PLAY_TCP_WAL_ZCRX_WRITE_PIPELINE")?
        .unwrap_or(default_write_frame_pipeline)
        .max(pipeline);
    let requested_frame_pipeline = env_size_opt("URING_PLAY_TCP_WAL_ZCRX_FRAME_PIPELINE")?;
    let (requested_slot_stride, configured_slot_stride) =
        tcp_wal_zcrx_slot_stride_from_env(required_alignment)?;
    let max_segments_per_frame = (recv_len as usize)
        .checked_add(requested_slot_stride.saturating_sub(1))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "ZCRX segment count overflow")
        })?
        / requested_slot_stride;
    let write_pipeline = write_frame_pipeline
        .checked_mul(max_segments_per_frame.max(1))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "ZCRX WAL write pipeline overflow",
            )
        })?;
    let frame_pipeline = requested_frame_pipeline
        .unwrap_or(write_pipeline)
        .max(write_pipeline)
        .max(pipeline);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(target.open_path())?;
    let fd = file.as_raw_fd();

    let min_ring_entries = tcp_bench_u32_len(
        write_pipeline.saturating_mul(2).max(64),
        "ZCRX WAL ring entries",
    )?;
    let ring_entries = ring_entries.max(min_ring_entries);
    let mut ring = RawRing::new(ring_entries, ring_entries.saturating_mul(2))?;
    ring.register_napi_from_env(&format!("tcp-wal-zcrx-worker-{worker}"))?;

    let mut fds = [fd];
    ring.register_files(&mut fds)?;
    let coalesce_batch_chunks =
        env_usize_or("URING_PLAY_TCP_WAL_ZCRX_COALESCE_BATCH_CHUNKS", 1).max(1);
    let coalesce_write_bytes = chunk_bytes
        .checked_mul(coalesce_batch_chunks)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "ZCRX WAL coalesce batch byte count overflow",
            )
        })?;
    if coalesce_write_bytes % required_alignment != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "ZCRX WAL coalesce_write_bytes={coalesce_write_bytes} is not aligned to {required_alignment}"
            ),
        ));
    }
    let _ = tcp_bench_u32_len(coalesce_write_bytes, "ZCRX WAL coalesced write length")?;
    let bounce_stride = align_up(coalesce_write_bytes, required_alignment);
    let bounce_buffers = FixedSendBuffers::new_with_preferred_numa(
        write_pipeline,
        bounce_stride,
        preferred_numa_node,
    )?;

    let _ = ready_tx.send(Ok(rxq));
    let assignment = streams_rx.recv().map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("ZCRX WAL worker {worker} stream channel closed before accept"),
        )
    })?;
    let wal_region_base = assignment.wal_region_base;
    let wal_region_end = assignment.wal_region_end;
    if wal_region_end < wal_region_base {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "ZCRX WAL worker {worker} got inverted WAL region \
                 [{wal_region_base}..{wal_region_end})"
            ),
        ));
    }
    let mut next_wal_offset = wal_region_base;
    let assigned_streams = assignment.streams;
    let stream_metas = assigned_streams
        .iter()
        .map(|stream| stream.meta)
        .collect::<Vec<_>>();
    let streams = assigned_streams
        .into_iter()
        .map(|stream| stream.stream)
        .collect::<Vec<_>>();
    let stream_count = streams.len();
    let tid = current_tid();
    let start_thread_cpu = thread_cpu_time().unwrap_or_default();
    let start_cpu = current_cpu();
    let start_switches = read_thread_context_switches(tid).unwrap_or_default();
    if stream_count == 0 {
        let end_thread_cpu = thread_cpu_time().unwrap_or(start_thread_cpu);
        let end_cpu = current_cpu();
        let end_switches = read_thread_context_switches(tid).unwrap_or(start_switches);
        ring.unregister_files()?;
        return Ok(TcpWalZcrxWorkerResult {
            worker,
            rxq,
            planned_nvme_queue,
            streams: 0,
            wal_region_base,
            wal_region_end,
            slot_stride: 0,
            fixed_buffers: 0,
            zcrx_area_base: 0,
            zcrx_area_bytes: 0,
            zcrx_area_alignment: 0,
            zcrx_rx_buf_len: 0,
            zcrx_area_memory_policy: "not-registered",
            write_pipeline,
            frame_pipeline,
            coalesce_batch_chunks,
            coalesce_write_bytes,
            received_bytes: 0,
            written_bytes: 0,
            wal_bytes: 0,
            direct_frames: 0,
            direct_bytes: 0,
            bounce_frames: 0,
            bounce_bytes: 0,
            coalesce_frames: 0,
            coalesce_bytes: 0,
            frames: 0,
            chunks: 0,
            elapsed: Duration::ZERO,
            cpu: end_thread_cpu.saturating_sub(start_thread_cpu),
            target_cpu: affinity.target_cpu,
            affinity_applied: affinity.applied,
            start_cpu,
            end_cpu,
            voluntary_switches: end_switches
                .voluntary
                .saturating_sub(start_switches.voluntary),
            involuntary_switches: end_switches
                .involuntary
                .saturating_sub(start_switches.involuntary),
            migrations: end_switches
                .migrations
                .saturating_sub(start_switches.migrations),
            worker_numa_node: preferred_numa_node,
            counters: TcpWalZcrxWorkerCounters::default(),
            timing: TcpWalZcrxTimingCounters {
                enabled: env_enabled_or("URING_PLAY_TCP_WAL_ZCRX_TIMING", false),
                ..Default::default()
            },
            conn_results: Vec::new(),
        });
    }

    let zcrx_area_size = tcp_wal_zcrx_worker_area_size(
        stream_count,
        bytes_per_connection,
        recv_len,
        write_pipeline,
    )?;
    let mut zcrx = ZcrxContext::register_with_options(
        ring.fd(),
        &ifname,
        rxq,
        worker == 0,
        None,
        preferred_numa_node,
        Some(zcrx_area_size),
    )?;
    let zcrx_area_base = zcrx.area_ptr as usize;
    let zcrx_area_bytes = zcrx.area_size;
    let zcrx_area_alignment = address_alignment(zcrx_area_base);
    let zcrx_rx_buf_len = zcrx.rx_buf_len;
    let zcrx_area_memory_policy = zcrx.area_memory_policy;
    let initial_slot_stride = tcp_wal_zcrx_initial_slot_stride(
        requested_slot_stride,
        configured_slot_stride,
        zcrx_rx_buf_len,
        required_alignment,
    );
    let slot_stride = tcp_wal_zcrx_auto_slot_stride(
        zcrx.area_size,
        initial_slot_stride,
        required_alignment,
        configured_slot_stride,
        if coalesce_fixed { write_pipeline } else { 0 },
    )?;
    if zcrx.area_size % slot_stride != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "ZCRX area size {} is not a multiple of io-slot stride {slot_stride}",
                zcrx.area_size,
            ),
        ));
    }
    let fixed_buffer_count = zcrx.area_size / slot_stride;
    if fixed_buffer_count == 0 || fixed_buffer_count > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "ZCRX area maps to {fixed_buffer_count} fixed buffers with slot_stride={slot_stride}; \
                 io-slot ids support 1..={}",
                u16::MAX,
            ),
        ));
    }
    if fixed_buffer_count > IORING_MAX_REG_BUFFERS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "ZCRX WAL needs {fixed_buffer_count} ZCRX registered buffers, exceeding \
                 the {IORING_MAX_REG_BUFFERS} registered-buffer limit"
            ),
        ));
    }

    let direct_enabled = env_enabled_or("URING_PLAY_TCP_WAL_ZCRX_DIRECT", true)
        && tcp_wal_zcrx_direct_fits_without_recycling(
            stream_count,
            bytes_per_connection,
            zcrx.area_size,
            recv_len,
            write_pipeline,
        )?;
    let recv_multishot = env_enabled_or("URING_PLAY_TCP_WAL_ZCRX_RECV_MULTISHOT", true);
    let slot_busy_guard = env_enabled_or("URING_PLAY_TCP_WAL_ZCRX_SLOT_BUSY_GUARD", true);
    let busy_bounce = env_enabled_or("URING_PLAY_TCP_WAL_ZCRX_BUSY_BOUNCE", true);
    let stall_coalesce = env_enabled_or("URING_PLAY_TCP_WAL_ZCRX_STALL_COALESCE", false);
    let coalesce_nt_copy = env_enabled_or("URING_PLAY_TCP_WAL_ZCRX_COALESCE_NT_COPY", false);
    let coalesce_nt_min_bytes =
        env_usize_or("URING_PLAY_TCP_WAL_ZCRX_COALESCE_NT_MIN_BYTES", 4096).max(1);
    let max_active_recvs_default = if direct_enabled { stream_count } else { 1 };
    let max_active_recvs = env_usize_or(
        "URING_PLAY_TCP_WAL_ZCRX_MAX_ACTIVE_RECVS",
        max_active_recvs_default,
    )
    .max(1)
    .min(stream_count);
    let mut counters = TcpWalZcrxWorkerCounters {
        direct_enabled,
        recv_multishot,
        max_active_recvs,
        direct_slot_busy_guard: slot_busy_guard,
        direct_busy_bounce: busy_bounce,
        direct_stall_coalesce: stall_coalesce,
        coalesce_nt_copy,
        ..Default::default()
    };
    let mut timing = TcpWalZcrxTimingCounters {
        enabled: env_enabled_or("URING_PLAY_TCP_WAL_ZCRX_TIMING", false),
        ..Default::default()
    };
    if !direct_enabled {
        let pressure_recv_buffer = env_usize_or(
            "URING_PLAY_TCP_WAL_ZCRX_PRESSURE_RCVBUF",
            (recv_len as usize).saturating_mul(32),
        )
        .max(recv_len as usize);
        for stream in &streams {
            set_socket_recv_buffer(stream.as_raw_fd(), pressure_recv_buffer)?;
        }
    }
    let mut slot_ids = Vec::new();
    let mut registered_buffers = false;
    let mut bounce_fixed_base = 0usize;
    if direct_enabled {
        let coalesce_registered_buffers = coalesce_fixed.then_some(write_pipeline).unwrap_or(0);
        let total_registered_buffers = fixed_buffer_count
            .checked_add(coalesce_registered_buffers)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ZCRX WAL registered buffer count overflow",
                )
            })?;
        if total_registered_buffers > IORING_MAX_REG_BUFFERS
            || total_registered_buffers > u16::MAX as usize + 1
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "ZCRX WAL needs {fixed_buffer_count} ZCRX buffers plus {write_pipeline} \
                     coalescer buffers, exceeding the registered-buffer limit"
                ),
            ));
        }
        let mut iovecs = Vec::with_capacity(total_registered_buffers);
        iovecs.extend((0..fixed_buffer_count).map(|index| libc::iovec {
            iov_base: unsafe {
                (zcrx.area_ptr as *mut u8)
                    .add(index.saturating_mul(slot_stride))
                    .cast()
            },
            iov_len: slot_stride,
        }));
        bounce_fixed_base = iovecs.len();
        if coalesce_fixed {
            iovecs.extend((0..write_pipeline).map(|index| libc::iovec {
                iov_base: bounce_buffers.ptr(index).cast(),
                iov_len: bounce_buffers.stride(),
            }));
        }
        ring.register_buffers(&mut iovecs)?;
        registered_buffers = true;
        slot_ids.reserve(fixed_buffer_count);
        for buf_index in 0..fixed_buffer_count {
            slot_ids.push(ring.register_io_slot(buf_index as u32, 0)?);
        }
    } else if coalesce_fixed {
        if write_pipeline > IORING_MAX_REG_BUFFERS || write_pipeline > u16::MAX as usize + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "ZCRX WAL coalescer needs {write_pipeline} registered buffers, exceeding \
                     the registered-buffer limit"
                ),
            ));
        }
        let mut iovecs = (0..write_pipeline)
            .map(|index| libc::iovec {
                iov_base: bounce_buffers.ptr(index).cast(),
                iov_len: bounce_buffers.stride(),
            })
            .collect::<Vec<_>>();
        ring.register_buffers(&mut iovecs)?;
        registered_buffers = true;
    }

    let mut conns = streams
        .iter()
        .map(|stream| TcpWalConn {
            fd: stream.as_raw_fd(),
            fixed_file: false,
            received: 0,
            recv_in_flight: false,
        })
        .collect::<Vec<_>>();
    let mut complete = vec![false; stream_count];
    let mut empty_final_progress = vec![usize::MAX; stream_count];
    let mut transient_recv_retries = vec![0usize; stream_count];
    let mut deferred_recvs = vec![true; stream_count];
    let mut complete_count = 0usize;
    let mut pending_frames = VecDeque::<(usize, IoUringCqe32)>::new();
    let mut pending_coalesce_flush = VecDeque::<usize>::new();
    let mut coalesce_flush_pending = vec![false; stream_count];
    let mut final_pending = vec![false; stream_count];
    let mut inflight = (0..write_pipeline).map(|_| None).collect::<Vec<_>>();
    let mut free_write_slots = (0..write_pipeline).rev().collect::<Vec<_>>();
    let mut frame_slots = (0..frame_pipeline).map(|_| None).collect::<Vec<_>>();
    let mut free_frame_slots = (0..frame_pipeline).rev().collect::<Vec<_>>();
    let mut coalesce_conns = vec![TcpWalZcrxCoalesceConn::default(); stream_count];
    let mut conn_counters = vec![TcpWalZcrxConnCounters::default(); stream_count];
    let mut zcrx_slot_busy = vec![false; slot_ids.len()];
    let mut inflight_writes = 0usize;
    let mut received_bytes = 0usize;
    let mut written_bytes = 0usize;
    let mut wal_bytes = 0usize;
    let mut direct_frames = 0usize;
    let mut direct_bytes = 0usize;
    let mut bounce_frames = 0usize;
    let mut bounce_bytes = 0usize;
    let mut coalesce_frames = 0usize;
    let mut coalesce_bytes = 0usize;
    let mut frames = 0usize;
    let mut chunks = 0usize;

    tcp_wal_zcrx_flush_deferred_recvs(
        &mut ring,
        &mut conns,
        &complete,
        &mut deferred_recvs,
        zcrx.zcrx_id,
        recv_len,
        max_active_recvs,
        recv_multishot,
        &mut counters,
    )?;
    maybe_send_ready_handshake(&streams, &format!("tcp-wal-zcrx-worker-{worker}"))?;

    let started = Instant::now();
    while complete_count < stream_count
        || inflight_writes > 0
        || !pending_frames.is_empty()
        || !pending_coalesce_flush.is_empty()
        || final_pending.iter().any(|pending| *pending)
    {
        let mut coalesce_flush_blocked = false;
        while let Some(conn) = pending_coalesce_flush.pop_front() {
            coalesce_flush_pending[conn] = false;
            let timing_start = timing.start();
            let queued_writes = tcp_wal_zcrx_queue_coalesced_flush(
                &mut ring,
                fd,
                &bounce_buffers,
                &mut inflight,
                &mut free_write_slots,
                &mut frame_slots,
                &mut free_frame_slots,
                &mut next_wal_offset,
                wal_region_end,
                required_alignment,
                conn,
                &mut coalesce_conns,
                bounce_fixed_base,
                coalesce_fixed,
                chunk_bytes,
                &mut counters,
            )?;
            timing.record_coalesce_flush(timing_start);
            if queued_writes.consumed {
                inflight_writes += queued_writes.writes;
                coalesce_frames += queued_writes.coalesce_frames;
                coalesce_bytes += queued_writes.coalesce_bytes;
                tcp_wal_zcrx_record_conn_queued(&mut conn_counters, conn, &queued_writes)?;
                if conns[conn].received == bytes_per_connection {
                    final_pending[conn] = false;
                    tcp_wal_zcrx_mark_conn_complete(
                        &mut ring,
                        &mut conns,
                        &mut complete,
                        &mut deferred_recvs,
                        &streams,
                        &mut complete_count,
                        zcrx.zcrx_id,
                        recv_len,
                        max_active_recvs,
                        recv_multishot,
                        &mut counters,
                        conn,
                    )?;
                }
            } else {
                coalesce_flush_pending[conn] = true;
                pending_coalesce_flush.push_front(conn);
                coalesce_flush_blocked = true;
                break;
            }
        }

        while !coalesce_flush_blocked {
            let Some((conn, cqe)) = pending_frames.pop_front() else {
                break;
            };
            let timing_start = timing.start();
            let queued_writes = tcp_wal_zcrx_queue_write(
                &mut ring,
                fd,
                &mut zcrx,
                &slot_ids,
                &mut zcrx_slot_busy,
                &bounce_buffers,
                bounce_stride,
                &mut inflight,
                &mut free_write_slots,
                &mut frame_slots,
                &mut free_frame_slots,
                &mut next_wal_offset,
                wal_region_end,
                required_alignment,
                slot_stride,
                slot_busy_guard,
                busy_bounce,
                stall_coalesce,
                chunk_bytes,
                &mut coalesce_conns,
                bounce_fixed_base,
                coalesce_fixed,
                coalesce_write_bytes,
                coalesce_nt_copy,
                coalesce_nt_min_bytes,
                conn,
                cqe,
                &mut counters,
            )?;
            timing.record_queue_write(timing_start);
            if queued_writes.consumed {
                inflight_writes += queued_writes.writes;
                direct_frames += queued_writes.direct_frames;
                direct_bytes += queued_writes.direct_bytes;
                bounce_frames += queued_writes.bounce_frames;
                bounce_bytes += queued_writes.bounce_bytes;
                coalesce_frames += queued_writes.coalesce_frames;
                coalesce_bytes += queued_writes.coalesce_bytes;
                tcp_wal_zcrx_record_conn_queued(&mut conn_counters, conn, &queued_writes)?;
                if !recv_multishot && !complete[conn] {
                    deferred_recvs[conn] = true;
                    if pending_frames.is_empty() {
                        tcp_wal_zcrx_flush_deferred_recvs(
                            &mut ring,
                            &mut conns,
                            &complete,
                            &mut deferred_recvs,
                            zcrx.zcrx_id,
                            recv_len,
                            max_active_recvs,
                            recv_multishot,
                            &mut counters,
                        )?;
                    }
                }
            } else {
                counters.pending_frame_requeues = counters.pending_frame_requeues.saturating_add(1);
                tcp_wal_zcrx_record_conn_requeue(&mut conn_counters, conn)?;
                pending_frames.push_front((conn, cqe));
                counters.pending_frame_max = counters.pending_frame_max.max(pending_frames.len());
                break;
            }
        }

        for conn in 0..stream_count {
            if !final_pending[conn] || complete[conn] {
                continue;
            }
            if tcp_wal_zcrx_conn_has_pending_frame(&pending_frames, conn) {
                continue;
            }
            if coalesce_conns[conn].write_slot.is_some() || coalesce_conns[conn].filled != 0 {
                tcp_wal_zcrx_schedule_coalesce_flush(
                    &mut pending_coalesce_flush,
                    &mut coalesce_flush_pending,
                    conn,
                );
                final_pending[conn] = false;
            } else {
                final_pending[conn] = false;
                tcp_wal_zcrx_mark_conn_complete(
                    &mut ring,
                    &mut conns,
                    &mut complete,
                    &mut deferred_recvs,
                    &streams,
                    &mut complete_count,
                    zcrx.zcrx_id,
                    recv_len,
                    max_active_recvs,
                    recv_multishot,
                    &mut counters,
                    conn,
                )?;
            }
        }

        if complete_count == stream_count
            && inflight_writes == 0
            && pending_frames.is_empty()
            && pending_coalesce_flush.is_empty()
            && !final_pending.iter().any(|pending| *pending)
        {
            break;
        }

        let timing_start = timing.start();
        let cqe = ring.wait_cqe()?;
        timing.record_wait_cqe(timing_start);
        let timing_start = timing.start();
        let completed_write_cqe = tcp_wal_zcrx_complete_write_cqe(
            &mut ring,
            &mut zcrx,
            &mut conns,
            &complete,
            &mut deferred_recvs,
            recv_len,
            max_active_recvs,
            recv_multishot,
            cqe,
            &mut inflight,
            &mut free_write_slots,
            &mut frame_slots,
            &mut free_frame_slots,
            &mut zcrx_slot_busy,
            &mut inflight_writes,
            &mut written_bytes,
            &mut wal_bytes,
            &mut chunks,
            &mut counters,
        )?;
        timing.record_complete_cqe(timing_start);
        if completed_write_cqe {
            continue;
        }

        let conn_idx = cqe.user_data as usize;
        if conn_idx >= conns.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected ZCRX WAL CQE user_data={}", cqe.user_data),
            ));
        }

        let cqe_has_more = (cqe.flags & IORING_CQE_F_MORE) != 0;
        let single_shot_data = !recv_multishot && !cqe_has_more && cqe.res > 0;
        if !cqe_has_more {
            conns[conn_idx].recv_in_flight = false;
        }

        if !cqe_has_more && !single_shot_data {
            counters.recv_final_cqes = counters.recv_final_cqes.saturating_add(1);
            if cqe.res < 0 {
                if tcp_wal_zcrx_is_transient_recv_error(cqe.res)
                    && conns[conn_idx].received != bytes_per_connection
                {
                    counters.transient_recv_errors =
                        counters.transient_recv_errors.saturating_add(1);
                    if inflight_writes > 0 || !pending_frames.is_empty() {
                        counters.transient_recv_deferred =
                            counters.transient_recv_deferred.saturating_add(1);
                        deferred_recvs[conn_idx] = true;
                        continue;
                    }
                    transient_recv_retries[conn_idx] =
                        transient_recv_retries[conn_idx].saturating_add(1);
                    if transient_recv_retries[conn_idx] > TCP_WAL_ZCRX_TRANSIENT_RECV_RETRY_LIMIT {
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!(
                                "ZCRX WAL conn={conn_idx} exceeded transient recv_zc retry \
                                 limit {} after res={} flags=0x{:x} and {}/{} bytes: {}",
                                TCP_WAL_ZCRX_TRANSIENT_RECV_RETRY_LIMIT,
                                cqe.res,
                                cqe.flags,
                                conns[conn_idx].received,
                                bytes_per_connection,
                                io::Error::from_raw_os_error(-cqe.res)
                            ),
                        ));
                    }
                    counters.transient_recv_backoffs =
                        counters.transient_recv_backoffs.saturating_add(1);
                    tcp_wal_zcrx_transient_recv_backoff(transient_recv_retries[conn_idx]);
                    deferred_recvs[conn_idx] = true;
                    tcp_wal_zcrx_flush_deferred_recvs(
                        &mut ring,
                        &mut conns,
                        &complete,
                        &mut deferred_recvs,
                        zcrx.zcrx_id,
                        recv_len,
                        max_active_recvs,
                        recv_multishot,
                        &mut counters,
                    )?;
                    continue;
                }
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "ZCRX WAL conn={conn_idx} final recv_zc error res={} flags=0x{:x}: {}",
                        cqe.res,
                        cqe.flags,
                        io::Error::from_raw_os_error(-cqe.res)
                    ),
                ));
            }
            if cqe.res != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "ZCRX WAL conn={conn_idx} unexpected final recv_zc result {} flags=0x{:x}",
                        cqe.res, cqe.flags
                    ),
                ));
            }
            if conns[conn_idx].received != bytes_per_connection {
                if empty_final_progress[conn_idx] == conns[conn_idx].received {
                    if inflight_writes > 0 || !pending_frames.is_empty() {
                        counters.recv_empty_finals = counters.recv_empty_finals.saturating_add(1);
                        deferred_recvs[conn_idx] = true;
                        continue;
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "ZCRX WAL conn={conn_idx} received {}/{} bytes",
                            conns[conn_idx].received, bytes_per_connection
                        ),
                    ));
                }
                empty_final_progress[conn_idx] = conns[conn_idx].received;
                counters.recv_empty_finals = counters.recv_empty_finals.saturating_add(1);
                deferred_recvs[conn_idx] = true;
                tcp_wal_zcrx_flush_deferred_recvs(
                    &mut ring,
                    &mut conns,
                    &complete,
                    &mut deferred_recvs,
                    zcrx.zcrx_id,
                    recv_len,
                    max_active_recvs,
                    recv_multishot,
                    &mut counters,
                )?;
                continue;
            }
            final_pending[conn_idx] = true;
            continue;
        }

        if cqe.res <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ZCRX WAL conn={conn_idx} data CQE returned res={} flags=0x{:x}",
                    cqe.res, cqe.flags
                ),
            ));
        }
        if complete[conn_idx] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ZCRX WAL data after final CQE for connection {conn_idx}"),
            ));
        }

        let len = cqe.res as usize;
        counters.recv_data_cqes = counters.recv_data_cqes.saturating_add(1);
        tcp_wal_zcrx_record_conn_recv(&mut conn_counters, conn_idx, len, chunk_bytes)?;
        transient_recv_retries[conn_idx] = 0;
        empty_final_progress[conn_idx] = usize::MAX;
        conns[conn_idx].received = conns[conn_idx].received.checked_add(len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "ZCRX WAL receive byte overflow")
        })?;
        received_bytes = received_bytes.checked_add(len).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "ZCRX WAL total receive overflow",
            )
        })?;
        if conns[conn_idx].received > bytes_per_connection {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "ZCRX WAL conn={conn_idx} received too many bytes: {}/{}",
                    conns[conn_idx].received, bytes_per_connection
                ),
            ));
        }
        if !recv_multishot
            && conns[conn_idx].received == bytes_per_connection
            && !complete[conn_idx]
        {
            complete[conn_idx] = true;
            complete_count += 1;
            let _ = streams[conn_idx].shutdown(Shutdown::Read);
        }
        frames += 1;

        let timing_start = timing.start();
        let queued_writes = tcp_wal_zcrx_queue_write(
            &mut ring,
            fd,
            &mut zcrx,
            &slot_ids,
            &mut zcrx_slot_busy,
            &bounce_buffers,
            bounce_stride,
            &mut inflight,
            &mut free_write_slots,
            &mut frame_slots,
            &mut free_frame_slots,
            &mut next_wal_offset,
            wal_region_end,
            required_alignment,
            slot_stride,
            slot_busy_guard,
            busy_bounce,
            stall_coalesce,
            chunk_bytes,
            &mut coalesce_conns,
            bounce_fixed_base,
            coalesce_fixed,
            coalesce_write_bytes,
            coalesce_nt_copy,
            coalesce_nt_min_bytes,
            conn_idx,
            cqe,
            &mut counters,
        )?;
        timing.record_queue_write(timing_start);
        if queued_writes.consumed {
            inflight_writes += queued_writes.writes;
            direct_frames += queued_writes.direct_frames;
            direct_bytes += queued_writes.direct_bytes;
            bounce_frames += queued_writes.bounce_frames;
            bounce_bytes += queued_writes.bounce_bytes;
            coalesce_frames += queued_writes.coalesce_frames;
            coalesce_bytes += queued_writes.coalesce_bytes;
            tcp_wal_zcrx_record_conn_queued(&mut conn_counters, conn_idx, &queued_writes)?;
            if !recv_multishot && !complete[conn_idx] {
                deferred_recvs[conn_idx] = true;
                tcp_wal_zcrx_flush_deferred_recvs(
                    &mut ring,
                    &mut conns,
                    &complete,
                    &mut deferred_recvs,
                    zcrx.zcrx_id,
                    recv_len,
                    max_active_recvs,
                    recv_multishot,
                    &mut counters,
                )?;
            }
        } else {
            counters.pending_frame_requeues = counters.pending_frame_requeues.saturating_add(1);
            tcp_wal_zcrx_record_conn_requeue(&mut conn_counters, conn_idx)?;
            pending_frames.push_back((conn_idx, cqe));
            counters.pending_frame_max = counters.pending_frame_max.max(pending_frames.len());
        }
    }

    if let Some((conn, coalescer)) = coalesce_conns
        .iter()
        .enumerate()
        .find(|(_, coalescer)| coalescer.write_slot.is_some() || coalescer.filled != 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "ZCRX WAL conn={conn} ended with partial coalesced chunk: filled={} slot_present={}",
                coalescer.filled,
                yes(coalescer.write_slot.is_some())
            ),
        ));
    }

    for slot_id in slot_ids {
        ring.unregister_io_slot(slot_id)?;
    }
    if registered_buffers {
        ring.unregister_buffers()?;
    }
    ring.unregister_files()?;

    let end_thread_cpu = thread_cpu_time().unwrap_or(start_thread_cpu);
    let end_cpu = current_cpu();
    let end_switches = read_thread_context_switches(tid).unwrap_or(start_switches);

    Ok(TcpWalZcrxWorkerResult {
        worker,
        rxq,
        planned_nvme_queue,
        streams: stream_count,
        wal_region_base,
        wal_region_end,
        slot_stride,
        fixed_buffers: fixed_buffer_count,
        zcrx_area_base,
        zcrx_area_bytes,
        zcrx_area_alignment,
        zcrx_rx_buf_len,
        zcrx_area_memory_policy,
        write_pipeline,
        frame_pipeline,
        coalesce_batch_chunks,
        coalesce_write_bytes,
        received_bytes,
        written_bytes,
        wal_bytes,
        direct_frames,
        direct_bytes,
        bounce_frames,
        bounce_bytes,
        coalesce_frames,
        coalesce_bytes,
        frames,
        chunks,
        elapsed: started.elapsed(),
        cpu: end_thread_cpu.saturating_sub(start_thread_cpu),
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
        start_cpu,
        end_cpu,
        voluntary_switches: end_switches
            .voluntary
            .saturating_sub(start_switches.voluntary),
        involuntary_switches: end_switches
            .involuntary
            .saturating_sub(start_switches.involuntary),
        migrations: end_switches
            .migrations
            .saturating_sub(start_switches.migrations),
        worker_numa_node: preferred_numa_node,
        counters,
        timing,
        conn_results: stream_metas
            .into_iter()
            .zip(conn_counters)
            .map(|(meta, counters)| TcpWalZcrxConnResult { meta, counters })
            .collect(),
    })
}

fn tcp_wal_queue_recv_for_slot(
    ring: &mut RawRing,
    buffers: &FixedSendBuffers,
    conns: &[TcpWalConn],
    slots: &mut [TcpWalSlot],
    slot: usize,
    chunk_bytes: usize,
    chunk_bytes_u32: u32,
) -> io::Result<()> {
    let filled = slots[slot].filled;
    if filled >= chunk_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TCP WAL recv slot is already full",
        ));
    }

    let len = chunk_bytes - filled;
    let buf = unsafe { buffers.ptr(slot).add(filled) };
    let conn = &conns[slots[slot].conn];
    let len = len.min(chunk_bytes_u32 as usize) as u32;
    if conn.fixed_file {
        ring.queue_recv_fixed_file(conn.fd as u32, buf, len, 0, tcp_wal_recv_user_data(slot))
    } else {
        ring.queue_recv(conn.fd, buf, len, 0, tcp_wal_recv_user_data(slot))
    }
}

fn tcp_wal_schedule_recvs(
    ring: &mut RawRing,
    buffers: &FixedSendBuffers,
    conns: &mut [TcpWalConn],
    slots: &mut [TcpWalSlot],
    free_slots: &mut Vec<usize>,
    next_conn: &mut usize,
    expected_bytes: usize,
    chunk_bytes: usize,
    chunk_bytes_u32: u32,
) -> io::Result<usize> {
    if conns.is_empty() {
        return Ok(0);
    }

    let mut scheduled = 0usize;
    while let Some(slot) = free_slots.pop() {
        let mut conn_idx = None;
        for _ in 0..conns.len() {
            let candidate = *next_conn;
            *next_conn = (*next_conn + 1) % conns.len();
            if conns[candidate].received < expected_bytes && !conns[candidate].recv_in_flight {
                conn_idx = Some(candidate);
                break;
            }
        }

        let Some(conn_idx) = conn_idx else {
            free_slots.push(slot);
            break;
        };

        slots[slot] = TcpWalSlot {
            stage: TcpWalSlotStage::Recv,
            conn: conn_idx,
            filled: 0,
            file_offset: 0,
        };
        conns[conn_idx].recv_in_flight = true;
        tcp_wal_queue_recv_for_slot(
            ring,
            buffers,
            conns,
            slots,
            slot,
            chunk_bytes,
            chunk_bytes_u32,
        )?;
        scheduled += 1;
    }

    Ok(scheduled)
}

#[allow(clippy::too_many_arguments)]
fn tcp_wal_queue_write_for_slot(
    ring: &mut RawRing,
    fd: i32,
    slot_ids: &[io_slots::IoSlotId],
    buffers: &FixedSendBuffers,
    slot: usize,
    file_offset: u64,
    len: u32,
    write_mode: TcpWalWriteMode,
    user_data: u64,
) -> io::Result<()> {
    match write_mode {
        TcpWalWriteMode::Null => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "null TCP WAL write mode does not queue block writes",
        )),
        TcpWalWriteMode::Slot => ring.queue_slot_rw(
            slot_ids[slot],
            0,
            file_offset,
            len,
            io_slots::SlotRw::Write,
            user_data,
        ),
        TcpWalWriteMode::Write => ring.queue_write(
            fd,
            buffers.ptr(slot).cast_const(),
            len,
            file_offset,
            user_data,
        ),
        TcpWalWriteMode::WriteFixed => ring.queue_write_fixed(
            fd,
            buffers.ptr(slot).cast_const(),
            len,
            file_offset,
            slot as u16,
            user_data,
        ),
        TcpWalWriteMode::WriteFixedFile => ring.queue_write_fixed_file(
            0,
            buffers.ptr(slot).cast_const(),
            len,
            file_offset,
            slot as u16,
            user_data,
        ),
    }
}

fn tcp_wal_worker(
    target: SlotWalTarget,
    worker: usize,
    base_offset: u64,
    streams: Vec<TcpStream>,
    bytes_per_connection: usize,
    chunk_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    buffer_mode: SlotWalBufferMode,
    write_mode: TcpWalWriteMode,
    fixed_recv: bool,
    pin: bool,
) -> io::Result<TcpWalWorkerResult> {
    let affinity = pin_current_thread_if_requested("tcp-wal-worker", worker, pin);
    let stream_count = streams.len();
    if stream_count == 0 {
        return Ok(TcpWalWorkerResult {
            worker,
            streams: 0,
            received_bytes: 0,
            written_bytes: 0,
            chunks: 0,
            elapsed: Duration::ZERO,
            target_cpu: affinity.target_cpu,
            affinity_applied: affinity.applied,
        });
    }

    let chunk_bytes_u32 = u32::try_from(chunk_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk-bytes must fit in u32 for TCP WAL SQEs",
        )
    })?;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(target.open_path())?;
    let fd = file.as_raw_fd();

    let mut ring = RawRing::new(ring_entries, ring_entries.saturating_mul(2))?;
    ring.register_napi_from_env(&format!("tcp-wal-worker-{worker}"))?;
    let mut registered_fds = Vec::with_capacity(
        (if write_mode.needs_registered_files() {
            1usize
        } else {
            0usize
        })
        .saturating_add(streams.len()),
    );
    if write_mode.needs_registered_files() {
        registered_fds.push(fd);
    }
    let recv_file_base = registered_fds.len();
    if fixed_recv {
        registered_fds.extend(streams.iter().map(|stream| stream.as_raw_fd()));
    }
    let registered_files = !registered_fds.is_empty();
    if registered_files {
        ring.register_files(&mut registered_fds)?;
    }

    let buffers = buffer_mode.allocate(pipeline, chunk_bytes)?;
    let mut iovecs = buffers.iovecs(chunk_bytes);
    let registered_buffers = write_mode.needs_registered_buffers();
    if write_mode.needs_registered_buffers() {
        ring.register_buffers(&mut iovecs)?;
    }

    let mut slot_ids = Vec::with_capacity(pipeline);
    if write_mode.needs_io_slots() {
        for buf_index in 0..pipeline {
            slot_ids.push(ring.register_io_slot(buf_index as u32, 0)?);
        }
    }

    let mut conns = streams
        .iter()
        .enumerate()
        .map(|(index, stream)| {
            let (fd, fixed_file) = if fixed_recv {
                (
                    fixed_file_index_i32(recv_file_base + index, "TCP WAL receive")?,
                    true,
                )
            } else {
                (stream.as_raw_fd(), false)
            };
            Ok(TcpWalConn {
                fd,
                fixed_file,
                received: 0,
                recv_in_flight: false,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut slots = (0..pipeline)
        .map(|_| TcpWalSlot::default())
        .collect::<Vec<_>>();
    let mut free_slots = (0..pipeline).rev().collect::<Vec<_>>();
    let mut next_conn = 0usize;
    let mut outstanding = 0usize;
    let mut total_received = 0usize;
    let mut total_written = 0usize;
    let mut chunks = 0usize;
    let total_target = stream_count
        .checked_mul(bytes_per_connection)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "TCP WAL total target overflows",
            )
        })?;
    let mut next_wal_offset = base_offset;
    let started = Instant::now();

    while total_written < total_target {
        outstanding += tcp_wal_schedule_recvs(
            &mut ring,
            &buffers,
            &mut conns,
            &mut slots,
            &mut free_slots,
            &mut next_conn,
            bytes_per_connection,
            chunk_bytes,
            chunk_bytes_u32,
        )?;

        if outstanding == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "TCP WAL pipeline has no outstanding receive or write operations",
            ));
        }

        let cqe = ring.wait_cqe()?;
        outstanding = outstanding.saturating_sub(1);
        let (kind, slot) = tcp_wal_decode_user_data(cqe.user_data)?;
        if slot >= slots.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("TCP WAL CQE returned invalid slot {slot}"),
            ));
        }

        match kind {
            TcpWalCqeKind::Recv => {
                if slots[slot].stage != TcpWalSlotStage::Recv {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("TCP WAL recv CQE for slot {slot} not in recv state"),
                    ));
                }
                if cqe.res < 0 {
                    return Err(io::Error::from_raw_os_error(-cqe.res));
                }
                if cqe.res == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "TCP WAL connection {} ended after {}/{} bytes",
                            slots[slot].conn,
                            conns[slots[slot].conn].received,
                            bytes_per_connection
                        ),
                    ));
                }

                let n = cqe.res as usize;
                let conn_idx = slots[slot].conn;
                slots[slot].filled = slots[slot].filled.checked_add(n).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "TCP WAL slot fill overflow")
                })?;
                conns[conn_idx].received =
                    conns[conn_idx].received.checked_add(n).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "TCP WAL receive overflow")
                    })?;
                total_received = total_received.checked_add(n).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "TCP WAL total receive overflow")
                })?;

                if slots[slot].filled > chunk_bytes
                    || conns[conn_idx].received > bytes_per_connection
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "TCP WAL received too much data: conn={} conn_bytes={}/{} slot_filled={}/{}",
                            conn_idx,
                            conns[conn_idx].received,
                            bytes_per_connection,
                            slots[slot].filled,
                            chunk_bytes
                        ),
                    ));
                }

                if slots[slot].filled < chunk_bytes {
                    tcp_wal_queue_recv_for_slot(
                        &mut ring,
                        &buffers,
                        &conns,
                        &mut slots,
                        slot,
                        chunk_bytes,
                        chunk_bytes_u32,
                    )?;
                    outstanding += 1;
                    continue;
                }

                conns[conn_idx].recv_in_flight = false;
                if write_mode.is_null() {
                    total_written = total_written.checked_add(chunk_bytes).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "TCP WAL total write overflow")
                    })?;
                    chunks += 1;
                    slots[slot] = TcpWalSlot::default();
                    free_slots.push(slot);
                    continue;
                }
                let file_offset = next_wal_offset;
                next_wal_offset =
                    next_wal_offset
                        .checked_add(chunk_bytes as u64)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "WAL offset overflow")
                        })?;
                slots[slot].stage = TcpWalSlotStage::Write;
                slots[slot].file_offset = file_offset;
                tcp_wal_queue_write_for_slot(
                    &mut ring,
                    fd,
                    &slot_ids,
                    &buffers,
                    slot,
                    file_offset,
                    chunk_bytes_u32,
                    write_mode,
                    tcp_wal_write_user_data(slot),
                )?;
                outstanding += 1;
            }
            TcpWalCqeKind::Write => {
                if slots[slot].stage != TcpWalSlotStage::Write {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("TCP WAL write CQE for slot {slot} not in write state"),
                    ));
                }
                if cqe.res < 0 {
                    return Err(io::Error::from_raw_os_error(-cqe.res));
                }
                if cqe.res != chunk_bytes_u32 as i32 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        format!(
                            "short TCP WAL write completion: res={} expected={chunk_bytes}",
                            cqe.res
                        ),
                    ));
                }
                total_written = total_written.checked_add(chunk_bytes).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "TCP WAL total write overflow")
                })?;
                chunks += 1;
                slots[slot] = TcpWalSlot::default();
                free_slots.push(slot);
            }
        }
    }

    let elapsed = started.elapsed();

    for slot_id in slot_ids {
        ring.unregister_io_slot(slot_id)?;
    }
    if registered_buffers {
        ring.unregister_buffers()?;
    }
    if registered_files {
        ring.unregister_files()?;
    }

    Ok(TcpWalWorkerResult {
        worker,
        streams: stream_count,
        received_bytes: total_received,
        written_bytes: total_written,
        chunks,
        elapsed,
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
    })
}

struct UdpWalSocketState {
    fd: i32,
    scheduled: usize,
    chunks_started: usize,
}

fn udp_wal_queue_recv_for_slot(
    ring: &mut RawRing,
    buffers: &FixedSendBuffers,
    sockets: &mut [UdpWalSocketState],
    slots: &mut [TcpWalSlot],
    slot: usize,
    datagrams_per_socket: usize,
    datagram_bytes: usize,
    chunk_bytes: usize,
) -> io::Result<()> {
    let filled = slots[slot].filled;
    if filled >= chunk_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "UDP WAL recv slot is already full",
        ));
    }
    let socket_idx = slots[slot].conn;
    if sockets[socket_idx].scheduled >= datagrams_per_socket {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "UDP WAL socket has no datagrams left to schedule for a partial WAL chunk",
        ));
    }
    let len = datagram_bytes.min(chunk_bytes - filled);
    let len_u32 = u32::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "UDP datagram bytes must fit in u32",
        )
    })?;
    let buf = unsafe { buffers.ptr(slot).add(filled) };
    ring.queue_recv(
        sockets[socket_idx].fd,
        buf,
        len_u32,
        0,
        tcp_wal_recv_user_data(slot),
    )?;
    sockets[socket_idx].scheduled += 1;
    Ok(())
}

fn udp_wal_schedule_recvs(
    ring: &mut RawRing,
    buffers: &FixedSendBuffers,
    sockets: &mut [UdpWalSocketState],
    slots: &mut [TcpWalSlot],
    free_slots: &mut Vec<usize>,
    next_socket: &mut usize,
    datagrams_per_socket: usize,
    chunks_per_socket: usize,
    datagram_bytes: usize,
    chunk_bytes: usize,
) -> io::Result<usize> {
    if sockets.is_empty() {
        return Ok(0);
    }

    let mut scheduled = 0usize;
    while let Some(slot) = free_slots.pop() {
        let mut socket_idx = None;
        for _ in 0..sockets.len() {
            let candidate = *next_socket;
            *next_socket = (*next_socket + 1) % sockets.len();
            if sockets[candidate].chunks_started < chunks_per_socket {
                socket_idx = Some(candidate);
                break;
            }
        }

        let Some(socket_idx) = socket_idx else {
            free_slots.push(slot);
            break;
        };

        slots[slot] = TcpWalSlot {
            stage: TcpWalSlotStage::Recv,
            conn: socket_idx,
            filled: 0,
            file_offset: 0,
        };
        sockets[socket_idx].chunks_started += 1;
        udp_wal_queue_recv_for_slot(
            ring,
            buffers,
            sockets,
            slots,
            slot,
            datagrams_per_socket,
            datagram_bytes,
            chunk_bytes,
        )?;
        scheduled += 1;
    }

    Ok(scheduled)
}

#[allow(clippy::too_many_arguments)]
fn udp_wal_worker(
    target: SlotWalTarget,
    worker: usize,
    base_offset: u64,
    sockets: Vec<UdpSocket>,
    bytes_per_socket: usize,
    chunk_bytes: usize,
    datagram_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    buffer_mode: SlotWalBufferMode,
    pin: bool,
) -> io::Result<TcpWalWorkerResult> {
    let affinity = pin_current_thread_if_requested("udp-wal-worker", worker, pin);
    let socket_count = sockets.len();
    if socket_count == 0 {
        return Ok(TcpWalWorkerResult {
            worker,
            streams: 0,
            received_bytes: 0,
            written_bytes: 0,
            chunks: 0,
            elapsed: Duration::ZERO,
            target_cpu: affinity.target_cpu,
            affinity_applied: affinity.applied,
        });
    }

    let chunk_bytes_u32 = u32::try_from(chunk_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk-bytes must fit in u32 for UDP WAL SQEs",
        )
    })?;
    if datagram_bytes == 0 || datagram_bytes > 65_507 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "URING_PLAY_UDP_DATAGRAM_BYTES must be between 1 and 65507",
        ));
    }
    if chunk_bytes % datagram_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "UDP WAL chunk-bytes must be a multiple of URING_PLAY_UDP_DATAGRAM_BYTES",
        ));
    }
    if bytes_per_socket % datagram_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "UDP WAL bytes per socket must be a multiple of URING_PLAY_UDP_DATAGRAM_BYTES",
        ));
    }
    if bytes_per_socket % chunk_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "UDP WAL bytes per socket must be a multiple of chunk-bytes",
        ));
    }
    let datagrams_per_socket = bytes_per_socket / datagram_bytes;
    let chunks_per_socket = bytes_per_socket / chunk_bytes;
    let total_chunks = socket_count.checked_mul(chunks_per_socket).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "UDP WAL chunk count overflow")
    })?;
    let total_target = total_chunks.checked_mul(chunk_bytes).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "UDP WAL byte count overflow")
    })?;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(target.open_path())?;
    let fd = file.as_raw_fd();

    let mut ring = RawRing::new(ring_entries, ring_entries.saturating_mul(2))?;
    ring.register_napi_from_env(&format!("udp-wal-worker-{worker}"))?;
    let mut fds = [fd];
    ring.register_files(&mut fds)?;

    let buffers = buffer_mode.allocate(pipeline, chunk_bytes)?;
    let mut iovecs = buffers.iovecs(chunk_bytes);
    ring.register_buffers(&mut iovecs)?;

    let mut slot_ids = Vec::with_capacity(pipeline);
    for buf_index in 0..pipeline {
        slot_ids.push(ring.register_io_slot(buf_index as u32, 0)?);
    }

    let mut socket_states = sockets
        .iter()
        .map(|socket| UdpWalSocketState {
            fd: socket.as_raw_fd(),
            scheduled: 0,
            chunks_started: 0,
        })
        .collect::<Vec<_>>();
    let mut slots = (0..pipeline)
        .map(|_| TcpWalSlot::default())
        .collect::<Vec<_>>();
    let mut free_slots = (0..pipeline).rev().collect::<Vec<_>>();
    let mut next_socket = 0usize;
    let mut outstanding = 0usize;
    let mut total_received = 0usize;
    let mut total_written = 0usize;
    let mut chunks = 0usize;
    let mut next_wal_offset = base_offset;
    let mut first_activity = None::<Instant>;

    while total_written < total_target {
        outstanding += udp_wal_schedule_recvs(
            &mut ring,
            &buffers,
            &mut socket_states,
            &mut slots,
            &mut free_slots,
            &mut next_socket,
            datagrams_per_socket,
            chunks_per_socket,
            datagram_bytes,
            chunk_bytes,
        )?;
        if outstanding == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "UDP WAL pipeline has no outstanding receive or write operations",
            ));
        }

        let cqe = ring.wait_cqe()?;
        outstanding = outstanding.saturating_sub(1);
        let (kind, slot) = tcp_wal_decode_user_data(cqe.user_data)?;
        if slot >= slots.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("UDP WAL CQE returned invalid slot {slot}"),
            ));
        }

        match kind {
            TcpWalCqeKind::Recv => {
                if slots[slot].stage != TcpWalSlotStage::Recv {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("UDP WAL recv CQE for slot {slot} not in recv state"),
                    ));
                }
                if cqe.res < 0 {
                    return Err(io::Error::from_raw_os_error(-cqe.res));
                }
                let expected = datagram_bytes.min(chunk_bytes - slots[slot].filled);
                if cqe.res != expected as i32 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "UDP WAL datagram size mismatch: res={} expected={expected}",
                            cqe.res
                        ),
                    ));
                }

                let n = cqe.res as usize;
                if first_activity.is_none() {
                    first_activity = Some(Instant::now());
                }
                slots[slot].filled = slots[slot].filled.checked_add(n).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "UDP WAL slot fill overflow")
                })?;
                total_received = total_received.checked_add(n).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "UDP WAL receive byte overflow")
                })?;
                if slots[slot].filled < chunk_bytes {
                    udp_wal_queue_recv_for_slot(
                        &mut ring,
                        &buffers,
                        &mut socket_states,
                        &mut slots,
                        slot,
                        datagrams_per_socket,
                        datagram_bytes,
                        chunk_bytes,
                    )?;
                    outstanding += 1;
                    continue;
                }
                let file_offset = next_wal_offset;
                next_wal_offset =
                    next_wal_offset
                        .checked_add(chunk_bytes as u64)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "WAL offset overflow")
                        })?;
                slots[slot].stage = TcpWalSlotStage::Write;
                slots[slot].file_offset = file_offset;
                ring.queue_slot_rw(
                    slot_ids[slot],
                    0,
                    file_offset,
                    chunk_bytes_u32,
                    io_slots::SlotRw::Write,
                    tcp_wal_write_user_data(slot),
                )?;
                outstanding += 1;
            }
            TcpWalCqeKind::Write => {
                if slots[slot].stage != TcpWalSlotStage::Write {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("UDP WAL write CQE for slot {slot} not in write state"),
                    ));
                }
                if cqe.res < 0 {
                    return Err(io::Error::from_raw_os_error(-cqe.res));
                }
                if cqe.res != chunk_bytes_u32 as i32 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        format!(
                            "short UDP WAL write completion: res={} expected={chunk_bytes}",
                            cqe.res
                        ),
                    ));
                }
                total_written = total_written.checked_add(chunk_bytes).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "UDP WAL write byte overflow")
                })?;
                chunks += 1;
                slots[slot] = TcpWalSlot::default();
                free_slots.push(slot);
            }
        }
    }

    let elapsed = first_activity
        .map(|started| started.elapsed())
        .unwrap_or(Duration::ZERO);
    for slot_id in slot_ids {
        ring.unregister_io_slot(slot_id)?;
    }
    ring.unregister_buffers()?;
    ring.unregister_files()?;

    Ok(TcpWalWorkerResult {
        worker,
        streams: socket_count,
        received_bytes: total_received,
        written_bytes: total_written,
        chunks,
        elapsed,
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
    })
}

fn tcp_wal_split_queue_recv_for_slot(
    ring: &mut RawRing,
    buffers: FixedBufferView,
    conns: &[TcpWalConn],
    slots: &mut [TcpWalSlot],
    slot: usize,
    chunk_bytes: usize,
    chunk_bytes_u32: u32,
) -> io::Result<()> {
    let filled = slots[slot].filled;
    if filled >= chunk_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "split TCP WAL recv slot is already full",
        ));
    }

    let len = chunk_bytes - filled;
    let buf = unsafe { buffers.ptr(slot).add(filled) };
    let conn = &conns[slots[slot].conn];
    let len = len.min(chunk_bytes_u32 as usize) as u32;
    if conn.fixed_file {
        ring.queue_recv_fixed_file(conn.fd as u32, buf, len, 0, tcp_wal_recv_user_data(slot))
    } else {
        ring.queue_recv(conn.fd, buf, len, 0, tcp_wal_recv_user_data(slot))
    }
}

fn tcp_wal_split_schedule_recvs(
    ring: &mut RawRing,
    buffers: FixedBufferView,
    conns: &mut [TcpWalConn],
    slots: &mut [TcpWalSlot],
    free_slots: &mut Vec<usize>,
    next_conn: &mut usize,
    expected_bytes: usize,
    chunk_bytes: usize,
    chunk_bytes_u32: u32,
) -> io::Result<usize> {
    if conns.is_empty() {
        return Ok(0);
    }

    let mut scheduled = 0usize;
    while let Some(slot) = free_slots.pop() {
        let mut conn_idx = None;
        for _ in 0..conns.len() {
            let candidate = *next_conn;
            *next_conn = (*next_conn + 1) % conns.len();
            if conns[candidate].received < expected_bytes && !conns[candidate].recv_in_flight {
                conn_idx = Some(candidate);
                break;
            }
        }

        let Some(conn_idx) = conn_idx else {
            free_slots.push(slot);
            break;
        };

        slots[slot] = TcpWalSlot {
            stage: TcpWalSlotStage::Recv,
            conn: conn_idx,
            filled: 0,
            file_offset: 0,
        };
        conns[conn_idx].recv_in_flight = true;
        tcp_wal_split_queue_recv_for_slot(
            ring,
            buffers,
            conns,
            slots,
            slot,
            chunk_bytes,
            chunk_bytes_u32,
        )?;
        scheduled += 1;
    }

    Ok(scheduled)
}

fn tcp_wal_split_recv_returned_slots(
    return_rx: &mpsc::Receiver<usize>,
    free_slots: &mut Vec<usize>,
    pending_writes: &mut usize,
) -> io::Result<usize> {
    let mut returned = 0usize;
    loop {
        match return_rx.try_recv() {
            Ok(slot) => {
                *pending_writes = pending_writes.checked_sub(1).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "split TCP WAL returned more buffers than pending writes",
                    )
                })?;
                free_slots.push(slot);
                returned += 1;
            }
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                if *pending_writes == 0 {
                    break;
                }
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "split TCP WAL writer return channel disconnected",
                ));
            }
        }
    }
    Ok(returned)
}

fn tcp_wal_split_wait_returned_slot(
    return_rx: &mpsc::Receiver<usize>,
    free_slots: &mut Vec<usize>,
    pending_writes: &mut usize,
) -> io::Result<()> {
    let slot = return_rx.recv().map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "split TCP WAL writer return channel disconnected",
        )
    })?;
    *pending_writes = pending_writes.checked_sub(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "split TCP WAL returned more buffers than pending writes",
        )
    })?;
    free_slots.push(slot);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn tcp_wal_split_rx_worker(
    worker: usize,
    slot_start: usize,
    slot_count: usize,
    streams: Vec<TcpStream>,
    buffers: FixedBufferView,
    write_senders: Vec<mpsc::Sender<TcpWalWriteDesc>>,
    return_rx: mpsc::Receiver<usize>,
    wal_allocator: Arc<AtomicU64>,
    wal_region_end: u64,
    bytes_per_connection: usize,
    chunk_bytes: usize,
    ring_entries: u32,
    fixed_recv: bool,
    pin: bool,
) -> io::Result<TcpWalSplitRxResult> {
    let affinity = pin_current_thread_if_requested("tcp-wal-rx-worker", worker, pin);
    let stream_count = streams.len();
    if stream_count == 0 {
        return Ok(TcpWalSplitRxResult {
            worker,
            streams: 0,
            received_bytes: 0,
            submitted_bytes: 0,
            chunks: 0,
            elapsed: Duration::ZERO,
            target_cpu: affinity.target_cpu,
            affinity_applied: affinity.applied,
        });
    }

    let chunk_bytes_u32 = u32::try_from(chunk_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk-bytes must fit in u32 for split TCP WAL receive",
        )
    })?;
    let mut ring = RawRing::new(ring_entries, ring_entries.saturating_mul(2))?;
    ring.register_napi_from_env(&format!("tcp-wal-rx-worker-{worker}"))?;
    let mut registered_fds = if fixed_recv {
        streams
            .iter()
            .map(|stream| stream.as_raw_fd())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if fixed_recv {
        ring.register_files(&mut registered_fds)?;
    }
    let mut conns = streams
        .iter()
        .enumerate()
        .map(|(index, stream)| {
            let (fd, fixed_file) = if fixed_recv {
                (fixed_file_index_i32(index, "split TCP WAL receive")?, true)
            } else {
                (stream.as_raw_fd(), false)
            };
            Ok(TcpWalConn {
                fd,
                fixed_file,
                received: 0,
                recv_in_flight: false,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut slots = (0..buffers.count)
        .map(|_| TcpWalSlot::default())
        .collect::<Vec<_>>();
    let mut free_slots = (slot_start..slot_start + slot_count)
        .rev()
        .collect::<Vec<_>>();
    let mut next_conn = 0usize;
    let mut outstanding_recvs = 0usize;
    let mut pending_writes = 0usize;
    let mut total_received = 0usize;
    let mut total_submitted = 0usize;
    let mut chunks = 0usize;
    let total_target = stream_count
        .checked_mul(bytes_per_connection)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "split TCP WAL receive target overflows",
            )
        })?;
    let started = Instant::now();

    while total_submitted < total_target || pending_writes > 0 || outstanding_recvs > 0 {
        tcp_wal_split_recv_returned_slots(&return_rx, &mut free_slots, &mut pending_writes)?;

        if total_submitted < total_target {
            outstanding_recvs += tcp_wal_split_schedule_recvs(
                &mut ring,
                buffers,
                &mut conns,
                &mut slots,
                &mut free_slots,
                &mut next_conn,
                bytes_per_connection,
                chunk_bytes,
                chunk_bytes_u32,
            )?;
        }

        if outstanding_recvs == 0 {
            if pending_writes > 0 {
                tcp_wal_split_wait_returned_slot(&return_rx, &mut free_slots, &mut pending_writes)?;
                continue;
            }
            if total_submitted >= total_target {
                break;
            }
        }

        let cqe = ring.wait_cqe()?;
        outstanding_recvs = outstanding_recvs.saturating_sub(1);
        let (kind, slot) = tcp_wal_decode_user_data(cqe.user_data)?;
        if kind != TcpWalCqeKind::Recv {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("split TCP WAL RX worker got non-recv CQE for slot {slot}"),
            ));
        }
        if slot >= slots.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("split TCP WAL recv CQE returned invalid slot {slot}"),
            ));
        }
        if slots[slot].stage != TcpWalSlotStage::Recv {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("split TCP WAL recv CQE for slot {slot} not in recv state"),
            ));
        }
        if cqe.res < 0 {
            return Err(io::Error::from_raw_os_error(-cqe.res));
        }
        if cqe.res == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "split TCP WAL connection {} ended after {}/{} bytes",
                    slots[slot].conn, conns[slots[slot].conn].received, bytes_per_connection
                ),
            ));
        }

        let n = cqe.res as usize;
        let conn_idx = slots[slot].conn;
        slots[slot].filled = slots[slot].filled.checked_add(n).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "split TCP WAL slot fill overflow",
            )
        })?;
        conns[conn_idx].received = conns[conn_idx].received.checked_add(n).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "split TCP WAL receive overflow")
        })?;
        total_received = total_received.checked_add(n).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "split TCP WAL total receive overflow",
            )
        })?;

        if slots[slot].filled > chunk_bytes || conns[conn_idx].received > bytes_per_connection {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "split TCP WAL received too much data: conn={} conn_bytes={}/{} slot_filled={}/{}",
                    conn_idx,
                    conns[conn_idx].received,
                    bytes_per_connection,
                    slots[slot].filled,
                    chunk_bytes
                ),
            ));
        }

        if slots[slot].filled < chunk_bytes {
            tcp_wal_split_queue_recv_for_slot(
                &mut ring,
                buffers,
                &conns,
                &mut slots,
                slot,
                chunk_bytes,
                chunk_bytes_u32,
            )?;
            outstanding_recvs += 1;
            continue;
        }

        conns[conn_idx].recv_in_flight = false;
        let file_offset = wal_allocator.fetch_add(chunk_bytes as u64, Ordering::Relaxed);
        let write_end = file_offset
            .checked_add(chunk_bytes as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WAL offset overflow"))?;
        if write_end > wal_region_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "split TCP WAL write [{}..{}) exceeds configured WAL region end {}",
                    file_offset, write_end, wal_region_end
                ),
            ));
        }

        let writer = ((file_offset / chunk_bytes as u64) as usize) % write_senders.len();
        write_senders[writer]
            .send(TcpWalWriteDesc {
                rx_worker: worker,
                buffer_index: slot,
                file_offset,
                len: chunk_bytes_u32,
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "split TCP WAL writer channel disconnected",
                )
            })?;
        slots[slot] = TcpWalSlot::default();
        pending_writes += 1;
        total_submitted = total_submitted.checked_add(chunk_bytes).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "split TCP WAL submitted byte count overflow",
            )
        })?;
        chunks += 1;
    }

    if fixed_recv {
        ring.unregister_files()?;
    }

    Ok(TcpWalSplitRxResult {
        worker,
        streams: stream_count,
        received_bytes: total_received,
        submitted_bytes: total_submitted,
        chunks,
        elapsed: started.elapsed(),
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
    })
}

#[allow(clippy::too_many_arguments)]
fn tcp_wal_split_queue_write_for_buffer(
    ring: &mut RawRing,
    fd: i32,
    slot_ids: &[io_slots::IoSlotId],
    buffers: FixedBufferView,
    buffer_index: usize,
    file_offset: u64,
    len: u32,
    write_mode: TcpWalWriteMode,
    user_data: u64,
) -> io::Result<()> {
    match write_mode {
        TcpWalWriteMode::Null => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "null split TCP WAL write mode does not queue block writes",
        )),
        TcpWalWriteMode::Slot => ring.queue_slot_rw(
            slot_ids[buffer_index],
            0,
            file_offset,
            len,
            io_slots::SlotRw::Write,
            user_data,
        ),
        TcpWalWriteMode::Write => ring.queue_write(
            fd,
            buffers.ptr(buffer_index).cast_const(),
            len,
            file_offset,
            user_data,
        ),
        TcpWalWriteMode::WriteFixed => ring.queue_write_fixed(
            fd,
            buffers.ptr(buffer_index).cast_const(),
            len,
            file_offset,
            buffer_index as u16,
            user_data,
        ),
        TcpWalWriteMode::WriteFixedFile => ring.queue_write_fixed_file(
            0,
            buffers.ptr(buffer_index).cast_const(),
            len,
            file_offset,
            buffer_index as u16,
            user_data,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn tcp_wal_split_writer_worker(
    target: SlotWalTarget,
    writer: usize,
    affinity_index: usize,
    buffers: FixedBufferView,
    return_senders: Vec<mpsc::Sender<usize>>,
    receiver: mpsc::Receiver<TcpWalWriteDesc>,
    chunk_bytes: usize,
    ring_entries: u32,
    write_mode: TcpWalWriteMode,
    pin: bool,
) -> io::Result<TcpWalSplitWriterResult> {
    let affinity = pin_current_thread_if_requested("tcp-wal-writer", affinity_index, pin);
    let chunk_bytes_u32 = u32::try_from(chunk_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk-bytes must fit in u32 for split TCP WAL writes",
        )
    })?;

    if write_mode.is_null() {
        let started = Instant::now();
        let mut written_bytes = 0usize;
        let mut chunks = 0usize;
        while let Ok(desc) = receiver.recv() {
            if desc.buffer_index >= buffers.count {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "split TCP WAL null writer got invalid buffer index {}",
                        desc.buffer_index
                    ),
                ));
            }
            if desc.len != chunk_bytes_u32 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "split TCP WAL null writer got len={} expected={chunk_bytes}",
                        desc.len
                    ),
                ));
            }
            written_bytes = written_bytes
                .checked_add(desc.len as usize)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "split TCP WAL null writer byte count overflow",
                    )
                })?;
            chunks += 1;
            return_senders[desc.rx_worker]
                .send(desc.buffer_index)
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "split TCP WAL RX return channel disconnected",
                    )
                })?;
        }
        return Ok(TcpWalSplitWriterResult {
            writer,
            written_bytes,
            chunks,
            elapsed: started.elapsed(),
            target_cpu: affinity.target_cpu,
            affinity_applied: affinity.applied,
        });
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(target.open_path())?;
    let fd = file.as_raw_fd();

    let writer_ring_entries = ring_entries.max(buffers.count as u32).max(64);
    let mut ring = RawRing::new(writer_ring_entries, writer_ring_entries.saturating_mul(2))?;
    let mut fds = [fd];
    let registered_files = write_mode.needs_registered_files();
    if registered_files {
        ring.register_files(&mut fds)?;
    }
    let mut iovecs = buffers.iovecs(chunk_bytes);
    let registered_buffers = write_mode.needs_registered_buffers();
    if registered_buffers {
        ring.register_buffers(&mut iovecs)?;
    }

    let mut slot_ids = Vec::with_capacity(buffers.count);
    if write_mode.needs_io_slots() {
        for buf_index in 0..buffers.count {
            slot_ids.push(ring.register_io_slot(buf_index as u32, 0)?);
        }
    }

    let mut inflight = (0..buffers.count).map(|_| None).collect::<Vec<_>>();
    let mut inflight_count = 0usize;
    let mut receive_open = true;
    let mut written_bytes = 0usize;
    let mut chunks = 0usize;
    let started = Instant::now();

    while receive_open || inflight_count > 0 {
        while receive_open {
            match receiver.try_recv() {
                Ok(desc) => {
                    if desc.buffer_index >= buffers.count {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "split TCP WAL writer got invalid buffer index {}",
                                desc.buffer_index
                            ),
                        ));
                    }
                    if desc.len != chunk_bytes_u32 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "split TCP WAL writer got len={} expected={chunk_bytes}",
                                desc.len
                            ),
                        ));
                    }
                    if inflight[desc.buffer_index].is_some() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "split TCP WAL writer got duplicate buffer {} while write is pending",
                                desc.buffer_index
                            ),
                        ));
                    }
                    tcp_wal_split_queue_write_for_buffer(
                        &mut ring,
                        fd,
                        &slot_ids,
                        buffers,
                        desc.buffer_index,
                        desc.file_offset,
                        desc.len,
                        write_mode,
                        desc.buffer_index as u64,
                    )?;
                    inflight[desc.buffer_index] = Some(desc);
                    inflight_count += 1;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    receive_open = false;
                    break;
                }
            }
        }

        if inflight_count == 0 {
            if !receive_open {
                break;
            }
            match receiver.recv() {
                Ok(desc) => {
                    if desc.buffer_index >= buffers.count {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "split TCP WAL writer got invalid buffer index {}",
                                desc.buffer_index
                            ),
                        ));
                    }
                    tcp_wal_split_queue_write_for_buffer(
                        &mut ring,
                        fd,
                        &slot_ids,
                        buffers,
                        desc.buffer_index,
                        desc.file_offset,
                        desc.len,
                        write_mode,
                        desc.buffer_index as u64,
                    )?;
                    inflight[desc.buffer_index] = Some(desc);
                    inflight_count += 1;
                }
                Err(_) => {
                    receive_open = false;
                    continue;
                }
            }
        }

        let cqe = ring.wait_cqe()?;
        let buffer_index = cqe.user_data as usize;
        if buffer_index >= inflight.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("split TCP WAL writer CQE invalid buffer index {buffer_index}"),
            ));
        }
        let desc = inflight[buffer_index].take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("split TCP WAL writer duplicate CQE for buffer {buffer_index}"),
            )
        })?;
        inflight_count = inflight_count.saturating_sub(1);
        if cqe.res < 0 {
            return Err(io::Error::from_raw_os_error(-cqe.res));
        }
        if cqe.res != desc.len as i32 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!(
                    "short split TCP WAL write completion: res={} expected={}",
                    cqe.res, desc.len
                ),
            ));
        }
        written_bytes = written_bytes
            .checked_add(desc.len as usize)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "split TCP WAL writer byte count overflow",
                )
            })?;
        chunks += 1;
        return_senders[desc.rx_worker]
            .send(desc.buffer_index)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "split TCP WAL RX return channel disconnected",
                )
            })?;
    }

    for slot_id in slot_ids {
        ring.unregister_io_slot(slot_id)?;
    }
    if registered_buffers {
        ring.unregister_buffers()?;
    }
    if registered_files {
        ring.unregister_files()?;
    }

    Ok(TcpWalSplitWriterResult {
        writer,
        written_bytes,
        chunks,
        elapsed: started.elapsed(),
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
    })
}

fn recycle_send_slot(
    op_slots: &mut [UringSendSlot],
    free_slots: &mut Vec<usize>,
    conns: &mut [UringSendConn],
    slot: usize,
) {
    if let Some(conn) = op_slots[slot].conn.take() {
        conns[conn].in_flight = conns[conn].in_flight.saturating_sub(1);
    }
    op_slots[slot].op = None;
    op_slots[slot].zc_notif_expected = false;
    op_slots[slot].zc_notif_done = false;
    free_slots.push(slot);
}

fn handle_send_zc_notification(
    op_slots: &mut [UringSendSlot],
    free_slots: &mut Vec<usize>,
    conns: &mut [UringSendConn],
    pending_zc_notifications: &mut usize,
    user_data: u64,
) -> io::Result<()> {
    let slot = user_data as usize;
    if slot >= op_slots.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected zerocopy notification user_data={user_data}"),
        ));
    }

    let state = &mut op_slots[slot];
    if state.zc_notif_done {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("duplicate zerocopy notification user_data={user_data}"),
        ));
    }

    state.zc_notif_done = true;
    if state.zc_notif_expected {
        *pending_zc_notifications = pending_zc_notifications.saturating_sub(1);
        if state.op.is_none() {
            recycle_send_slot(op_slots, free_slots, conns, slot);
        }
    }

    Ok(())
}

fn uring_send_worker(
    worker: usize,
    affinity: ThreadAffinity,
    streams: Vec<TcpStream>,
    bytes_per_connection: usize,
    chunk_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    send_mode: UringSendMode,
    payload_pattern: SendPayloadPattern,
    fixed_file_send: bool,
) -> io::Result<UringSendStats> {
    let tid = current_tid();
    let start_wall = Instant::now();
    let start_thread_cpu = thread_cpu_time().unwrap_or_default();
    let start_cpu = current_cpu();
    let start_switches = read_thread_context_switches(tid).unwrap_or_default();
    let stream_count = streams.len();

    if streams.is_empty() || bytes_per_connection == 0 {
        let end_thread_cpu = thread_cpu_time().unwrap_or(start_thread_cpu);
        let end_cpu = current_cpu();
        let end_switches = read_thread_context_switches(tid).unwrap_or(start_switches);
        return Ok(UringSendStats {
            worker,
            streams: stream_count,
            wall: start_wall.elapsed(),
            cpu: end_thread_cpu.saturating_sub(start_thread_cpu),
            target_cpu: affinity.target_cpu,
            affinity_applied: affinity.applied,
            start_cpu,
            end_cpu,
            voluntary_switches: end_switches
                .voluntary
                .saturating_sub(start_switches.voluntary),
            involuntary_switches: end_switches
                .involuntary
                .saturating_sub(start_switches.involuntary),
            migrations: end_switches
                .migrations
                .saturating_sub(start_switches.migrations),
            ..UringSendStats::default()
        });
    }

    let chunk_bytes = tcp_bench_u32_len(chunk_bytes.max(4096), "chunk bytes")?;
    let pipeline = pipeline.max(1);
    let ring_entries = ring_entries.max(
        (streams.len() as u32)
            .saturating_mul(pipeline as u32)
            .saturating_mul(2)
            .max(64),
    );
    let mut ring = RawRing::new(ring_entries, ring_entries.saturating_mul(2))?;
    ring.register_napi_from_env(&format!("uring-send-worker-{worker}"))?;
    let mut registered_fds = if fixed_file_send {
        streams
            .iter()
            .map(|stream| stream.as_raw_fd())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if fixed_file_send {
        ring.register_files(&mut registered_fds)?;
    }
    let chunk_len = chunk_bytes as usize;
    payload_pattern.validate(bytes_per_connection, chunk_len)?;
    let mut shared_buf = vec![0u8; chunk_len];
    payload_pattern.fill(&mut shared_buf);
    let max_in_flight = streams.len().saturating_mul(pipeline);
    if send_mode.uses_fixed_buffer() && max_in_flight > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("send-zc-fixed needs {max_in_flight} registered buffers, max is 65535"),
        ));
    }

    let mut fixed_buffers = None;
    let mut registered_buffers = false;
    if send_mode.uses_fixed_buffer() {
        fixed_buffers = Some(FixedSendBuffers::new(max_in_flight, chunk_len)?);
        fixed_buffers
            .as_ref()
            .expect("fixed buffers were just allocated")
            .fill_each(chunk_len, |buf| payload_pattern.fill(buf));
        let mut iovecs = fixed_buffers
            .as_ref()
            .expect("fixed buffers were just allocated")
            .iovecs(chunk_len);
        ring.register_buffers(&mut iovecs)?;
        registered_buffers = true;
    }
    let mut conns = streams
        .iter()
        .enumerate()
        .map(|(index, stream)| {
            let (fd, fixed_file) = if fixed_file_send {
                (fixed_file_index_i32(index, "uring send")?, true)
            } else {
                (stream.as_raw_fd(), false)
            };
            Ok(UringSendConn {
                fd,
                fixed_file,
                scheduled: 0,
                completed: 0,
                in_flight: 0,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut op_slots = Vec::<UringSendSlot>::new();
    let mut free_slots = Vec::<usize>::new();
    let total_target = streams.len() * bytes_per_connection;
    let mut total_completed = 0usize;
    let mut total_in_flight = 0usize;
    let mut pending_zc_notifications = 0usize;
    let mut zc_notifications = 0usize;
    let mut zc_copied_notifications = 0usize;

    while total_completed < total_target {
        'fill: loop {
            let mut progressed = false;
            for idx in 0..conns.len() {
                while conns[idx].in_flight < pipeline && conns[idx].scheduled < bytes_per_connection
                {
                    let len = (bytes_per_connection - conns[idx].scheduled).min(chunk_len);
                    let slot = free_slots.pop().unwrap_or_else(|| {
                        op_slots.push(UringSendSlot {
                            op: None,
                            conn: None,
                            zc_notif_expected: false,
                            zc_notif_done: false,
                        });
                        op_slots.len() - 1
                    });
                    op_slots[slot] = UringSendSlot {
                        op: Some(UringSendOp {
                            conn: idx,
                            len,
                            zc: send_mode.uses_zc(),
                        }),
                        conn: None,
                        zc_notif_expected: false,
                        zc_notif_done: false,
                    };
                    let buf_ptr = if send_mode.uses_fixed_buffer() {
                        fixed_buffers
                            .as_ref()
                            .expect("fixed send mode has fixed buffers")
                            .ptr(slot)
                            .cast_const()
                    } else {
                        shared_buf.as_ptr()
                    };
                    let queue_result = if send_mode.uses_zc() {
                        if conns[idx].fixed_file {
                            ring.queue_send_zc_fixed_file(
                                conns[idx].fd as u32,
                                buf_ptr,
                                len as u32,
                                0,
                                send_mode.uses_fixed_buffer().then_some(slot as u16),
                                true,
                                slot as u64,
                            )
                        } else {
                            ring.queue_send_zc(
                                conns[idx].fd,
                                buf_ptr,
                                len as u32,
                                0,
                                send_mode.uses_fixed_buffer().then_some(slot as u16),
                                true,
                                slot as u64,
                            )
                        }
                    } else if conns[idx].fixed_file {
                        ring.queue_send_fixed_file(
                            conns[idx].fd as u32,
                            buf_ptr,
                            len as u32,
                            0,
                            slot as u64,
                        )
                    } else {
                        ring.queue_send(conns[idx].fd, buf_ptr, len as u32, 0, slot as u64)
                    };
                    match queue_result {
                        Ok(()) => {
                            conns[idx].scheduled += len;
                            conns[idx].in_flight += 1;
                            op_slots[slot].conn = Some(idx);
                            total_in_flight += 1;
                            progressed = true;
                        }
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            recycle_send_slot(&mut op_slots, &mut free_slots, &mut conns, slot);
                            break 'fill;
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
            if !progressed {
                break;
            }
        }

        ring.submit_pending()?;
        if total_in_flight == 0 && pending_zc_notifications == 0 {
            break;
        }

        let cqe = ring.wait_cqe()?;
        if (cqe.flags & IORING_CQE_F_NOTIF) != 0 {
            zc_notifications += 1;
            if (cqe.res & IORING_NOTIF_USAGE_ZC_COPIED) != 0 {
                zc_copied_notifications += 1;
            }
            handle_send_zc_notification(
                &mut op_slots,
                &mut free_slots,
                &mut conns,
                &mut pending_zc_notifications,
                cqe.user_data,
            )?;
            continue;
        }

        let slot = cqe.user_data as usize;
        if slot >= op_slots.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected send CQE user_data={}", cqe.user_data),
            ));
        }
        let op = op_slots[slot].op.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate send CQE user_data={}", cqe.user_data),
            )
        })?;
        if cqe.res < 0 {
            return Err(io::Error::from_raw_os_error(-cqe.res));
        }
        if cqe.res == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "send returned 0"));
        }

        let sent = cqe.res as usize;
        if sent > op.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("send completed {sent} bytes for {} byte op", op.len),
            ));
        }

        conns[op.conn].completed += sent;
        total_completed += sent;
        total_in_flight -= 1;
        if sent < op.len {
            conns[op.conn].scheduled -= op.len - sent;
        }

        let notif_expected = op.zc && (cqe.flags & IORING_CQE_F_MORE) != 0;
        if notif_expected {
            op_slots[slot].zc_notif_expected = true;
            if !op_slots[slot].zc_notif_done {
                pending_zc_notifications += 1;
            }
        }
        if !notif_expected || op_slots[slot].zc_notif_done {
            recycle_send_slot(&mut op_slots, &mut free_slots, &mut conns, slot);
        }
    }

    while pending_zc_notifications > 0 {
        let cqe = ring.wait_cqe()?;
        if (cqe.flags & IORING_CQE_F_NOTIF) == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "expected zerocopy notification, got user_data={} res={} flags=0x{:x}",
                    cqe.user_data, cqe.res, cqe.flags
                ),
            ));
        }
        zc_notifications += 1;
        if (cqe.res & IORING_NOTIF_USAGE_ZC_COPIED) != 0 {
            zc_copied_notifications += 1;
        }
        handle_send_zc_notification(
            &mut op_slots,
            &mut free_slots,
            &mut conns,
            &mut pending_zc_notifications,
            cqe.user_data,
        )?;
    }
    if registered_buffers {
        ring.unregister_buffers()?;
    }
    if fixed_file_send {
        ring.unregister_files()?;
    }

    for stream in streams {
        stream.shutdown(Shutdown::Write)?;
    }
    let end_thread_cpu = thread_cpu_time().unwrap_or(start_thread_cpu);
    let end_cpu = current_cpu();
    let end_switches = read_thread_context_switches(tid).unwrap_or(start_switches);
    Ok(UringSendStats {
        worker,
        streams: stream_count,
        bytes: total_completed,
        zc_notifications,
        zc_copied_notifications,
        wall: start_wall.elapsed(),
        cpu: end_thread_cpu.saturating_sub(start_thread_cpu),
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
        start_cpu,
        end_cpu,
        voluntary_switches: end_switches
            .voluntary
            .saturating_sub(start_switches.voluntary),
        involuntary_switches: end_switches
            .involuntary
            .saturating_sub(start_switches.involuntary),
        migrations: end_switches
            .migrations
            .saturating_sub(start_switches.migrations),
    })
}

fn tcp_bench_uring_mux_server(
    bind: &str,
    base_port: u16,
    ports: usize,
    connections_per_port: usize,
    expected_bytes: usize,
    workers: usize,
    recv_bytes: usize,
    ring_entries: u32,
) -> io::Result<()> {
    let total_connections = tcp_bench_total_connections(ports, connections_per_port)?;
    let selected_connections_per_port =
        tcp_bench_mux_selected_connections_per_port(connections_per_port)?;
    let selected_total_connections =
        tcp_bench_total_connections(ports, selected_connections_per_port)?;
    println!(
        "tcp-bench-uring-mux-server: listening on {bind}:{base_port}.. ports={ports} \
         connections_per_port={connections_per_port} total_connections={total_connections} \
         selected_connections_per_port={selected_connections_per_port} \
         selected_total_connections={selected_total_connections} expected_bytes={expected_bytes}"
    );
    let listeners = tcp_bench_mux_bind_listeners(bind, base_port, ports)?;
    let mut streams =
        tcp_bench_mux_accept_tagged_listeners(listeners, ports, connections_per_port)?;
    let workers = tcp_bench_auto_workers(workers, selected_total_connections);
    let shard_policy =
        TcpMuxShardPolicy::from_env_or("URING_PLAY_MUX_SHARD", TcpMuxShardPolicy::Observed)?;
    streams = tcp_bench_select_live_conn_indices_per_lane(
        streams,
        ports,
        connections_per_port,
        selected_connections_per_port,
        workers,
        shard_policy,
        "tcp-bench-uring-mux-server",
        false,
    )?;
    let start_handshake = start_handshake_enabled();
    let control_streams = if start_handshake {
        let control_streams = clone_tcp_bench_control_streams(&streams)?;
        send_tcp_control_byte(
            &control_streams,
            TCP_READY_BYTE,
            "tcp-bench-uring-mux-server",
            "start_handshake_ready",
        )?;
        recv_tcp_control_byte_from_accepted(
            &mut streams,
            TCP_ACK_BYTE,
            "tcp-bench-uring-mux-server",
            "start_handshake_ack",
        )?;
        control_streams
    } else {
        maybe_clone_ready_handshake_streams(&streams)?
    };
    let shards = tcp_bench_partition_accepted_streams(
        streams,
        workers,
        shard_policy,
        "tcp-bench-uring-mux-server",
    );
    let active_workers = shards.iter().filter(|shard| !shard.is_empty()).count();
    let sync_enabled = start_handshake || ready_handshake_enabled();
    let start_barrier = sync_enabled.then(|| Arc::new(Barrier::new(active_workers + 1)));
    let started_before_spawn = (!sync_enabled).then(Instant::now);
    let mut handles = Vec::with_capacity(workers);
    for (worker_idx, shard) in shards.into_iter().enumerate() {
        if shard.is_empty() {
            continue;
        }
        let worker_start_barrier = start_barrier.as_ref().map(Arc::clone);
        handles.push(thread::spawn(move || {
            let _affinity = maybe_pin_current_thread("uring-recv-worker", worker_idx);
            if let Some(barrier) = worker_start_barrier.as_ref() {
                barrier.wait();
            }
            uring_recv_worker(shard, expected_bytes, recv_bytes, ring_entries)
        }));
    }
    let started = if let Some(started) = started_before_spawn {
        started
    } else {
        println!(
            "tcp-bench-uring-mux-server: start_sync=waiting-for-workers active_workers={active_workers}"
        );
        let started = Instant::now();
        if let Some(barrier) = start_barrier.as_ref() {
            barrier.wait();
        }
        println!(
            "tcp-bench-uring-mux-server: start_sync=workers-released active_workers={active_workers}"
        );
        if start_handshake {
            send_tcp_control_byte(
                &control_streams,
                TCP_START_BYTE,
                "tcp-bench-uring-mux-server",
                "start_handshake_go",
            )?;
        } else {
            maybe_send_ready_handshake(&control_streams, "tcp-bench-uring-mux-server")?;
        }
        started
    };

    let mut total = 0usize;
    for handle in handles {
        total += handle.join().map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "tcp bench uring mux recv worker panicked",
            )
        })??;
    }
    print_tcp_bench_result("tcp-bench-uring-mux-server", total, started.elapsed());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn tcp_wal_zcrx_mux_server(
    target: SlotWalTarget,
    bind: &str,
    base_port: u16,
    ports: usize,
    connections_per_port: usize,
    bytes_per_connection: usize,
    chunk_bytes: usize,
    pipeline: usize,
    workers: usize,
    ring_entries: u32,
    required_alignment: usize,
    wal_region_base: usize,
    wal_region_end: u64,
    pin_workers: bool,
) -> io::Result<()> {
    let total_connections = tcp_bench_total_connections(ports, connections_per_port)?;
    let selected_connections_per_port =
        tcp_wal_zcrx_selected_connections_per_port(connections_per_port)?;
    let selected_total_connections =
        tcp_bench_total_connections(ports, selected_connections_per_port)?;
    if chunk_bytes == 0 || bytes_per_connection % chunk_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "ZCRX WAL coalescing currently requires bytes_per_connection={bytes_per_connection} \
                 to be a multiple of chunk_bytes={chunk_bytes}"
            ),
        ));
    }
    let workers = tcp_bench_auto_workers(workers, selected_total_connections);
    let ifname = env::var("URING_PLAY_ZCRX_IFNAME").unwrap_or_else(|_| "raft0".to_string());
    let rxq_base = env_usize_or("URING_PLAY_ZCRX_RXQ", 0);
    let rxq_count = env_usize_or("URING_PLAY_ZCRX_RXQ_COUNT", workers).max(1);
    if rxq_base > u32::MAX as usize || rxq_base + rxq_count > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "URING_PLAY_ZCRX_RXQ plus URING_PLAY_ZCRX_RXQ_COUNT exceeds u32",
        ));
    }
    let shard_policy = TcpMuxShardPolicy::from_env_or(
        "URING_PLAY_TCP_WAL_ZCRX_SHARD",
        TcpMuxShardPolicy::PortLane,
    )?;
    println!(
        "tcp-wal-mux-server-zcrx: target={} bind={bind} base_port={base_port} ports={ports} \
         connections_per_port={connections_per_port} total_connections={total_connections} \
         selected_connections_per_port={selected_connections_per_port} selected_total_connections={selected_total_connections} \
         bytes_per_connection={bytes_per_connection} chunk_bytes={chunk_bytes} \
         pipeline={pipeline} ring_entries={ring_entries} ifname={ifname} rxq_base={rxq_base} \
         rxq_count={rxq_count} shard_policy={} required_alignment={required_alignment} \
         wal_region_base={wal_region_base} wal_region_end={wal_region_end} \
         pin_workers={pin_workers} slot_backend={}",
        target.label(),
        shard_policy.label(),
        io_slots::submission_backend_label()
    );

    let target_metadata = fs::metadata(target.open_path())?;
    let block_topology = BlockDeviceTopology::from_metadata(&target_metadata)?;
    let avoid_smt = env_enabled_or("URING_PLAY_TCP_WAL_ZCRX_AVOID_SMT", true);
    println!(
        "tcp-wal-mux-server-zcrx-topology: block_sysfs={} disk_sysfs={} \
         nvme_queue_count={} device_numa_node={} pin_workers={} avoid_smt={} \
         pin_env={} pin_cpu_list={}",
        block_topology.block_dir.display(),
        block_topology.disk_dir.display(),
        block_topology.queues.len(),
        option_i32_label(block_topology.device_numa_node),
        pin_workers,
        yes(avoid_smt),
        env_truthy("URING_PLAY_PIN_CPUS"),
        env::var("URING_PLAY_PIN_CPU_LIST").unwrap_or_else(|_| "unset".to_string())
    );
    for queue in &block_topology.queues {
        println!(
            "tcp-wal-mux-server-zcrx-nvme-queue: queue={} cpus={}",
            queue.index,
            format_cpu_list(&queue.cpus)
        );
    }
    let planned_cpus = block_topology.planned_cpus(rxq_count, avoid_smt);
    let worker_cpu_plan = (0..rxq_count)
        .map(|worker| {
            let cpu = planned_cpus[worker];
            let queue = block_topology.queue_for_cpu(worker, cpu);
            let cpu_numa = cpu_numa_node(cpu);
            let numa_local = block_topology.device_numa_node.is_none()
                || cpu_numa.is_none()
                || block_topology.device_numa_node == cpu_numa;
            let smt_siblings = thread_siblings(cpu);
            let smt_conflict = cpu_has_smt_conflict(cpu, &planned_cpus[..worker]);
            (
                cpu,
                queue.index,
                format_cpu_list(&queue.cpus),
                cpu_numa,
                numa_local,
                format_cpu_list(&smt_siblings),
                smt_conflict,
            )
        })
        .collect::<Vec<_>>();
    for (
        worker,
        (cpu, queue_index, queue_cpus, cpu_numa, numa_local, smt_siblings, smt_conflict),
    ) in worker_cpu_plan.iter().enumerate()
    {
        println!(
            "tcp-wal-mux-server-zcrx-cpu-plan: worker={worker} rxq={} \
             cpu={} cpu_numa_node={} nvme_queue={} queue_cpus={} numa_local={} \
             smt_siblings={} smt_conflict={}",
            rxq_base + worker,
            cpu,
            option_i32_label(*cpu_numa),
            queue_index,
            queue_cpus,
            numa_local,
            smt_siblings,
            smt_conflict
        );
    }

    let listeners = tcp_bench_mux_bind_listeners(bind, base_port, ports)?;
    let (ready_tx, ready_rx) = mpsc::channel::<io::Result<u32>>();
    let mut stream_senders = Vec::with_capacity(rxq_count);
    let mut handles = Vec::with_capacity(rxq_count);
    for worker in 0..rxq_count {
        let target = target.clone();
        let ifname = ifname.clone();
        let planned_cpu = worker_cpu_plan[worker].0;
        let planned_nvme_queue = worker_cpu_plan[worker].1;
        let ready_tx = ready_tx.clone();
        let ready_tx_for_error = ready_tx.clone();
        let (streams_tx, streams_rx) = mpsc::channel::<TcpWalZcrxWorkerAssignment>();
        stream_senders.push(streams_tx);
        let rxq = (rxq_base + worker) as u32;
        handles.push(thread::spawn(move || {
            let result = tcp_wal_zcrx_worker(
                target,
                worker,
                Some(planned_cpu),
                planned_nvme_queue,
                ifname,
                rxq,
                streams_rx,
                ready_tx,
                bytes_per_connection,
                chunk_bytes,
                pipeline,
                ring_entries,
                required_alignment,
                pin_workers,
            );
            if let Err(err) = &result {
                eprintln!("tcp-wal-zcrx-worker-error: worker={worker} error={err}");
                let _ = ready_tx_for_error.send(Err(io::Error::new(err.kind(), err.to_string())));
            }
            result
        }));
    }
    drop(ready_tx);

    let mut ready_error = None;
    for _ in 0..rxq_count {
        match ready_rx.recv() {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => {
                ready_error = Some(err);
                break;
            }
            Err(_) => {
                ready_error = Some(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "ZCRX WAL worker registration channel closed",
                ));
                break;
            }
        }
    }
    if let Some(err) = ready_error {
        drop(stream_senders);
        let mut worker_error = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    if worker_error.is_none() {
                        worker_error = Some(err);
                    }
                }
                Err(_) => {
                    if worker_error.is_none() {
                        worker_error = Some(io::Error::new(
                            io::ErrorKind::Other,
                            "TCP WAL ZCRX worker thread panicked during registration",
                        ));
                    }
                }
            }
        }
        return Err(worker_error.unwrap_or(err));
    }
    println!("tcp-wal-mux-server-zcrx: registered all ZCRX WAL workers count={rxq_count}");

    let accepted = tcp_bench_mux_accept_tagged_listeners(listeners, ports, connections_per_port)?;
    let accepted = tcp_bench_select_live_conn_indices_per_lane(
        accepted,
        ports,
        connections_per_port,
        selected_connections_per_port,
        rxq_count,
        shard_policy,
        "tcp-wal-mux-server-zcrx",
        pin_workers,
    )?;
    let shards = tcp_wal_zcrx_partition_accepted_streams_with_pin(
        accepted,
        rxq_count,
        shard_policy,
        "tcp-wal-mux-server-zcrx",
        pin_workers,
    );
    let stream_counts = shards.iter().map(Vec::len).collect::<Vec<_>>();
    let wal_region_total_bytes = usize::try_from(
        wal_region_end
            .checked_sub(wal_region_base as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WAL region underflow"))?,
    )
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "WAL region too large"))?;
    let bytes_per_stream_region = wal_region_total_bytes
        .checked_div(selected_total_connections)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "zero TCP WAL connections"))?;
    ensure_aligned_to(
        bytes_per_stream_region,
        required_alignment,
        "per-stream ZCRX WAL region",
    )?;
    if bytes_per_stream_region < bytes_per_connection {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "per-stream ZCRX WAL region {bytes_per_stream_region} is smaller than \
                 logical stream length {bytes_per_connection}"
            ),
        ));
    }
    let wal_regions = make_observed_wal_regions(
        &stream_counts,
        bytes_per_stream_region,
        wal_region_base as u64,
        wal_region_end,
    )?;
    let started = Instant::now();
    for (idx, streams) in shards.into_iter().enumerate() {
        let region = wal_regions[idx];
        let region_end = region.end_offset()?;
        println!(
            "tcp-wal-mux-server-zcrx-region: worker={idx} streams={} \
             wal_region_base={} wal_region_end={} wal_region_bytes={}",
            streams.len(),
            region.base_offset,
            region_end,
            region.len_bytes
        );
        stream_senders[idx]
            .send(TcpWalZcrxWorkerAssignment {
                streams,
                wal_region_base: region.base_offset,
                wal_region_end: region_end,
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("ZCRX WAL worker {idx} exited before streams were sent"),
                )
            })?;
    }
    drop(stream_senders);

    let mut total_received = 0usize;
    let mut total_written = 0usize;
    let mut total_wal = 0usize;
    let mut total_direct_frames = 0usize;
    let mut total_direct_bytes = 0usize;
    let mut total_bounce_frames = 0usize;
    let mut total_bounce_bytes = 0usize;
    let mut total_coalesce_frames = 0usize;
    let mut total_coalesce_bytes = 0usize;
    let mut total_frames = 0usize;
    let mut total_chunks = 0usize;
    for handle in handles {
        let stats = handle.join().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "TCP WAL ZCRX worker thread panicked")
        })??;
        let elapsed_secs = stats.elapsed.as_secs_f64();
        let secs = elapsed_secs.max(f64::MIN_POSITIVE);
        let cpu_secs = stats.cpu.as_secs_f64();
        let cpu_pct = if elapsed_secs > 0.0 {
            (cpu_secs / elapsed_secs) * 100.0
        } else {
            0.0
        };
        println!(
            "tcp-wal-mux-server-zcrx-worker: worker={} rxq={} streams={} \
             nvme_queue={} \
             wal_region_base={} wal_region_end={} slot_stride={} fixed_buffers={} \
             zcrx_area_base=0x{:x} zcrx_area_bytes={} zcrx_area_alignment={} \
             zcrx_rx_buf_len={} memory_policy={} write_pipeline={} frame_pipeline={} \
             coalesce_batch_chunks={} coalesce_write_bytes={} \
             received_bytes={} written_bytes={} \
             wal_bytes={} direct_frames={} direct_bytes={} \
             bounce_frames={} bounce_bytes={} coalesce_frames={} coalesce_bytes={} \
             frames={} chunks={} \
             direct_enabled={} recv_multishot={} max_active_recvs={} recv_data_cqes={} recv_final_cqes={} \
             recv_empty_finals={} transient_recv_errors={} transient_recv_deferred={} \
             transient_recv_backoffs={} recv_flush_calls={} recv_flush_queued={} \
             pending_frame_requeues={} pending_frame_max={} \
             direct_attempts={} direct_stall_write_slots={} direct_stall_frame_slots={} \
             direct_stall_busy_slots={} direct_stall_sq_space={} direct_padded_frames={} \
             direct_padded_bytes={} direct_multisegment_frames={} direct_segment_writes={} \
             direct_fallback_to_bounce={} direct_slot_busy_guard={} \
             direct_busy_bounce={} direct_busy_bounce_attempts={} \
             direct_stall_coalesce={} direct_stall_coalesce_attempts={} \
             bounce_attempts={} bounce_stall_write_slots={} \
             bounce_stall_frame_slots={} bounce_stall_sq_space={} \
             coalesce_appends={} coalesce_completed_chunks={} coalesce_fixed_writes={} \
             coalesce_flushes={} coalesce_nt_copy={} coalesce_nt_copy_calls={} \
             coalesce_nt_copy_bytes={} \
             seconds={secs:.6} thread_cpu_seconds={cpu_secs:.6} cpu_wall_pct={cpu_pct:.1} \
             MiBps={:.2} target_cpu={} affinity_applied={} \
             start_cpu={} end_cpu={} worker_numa_node={} \
             voluntary_ctxt_switches={} involuntary_ctxt_switches={} migrations={}",
            stats.worker,
            stats.rxq,
            stats.streams,
            stats.planned_nvme_queue,
            stats.wal_region_base,
            stats.wal_region_end,
            stats.slot_stride,
            stats.fixed_buffers,
            stats.zcrx_area_base,
            stats.zcrx_area_bytes,
            stats.zcrx_area_alignment,
            stats.zcrx_rx_buf_len,
            stats.zcrx_area_memory_policy,
            stats.write_pipeline,
            stats.frame_pipeline,
            stats.coalesce_batch_chunks,
            stats.coalesce_write_bytes,
            stats.received_bytes,
            stats.written_bytes,
            stats.wal_bytes,
            stats.direct_frames,
            stats.direct_bytes,
            stats.bounce_frames,
            stats.bounce_bytes,
            stats.coalesce_frames,
            stats.coalesce_bytes,
            stats.frames,
            stats.chunks,
            yes(stats.counters.direct_enabled),
            yes(stats.counters.recv_multishot),
            stats.counters.max_active_recvs,
            stats.counters.recv_data_cqes,
            stats.counters.recv_final_cqes,
            stats.counters.recv_empty_finals,
            stats.counters.transient_recv_errors,
            stats.counters.transient_recv_deferred,
            stats.counters.transient_recv_backoffs,
            stats.counters.recv_flush_calls,
            stats.counters.recv_flush_queued,
            stats.counters.pending_frame_requeues,
            stats.counters.pending_frame_max,
            stats.counters.direct_queue_attempts,
            stats.counters.direct_stall_write_slots,
            stats.counters.direct_stall_frame_slots,
            stats.counters.direct_stall_busy_slots,
            stats.counters.direct_stall_sq_space,
            stats.counters.direct_padded_frames,
            stats.counters.direct_padded_bytes,
            stats.counters.direct_multisegment_frames,
            stats.counters.direct_segment_writes,
            stats.counters.direct_fallback_to_bounce,
            yes(stats.counters.direct_slot_busy_guard),
            yes(stats.counters.direct_busy_bounce),
            stats.counters.direct_busy_bounce_attempts,
            yes(stats.counters.direct_stall_coalesce),
            stats.counters.direct_stall_coalesce_attempts,
            stats.counters.bounce_queue_attempts,
            stats.counters.bounce_stall_write_slots,
            stats.counters.bounce_stall_frame_slots,
            stats.counters.bounce_stall_sq_space,
            stats.counters.coalesce_appends,
            stats.counters.coalesce_completed_chunks,
            stats.counters.coalesce_fixed_writes,
            stats.counters.coalesce_flushes,
            yes(stats.counters.coalesce_nt_copy),
            stats.counters.coalesce_nt_copy_calls,
            stats.counters.coalesce_nt_copy_bytes,
            (stats.written_bytes as f64 / (1024.0 * 1024.0)) / secs,
            stats.target_cpu,
            stats.affinity_applied,
            stats.start_cpu,
            stats.end_cpu,
            option_i32_label(stats.worker_numa_node),
            stats.voluntary_switches,
            stats.involuntary_switches,
            stats.migrations
        );
        if stats.timing.enabled {
            let avg =
                |ns: u128, calls: u64| -> u128 { if calls == 0 { 0 } else { ns / calls as u128 } };
            println!(
                "tcp-wal-mux-server-zcrx-timing: worker={} rxq={} \
                 queue_write_calls={} queue_write_ns={} queue_write_avg_ns={} \
                 coalesce_flush_calls={} coalesce_flush_ns={} coalesce_flush_avg_ns={} \
                 wait_cqe_calls={} wait_cqe_ns={} wait_cqe_avg_ns={} \
                 complete_cqe_calls={} complete_cqe_ns={} complete_cqe_avg_ns={}",
                stats.worker,
                stats.rxq,
                stats.timing.queue_write_calls,
                stats.timing.queue_write_ns,
                avg(stats.timing.queue_write_ns, stats.timing.queue_write_calls),
                stats.timing.coalesce_flush_calls,
                stats.timing.coalesce_flush_ns,
                avg(
                    stats.timing.coalesce_flush_ns,
                    stats.timing.coalesce_flush_calls
                ),
                stats.timing.wait_cqe_calls,
                stats.timing.wait_cqe_ns,
                avg(stats.timing.wait_cqe_ns, stats.timing.wait_cqe_calls),
                stats.timing.complete_cqe_calls,
                stats.timing.complete_cqe_ns,
                avg(
                    stats.timing.complete_cqe_ns,
                    stats.timing.complete_cqe_calls
                )
            );
        }
        for (conn, conn_result) in stats.conn_results.iter().enumerate() {
            let conn_stats = &conn_result.counters;
            println!(
                "tcp-wal-mux-server-zcrx-conn: worker={} rxq={} conn={} peer={} \
                 listener_lane={} listener_port={} conn_index={} {} \
                 received_bytes={} recv_data_cqes={} full_chunk_cqes={} partial_cqes={} \
                 min_cqe_len={} max_cqe_len={} direct_frames={} direct_bytes={} \
                 coalesce_frames={} coalesce_bytes={} pending_frame_requeues={}",
                stats.worker,
                stats.rxq,
                conn,
                conn_result.meta.peer_addr,
                conn_result.meta.lane,
                conn_result.meta.port,
                conn_result.meta.conn_index,
                socket_locality_label(conn_result.meta.locality),
                conn_stats.received_bytes,
                conn_stats.recv_data_cqes,
                conn_stats.full_chunk_cqes,
                conn_stats.partial_cqes,
                conn_stats.min_cqe_len,
                conn_stats.max_cqe_len,
                conn_stats.direct_frames,
                conn_stats.direct_bytes,
                conn_stats.coalesce_frames,
                conn_stats.coalesce_bytes,
                conn_stats.pending_frame_requeues
            );
        }
        total_received += stats.received_bytes;
        total_written += stats.written_bytes;
        total_wal += stats.wal_bytes;
        total_direct_frames += stats.direct_frames;
        total_direct_bytes += stats.direct_bytes;
        total_bounce_frames += stats.bounce_frames;
        total_bounce_bytes += stats.bounce_bytes;
        total_coalesce_frames += stats.coalesce_frames;
        total_coalesce_bytes += stats.coalesce_bytes;
        total_frames += stats.frames;
        total_chunks += stats.chunks;
    }

    println!(
        "tcp-wal-mux-server-zcrx-summary: received_bytes={total_received} \
         written_bytes={total_written} wal_bytes={total_wal} \
         direct_frames={total_direct_frames} direct_bytes={total_direct_bytes} \
         bounce_frames={total_bounce_frames} bounce_bytes={total_bounce_bytes} \
         coalesce_frames={total_coalesce_frames} coalesce_bytes={total_coalesce_bytes} \
         frames={total_frames} chunks={total_chunks}"
    );
    print_tcp_bench_result("tcp-wal-mux-server-zcrx", total_written, started.elapsed());
    Ok(())
}

fn tcp_wal_mux_server(
    target_arg: &str,
    bind: &str,
    base_port: u16,
    ports: usize,
    connections_per_port: usize,
    bytes_per_connection: usize,
    chunk_bytes: usize,
    pipeline: usize,
    workers: usize,
    ring_entries: u32,
    buffer_mode: SlotWalBufferMode,
    pin_workers: bool,
) -> io::Result<()> {
    let total_connections = tcp_bench_total_connections(ports, connections_per_port)?;
    let pipeline_mode = TcpWalPipelineMode::from_env()?;
    let write_mode = TcpWalWriteMode::from_env()?;
    let fixed_recv = pipeline_mode != TcpWalPipelineMode::Zcrx && tcp_wal_fixed_recv_enabled();
    let effective_connections_per_port = if pipeline_mode == TcpWalPipelineMode::Zcrx {
        tcp_wal_zcrx_selected_connections_per_port(connections_per_port)?
    } else {
        connections_per_port
    };
    let effective_total_connections =
        tcp_bench_total_connections(ports, effective_connections_per_port)?;
    if bytes_per_connection == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bytes-per-connection must be greater than zero",
        ));
    }
    if chunk_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk-bytes must be greater than zero",
        ));
    }
    if bytes_per_connection % chunk_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bytes-per-connection must be an exact multiple of chunk-bytes",
        ));
    }
    validate_slot_wal_pipeline(pipeline, ring_entries)?;
    let total_bytes = effective_total_connections
        .checked_mul(bytes_per_connection)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "total TCP WAL bytes overflow")
        })?;
    let (target, device_bytes, required_alignment, _segment_bytes) = validate_slot_wal_common(
        target_arg,
        total_bytes,
        chunk_bytes,
        SlotWalMode::Write,
        buffer_mode,
    )?;
    let region_base_offset = env_size_opt("URING_PLAY_WAL_BASE_OFFSET_BYTES")?.unwrap_or(0);
    let default_region_bytes = if pipeline_mode == TcpWalPipelineMode::Zcrx {
        let multiplier = env_usize_or("URING_PLAY_ZCRX_WAL_REGION_MULTIPLIER", 2).max(1);
        total_bytes.checked_mul(multiplier).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "default ZCRX WAL region size overflow",
            )
        })?
    } else {
        total_bytes
    };
    let region_bytes = env_size_opt("URING_PLAY_WAL_REGION_BYTES")?.unwrap_or(default_region_bytes);
    if region_base_offset % required_alignment != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("URING_PLAY_WAL_BASE_OFFSET_BYTES must be a multiple of {required_alignment}"),
        ));
    }
    ensure_aligned_to(
        region_bytes,
        required_alignment,
        "URING_PLAY_WAL_REGION_BYTES",
    )?;
    if total_bytes > region_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "total TCP WAL bytes {total_bytes} exceeds configured WAL region {region_bytes}"
            ),
        ));
    }
    let region_end = (region_base_offset as u64)
        .checked_add(region_bytes as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WAL region end overflow"))?;
    if region_end > device_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "WAL region [{}..{}) exceeds target size {device_bytes}",
                region_base_offset, region_end
            ),
        ));
    }

    let shard_policy =
        TcpMuxShardPolicy::from_env_or("URING_PLAY_TCP_WAL_SHARD", TcpMuxShardPolicy::Observed)?;
    println!(
        "tcp-wal-mux-server: target={} bind={bind} base_port={base_port} ports={ports} \
         connections_per_port={connections_per_port} total_connections={total_connections} \
         effective_connections_per_port={effective_connections_per_port} effective_total_connections={effective_total_connections} \
         bytes_per_connection={bytes_per_connection} total_bytes={total_bytes} \
         wal_region_base={} wal_region_bytes={} \
         chunk_bytes={chunk_bytes} pipeline={pipeline} ring_entries={ring_entries} \
         buffer_mode={} shard_policy={} pipeline_mode={} write_mode={} fixed_recv={} pin_workers={pin_workers}",
        target.label(),
        region_base_offset,
        region_bytes,
        buffer_mode.as_str(),
        shard_policy.label(),
        pipeline_mode.label(),
        write_mode.label(),
        fixed_recv
    );

    if pipeline_mode == TcpWalPipelineMode::Zcrx {
        return tcp_wal_zcrx_mux_server(
            target,
            bind,
            base_port,
            ports,
            connections_per_port,
            bytes_per_connection,
            chunk_bytes,
            pipeline,
            workers,
            ring_entries,
            required_alignment,
            region_base_offset,
            region_end,
            pin_workers,
        );
    }

    let listeners = tcp_bench_mux_bind_listeners(bind, base_port, ports)?;
    let accepted = tcp_bench_mux_accept_tagged_listeners(listeners, ports, connections_per_port)?;
    let workers = tcp_bench_auto_workers(workers, accepted.len());
    let shards = tcp_bench_partition_accepted_streams_with_pin(
        accepted,
        workers,
        shard_policy,
        "tcp-wal-mux-server",
        pin_workers,
    );

    if pipeline_mode == TcpWalPipelineMode::Split {
        let writer_count = env_usize_or("URING_PLAY_TCP_WAL_WRITERS", workers).max(1);
        let buffer_count = workers.checked_mul(pipeline).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "split TCP WAL buffer count overflow",
            )
        })?;
        if buffer_count > u16::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "split TCP WAL needs {buffer_count} buffers/slots, max supported is {}",
                    u16::MAX
                ),
            ));
        }
        let buffers = buffer_mode.allocate(buffer_count, chunk_bytes)?;
        let buffer_view = FixedBufferView::from_buffers(&buffers);
        let mut write_senders = Vec::with_capacity(writer_count);
        let mut write_receivers = Vec::with_capacity(writer_count);
        for _ in 0..writer_count {
            let (tx, rx) = mpsc::channel::<TcpWalWriteDesc>();
            write_senders.push(tx);
            write_receivers.push(rx);
        }
        let mut return_senders = Vec::with_capacity(workers);
        let mut return_receivers = Vec::with_capacity(workers);
        for _ in 0..workers {
            let (tx, rx) = mpsc::channel::<usize>();
            return_senders.push(tx);
            return_receivers.push(Some(rx));
        }

        println!(
            "tcp-wal-mux-server-split: rx_workers={workers} wal_writers={writer_count} \
             shared_buffers={buffer_count} buffers_per_rx_worker={pipeline} write_mode={}",
            write_mode.label()
        );

        let wal_allocator = Arc::new(AtomicU64::new(region_base_offset as u64));
        let started = Instant::now();
        let mut writer_handles = Vec::with_capacity(writer_count);
        for (writer_idx, write_rx) in write_receivers.into_iter().enumerate() {
            let target = target.clone();
            let returns = return_senders.clone();
            let writer_affinity_index = workers + writer_idx;
            writer_handles.push(thread::spawn(move || {
                tcp_wal_split_writer_worker(
                    target,
                    writer_idx,
                    writer_affinity_index,
                    buffer_view,
                    returns,
                    write_rx,
                    chunk_bytes,
                    ring_entries,
                    write_mode,
                    pin_workers,
                )
            }));
        }
        drop(return_senders);

        let mut rx_handles = Vec::with_capacity(workers);
        for (worker_idx, shard) in shards.into_iter().enumerate() {
            let Some(return_rx) = return_receivers.get_mut(worker_idx).and_then(Option::take)
            else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "split TCP WAL return receiver missing",
                ));
            };
            if shard.is_empty() {
                continue;
            }
            let writer_txs = write_senders.clone();
            let allocator = wal_allocator.clone();
            let slot_start = worker_idx.checked_mul(pipeline).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "split TCP WAL slot range overflow",
                )
            })?;
            rx_handles.push(thread::spawn(move || {
                tcp_wal_split_rx_worker(
                    worker_idx,
                    slot_start,
                    pipeline,
                    shard,
                    buffer_view,
                    writer_txs,
                    return_rx,
                    allocator,
                    region_end,
                    bytes_per_connection,
                    chunk_bytes,
                    ring_entries,
                    fixed_recv,
                    pin_workers,
                )
            }));
        }
        drop(write_senders);

        let mut total_received = 0usize;
        let mut total_submitted = 0usize;
        let mut rx_chunks = 0usize;
        for handle in rx_handles {
            let stats = handle
                .join()
                .map_err(|_| io::Error::other("split TCP WAL RX worker panicked"))??;
            let secs = stats.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
            println!(
                "tcp-wal-mux-server-split-rx-worker: worker={} streams={} \
                 received_bytes={} submitted_bytes={} chunks={} chunks_per_sec={:.0} \
                 seconds={secs:.6} MiBps={:.2} target_cpu={} affinity_applied={}",
                stats.worker,
                stats.streams,
                stats.received_bytes,
                stats.submitted_bytes,
                stats.chunks,
                stats.chunks as f64 / secs,
                (stats.submitted_bytes as f64 / (1024.0 * 1024.0)) / secs,
                stats.target_cpu,
                stats.affinity_applied
            );
            total_received += stats.received_bytes;
            total_submitted += stats.submitted_bytes;
            rx_chunks += stats.chunks;
        }

        let mut total_written = 0usize;
        let mut writer_chunks = 0usize;
        for handle in writer_handles {
            let stats = handle
                .join()
                .map_err(|_| io::Error::other("split TCP WAL writer panicked"))??;
            let secs = stats.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
            println!(
                "tcp-wal-mux-server-split-writer: writer={} written_bytes={} chunks={} \
                 chunks_per_sec={:.0} seconds={secs:.6} MiBps={:.2} \
                 target_cpu={} affinity_applied={}",
                stats.writer,
                stats.written_bytes,
                stats.chunks,
                stats.chunks as f64 / secs,
                (stats.written_bytes as f64 / (1024.0 * 1024.0)) / secs,
                stats.target_cpu,
                stats.affinity_applied
            );
            total_written += stats.written_bytes;
            writer_chunks += stats.chunks;
        }

        println!(
            "tcp-wal-mux-server-split-summary: received_bytes={total_received} \
             submitted_bytes={total_submitted} written_bytes={total_written} \
             rx_chunks={rx_chunks} writer_chunks={writer_chunks} \
             writer_chunks_per_sec={:.0}",
            writer_chunks as f64 / started.elapsed().as_secs_f64().max(f64::MIN_POSITIVE)
        );
        print_tcp_bench_result("tcp-wal-mux-server", total_written, started.elapsed());
        return Ok(());
    }

    let mut base_offsets = Vec::with_capacity(workers);
    let mut next_base_offset = region_base_offset as u64;
    for shard in &shards {
        base_offsets.push(next_base_offset);
        let shard_bytes = shard
            .len()
            .checked_mul(bytes_per_connection)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "shard byte count overflow")
            })?;
        next_base_offset = next_base_offset
            .checked_add(shard_bytes as u64)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "WAL shard offset overflow")
            })?;
    }

    let started = Instant::now();
    let mut handles = Vec::with_capacity(workers);
    for (worker_idx, shard) in shards.into_iter().enumerate() {
        if shard.is_empty() {
            continue;
        }
        let target = target.clone();
        let base_offset = base_offsets[worker_idx];
        handles.push(thread::spawn(move || {
            tcp_wal_worker(
                target,
                worker_idx,
                base_offset,
                shard,
                bytes_per_connection,
                chunk_bytes,
                pipeline,
                ring_entries,
                buffer_mode,
                write_mode,
                fixed_recv,
                pin_workers,
            )
        }));
    }

    let mut total_received = 0usize;
    let mut total_written = 0usize;
    let mut total_chunks = 0usize;
    for handle in handles {
        let stats = handle
            .join()
            .map_err(|_| io::Error::other("TCP WAL worker thread panicked"))??;
        let secs = stats.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        println!(
            "tcp-wal-mux-server-worker: worker={} streams={} received_bytes={} \
             written_bytes={} chunks={} chunks_per_sec={:.0} seconds={secs:.6} MiBps={:.2} \
             target_cpu={} affinity_applied={}",
            stats.worker,
            stats.streams,
            stats.received_bytes,
            stats.written_bytes,
            stats.chunks,
            stats.chunks as f64 / secs,
            (stats.written_bytes as f64 / (1024.0 * 1024.0)) / secs,
            stats.target_cpu,
            stats.affinity_applied
        );
        total_received += stats.received_bytes;
        total_written += stats.written_bytes;
        total_chunks += stats.chunks;
    }

    println!(
        "tcp-wal-mux-server-summary: received_bytes={total_received} \
         written_bytes={total_written} chunks={total_chunks} chunks_per_sec={:.0}",
        total_chunks as f64 / started.elapsed().as_secs_f64().max(f64::MIN_POSITIVE)
    );
    print_tcp_bench_result("tcp-wal-mux-server", total_written, started.elapsed());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn udp_wal_mux_server(
    target_arg: &str,
    bind: &str,
    base_port: u16,
    ports: usize,
    flows_per_port: usize,
    bytes_per_flow: usize,
    chunk_bytes: usize,
    pipeline: usize,
    workers: usize,
    ring_entries: u32,
    buffer_mode: SlotWalBufferMode,
    pin_workers: bool,
) -> io::Result<()> {
    let total_flows = tcp_bench_total_connections(ports, flows_per_port)?;
    if bytes_per_flow == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bytes-per-flow must be greater than zero",
        ));
    }
    if chunk_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk-bytes must be greater than zero",
        ));
    }
    let datagram_bytes =
        env_size_opt("URING_PLAY_UDP_DATAGRAM_BYTES")?.unwrap_or(chunk_bytes.min(32 * 1024));
    if datagram_bytes == 0 || datagram_bytes > 65_507 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "URING_PLAY_UDP_DATAGRAM_BYTES must be between 1 and 65507",
        ));
    }
    if chunk_bytes % datagram_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk-bytes must be a multiple of URING_PLAY_UDP_DATAGRAM_BYTES",
        ));
    }
    if bytes_per_flow % datagram_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bytes-per-flow must be a multiple of URING_PLAY_UDP_DATAGRAM_BYTES",
        ));
    }
    let bytes_per_socket = flows_per_port.checked_mul(bytes_per_flow).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "UDP bytes per port overflow")
    })?;
    if bytes_per_socket % chunk_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "flows-per-port * bytes-per-flow must be a multiple of chunk-bytes",
        ));
    }
    validate_slot_wal_pipeline(pipeline, ring_entries)?;
    let total_bytes = total_flows.checked_mul(bytes_per_flow).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "total UDP WAL bytes overflow")
    })?;
    let (target, device_bytes, required_alignment, _segment_bytes) = validate_slot_wal_common(
        target_arg,
        total_bytes,
        chunk_bytes,
        SlotWalMode::Write,
        buffer_mode,
    )?;
    let region_base_offset = env_size_opt("URING_PLAY_WAL_BASE_OFFSET_BYTES")?.unwrap_or(0);
    let region_bytes = env_size_opt("URING_PLAY_WAL_REGION_BYTES")?.unwrap_or(total_bytes);
    if region_base_offset % required_alignment != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("URING_PLAY_WAL_BASE_OFFSET_BYTES must be a multiple of {required_alignment}"),
        ));
    }
    ensure_aligned_to(
        region_bytes,
        required_alignment,
        "URING_PLAY_WAL_REGION_BYTES",
    )?;
    if total_bytes > region_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "total UDP WAL bytes {total_bytes} exceeds configured WAL region {region_bytes}"
            ),
        ));
    }
    let region_end = (region_base_offset as u64)
        .checked_add(region_bytes as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WAL region end overflow"))?;
    if region_end > device_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "WAL region [{}..{}) exceeds target size {device_bytes}",
                region_base_offset, region_end
            ),
        ));
    }

    let workers = tcp_bench_auto_workers(workers, ports);
    println!(
        "udp-wal-mux-server: target={} bind={bind} base_port={base_port} ports={ports} \
         flows_per_port={flows_per_port} total_flows={total_flows} \
         bytes_per_flow={bytes_per_flow} bytes_per_port={bytes_per_socket} \
         total_bytes={total_bytes} wal_region_base={} wal_region_bytes={} \
         chunk_bytes={chunk_bytes} datagram_bytes={datagram_bytes} pipeline={pipeline} \
         ring_entries={ring_entries} workers={workers} buffer_mode={} pin_workers={pin_workers}",
        target.label(),
        region_base_offset,
        region_bytes,
        buffer_mode.as_str()
    );

    let mut shards = (0..workers).map(|_| Vec::new()).collect::<Vec<_>>();
    for lane in 0..ports {
        let port = tcp_bench_port(base_port, lane)?;
        let socket = UdpSocket::bind((bind, port))?;
        set_udp_bench_buffers(&socket);
        println!("udp-wal-mux-server: bound {bind}:{port} lane={lane}");
        shards[lane % workers].push(socket);
    }

    let mut base_offsets = Vec::with_capacity(workers);
    let mut next_base_offset = region_base_offset as u64;
    for shard in &shards {
        base_offsets.push(next_base_offset);
        let shard_bytes = shard.len().checked_mul(bytes_per_socket).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "UDP WAL shard byte overflow")
        })?;
        next_base_offset = next_base_offset
            .checked_add(shard_bytes as u64)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "UDP WAL shard offset overflow")
            })?;
    }

    let mut handles = Vec::with_capacity(workers);
    for (worker_idx, shard) in shards.into_iter().enumerate() {
        if shard.is_empty() {
            continue;
        }
        let target = target.clone();
        let base_offset = base_offsets[worker_idx];
        handles.push(thread::spawn(move || {
            udp_wal_worker(
                target,
                worker_idx,
                base_offset,
                shard,
                bytes_per_socket,
                chunk_bytes,
                datagram_bytes,
                pipeline,
                ring_entries,
                buffer_mode,
                pin_workers,
            )
        }));
    }

    let mut total_received = 0usize;
    let mut total_written = 0usize;
    let mut total_chunks = 0usize;
    let mut max_elapsed = Duration::ZERO;
    for handle in handles {
        let stats = handle
            .join()
            .map_err(|_| io::Error::other("UDP WAL worker thread panicked"))??;
        let secs = stats.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        println!(
            "udp-wal-mux-server-worker: worker={} sockets={} received_bytes={} \
             written_bytes={} chunks={} seconds={secs:.6} MiBps={:.2} \
             target_cpu={} affinity_applied={}",
            stats.worker,
            stats.streams,
            stats.received_bytes,
            stats.written_bytes,
            stats.chunks,
            (stats.written_bytes as f64 / (1024.0 * 1024.0)) / secs,
            stats.target_cpu,
            stats.affinity_applied
        );
        total_received += stats.received_bytes;
        total_written += stats.written_bytes;
        total_chunks += stats.chunks;
        max_elapsed = max_elapsed.max(stats.elapsed);
    }

    println!(
        "udp-wal-mux-server-summary: received_bytes={total_received} \
         written_bytes={total_written} chunks={total_chunks}"
    );
    print_tcp_bench_result("udp-wal-mux-server", total_written, max_elapsed);
    Ok(())
}

fn tcp_bench_uring_zcrx_mux_server(
    ifname: &str,
    rxq: u32,
    rxq_count: usize,
    bind: &str,
    base_port: u16,
    ports: usize,
    connections_per_port: usize,
    expected_bytes: usize,
    recv_bytes: usize,
    ring_entries: u32,
) -> io::Result<()> {
    let total_connections = tcp_bench_total_connections(ports, connections_per_port)?;
    let rxq_count = rxq_count.max(1);
    let consume_mode = ZcrxConsumeMode::from_env()?;
    println!(
        "tcp-bench-uring-mux-server-zcrx: listening on {bind}:{base_port}.. ports={ports} \
         connections_per_port={connections_per_port} total_connections={total_connections} \
         expected_bytes={expected_bytes} ifname={ifname} rxq={rxq} rxq_count={rxq_count} \
         consume_mode={}",
        consume_mode.label()
    );
    let listeners = tcp_bench_mux_bind_listeners(bind, base_port, ports)?;
    let ring_entries = ring_entries.max((total_connections as u32).saturating_mul(2).max(64));
    let (ready_tx, ready_rx) = mpsc::channel::<io::Result<u32>>();
    let mut stream_senders = Vec::with_capacity(rxq_count);
    let mut handles = Vec::with_capacity(rxq_count);
    for queue_offset in 0..rxq_count {
        let queue = rxq
            .checked_add(queue_offset as u32)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rxq overflow"))?;
        let ifname = ifname.to_string();
        let ready_tx = ready_tx.clone();
        let (streams_tx, streams_rx) = mpsc::channel::<Vec<TcpStream>>();
        stream_senders.push(streams_tx);
        handles.push(thread::spawn(move || -> io::Result<ZcrxWorkerStats> {
            let affinity = maybe_pin_current_thread("zcrx-worker", queue_offset);
            let mut ring = RawRing::new(ring_entries, ring_entries.saturating_mul(2))?;
            ring.register_napi_from_env(&format!("zcrx-worker-{queue_offset}"))?;
            let mut zcrx = match ZcrxContext::register(ring.fd(), &ifname, queue, true, None) {
                Ok(zcrx) => {
                    let _ = ready_tx.send(Ok(queue));
                    zcrx
                }
                Err(err) => {
                    let message = format!("rxq={queue} register ZCRX IFQ failed: {err}");
                    let kind = err.kind();
                    let _ = ready_tx.send(Err(io::Error::new(kind, message.clone())));
                    return Err(io::Error::new(kind, message));
                }
            };
            let streams = streams_rx.recv().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("rxq={queue} stream channel closed before receive"),
                )
            })?;
            uring_zcrx_recv_worker(
                queue,
                affinity,
                consume_mode,
                &mut ring,
                &mut zcrx,
                streams,
                expected_bytes,
                recv_bytes,
            )
        }));
    }
    drop(ready_tx);
    let mut ready_error = None;
    for _ in 0..rxq_count {
        match ready_rx.recv().map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ZCRX IFQ registration channel closed",
            )
        })? {
            Ok(_) => {}
            Err(err) => {
                ready_error = Some(err);
            }
        }
    }
    if let Some(err) = ready_error {
        drop(stream_senders);
        for handle in handles {
            let _ = handle.join();
        }
        return Err(err);
    }
    println!("registered all ZCRX IFQs: count={rxq_count}");
    let accepted = tcp_bench_mux_accept_tagged_listeners(listeners, ports, connections_per_port)?;
    let shard_policy =
        TcpMuxShardPolicy::from_env_or("URING_PLAY_ZCRX_SHARD", TcpMuxShardPolicy::PortLane)?;
    let mut shards = (0..rxq_count).map(|_| Vec::new()).collect::<Vec<_>>();
    for (idx, accepted) in accepted.into_iter().enumerate() {
        let worker = shard_policy.choose_worker(&accepted, idx, rxq_count);
        println!(
            "tcp-bench-uring-mux-server-zcrx-mux-assignment: rxq_worker={worker} policy={} \
             peer={} listener_lane={} listener_port={} conn={} {}",
            shard_policy.label(),
            accepted.peer_addr,
            accepted.lane,
            accepted.port,
            accepted.conn_index,
            socket_locality_label(accepted.locality)
        );
        shards[worker].push(accepted.stream);
    }

    let softirq_before = read_softirq_per_cpu_counts().unwrap_or_default();
    let softnet_before = read_softnet_stat().unwrap_or_default();
    let started = Instant::now();
    for (idx, streams) in shards.into_iter().enumerate() {
        stream_senders[idx].send(streams).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("rxq worker {idx} exited before receive streams were sent"),
            )
        })?;
    }
    drop(stream_senders);

    let mut total = 0usize;
    let mut total_consumed = 0usize;
    let mut total_skipped = 0usize;
    let mut total_frames = 0usize;
    let mut total_checksum = 0u64;
    for handle in handles {
        let stats = handle.join().map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "tcp bench uring zcrx recv worker panicked",
            )
        })??;
        let wall_secs = stats.wall.as_secs_f64().max(f64::MIN_POSITIVE);
        let cpu_secs = stats.cpu.as_secs_f64();
        let cpu_pct = (cpu_secs / wall_secs) * 100.0;
        println!(
            "tcp-bench-uring-mux-server-zcrx-worker: rxq={} streams={} bytes={} \
             consumed_bytes={} skipped_bytes={} frames={} checksum=0x{:016x} \
             wall_seconds={:.6} thread_cpu_seconds={:.6} cpu_wall_pct={:.1} \
             target_cpu={} affinity_applied={} start_cpu={} end_cpu={} voluntary_ctxt_switches={} \
             involuntary_ctxt_switches={} migrations={}",
            stats.rxq,
            stats.streams,
            stats.bytes,
            stats.consumed_bytes,
            stats.skipped_bytes,
            stats.frames,
            stats.checksum,
            wall_secs,
            cpu_secs,
            cpu_pct,
            stats.target_cpu,
            stats.affinity_applied,
            stats.start_cpu,
            stats.end_cpu,
            stats.voluntary_switches,
            stats.involuntary_switches,
            stats.migrations
        );
        total += stats.bytes;
        total_consumed += stats.consumed_bytes;
        total_skipped += stats.skipped_bytes;
        total_frames += stats.frames;
        total_checksum = total_checksum.wrapping_add(stats.checksum);
    }
    let softirq_after = read_softirq_per_cpu_counts().unwrap_or_default();
    let softnet_after = read_softnet_stat().unwrap_or_default();
    let softirq_before_total = sum_softirq_counts(&softirq_before);
    let softirq_after_total = sum_softirq_counts(&softirq_after);
    print_softirq_delta(
        "tcp-bench-uring-mux-server-zcrx-softirq-delta",
        &softirq_before_total,
        &softirq_after_total,
    );
    print_softirq_per_cpu_delta(
        "tcp-bench-uring-mux-server-zcrx-softirq-cpu-delta",
        &softirq_before,
        &softirq_after,
    );
    print_softnet_delta(
        "tcp-bench-uring-mux-server-zcrx-softnet-delta",
        &softnet_before,
        &softnet_after,
    );
    if consume_mode != ZcrxConsumeMode::None {
        println!(
            "tcp-bench-uring-mux-server-zcrx-consume: mode={} bytes={} skipped_bytes={} frames={} checksum=0x{:016x}",
            consume_mode.label(),
            total_consumed,
            total_skipped,
            total_frames,
            total_checksum
        );
    }
    print_tcp_bench_result("tcp-bench-uring-mux-server-zcrx", total, started.elapsed());
    Ok(())
}

fn env_truthy(name: &str) -> bool {
    env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn env_enabled_or(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(value) => match value.as_str() {
            "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON" => true,
            "0" | "false" | "FALSE" | "no" | "NO" | "off" | "OFF" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

fn napi_tracking_label(value: u32) -> &'static str {
    match value {
        0 => "dynamic",
        1 => "static",
        255 => "inactive",
        _ => "custom",
    }
}

fn napi_tracking_from_env() -> io::Result<u32> {
    let Ok(value) = env::var("URING_PLAY_NAPI_TRACKING") else {
        return Ok(0);
    };
    match value.as_str() {
        "dynamic" | "0" => Ok(0),
        "static" | "1" => Ok(1),
        "inactive" | "255" => Ok(255),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("URING_PLAY_NAPI_TRACKING must be dynamic, static, or inactive; got {other:?}"),
        )),
    }
}

fn napi_config_from_env() -> io::Result<Option<IoUringNapi>> {
    let busy_poll_to = env_size_opt("URING_PLAY_NAPI_BUSY_POLL_US")?;
    if !env_truthy("URING_PLAY_REGISTER_NAPI") && busy_poll_to.is_none() {
        return Ok(None);
    }
    let busy_poll_to = busy_poll_to.unwrap_or(50);
    let busy_poll_to = u32::try_from(busy_poll_to).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "URING_PLAY_NAPI_BUSY_POLL_US must fit in u32",
        )
    })?;
    Ok(Some(IoUringNapi {
        busy_poll_to,
        prefer_busy_poll: u8::from(env_truthy("URING_PLAY_NAPI_PREFER_BUSY_POLL")),
        opcode: 0,
        pad: [0; 2],
        op_param: napi_tracking_from_env()?,
        resv: 0,
    }))
}

fn running_under_qemu() -> bool {
    for path in [
        "/sys/class/dmi/id/sys_vendor",
        "/sys/class/dmi/id/product_name",
        "/sys/class/dmi/id/board_vendor",
    ] {
        let Ok(value) = fs::read_to_string(path) else {
            continue;
        };
        let value = value.to_ascii_lowercase();
        if value.contains("qemu") || value.contains("kvm") || value.contains("bochs") {
            return true;
        }
    }
    false
}

fn validate_uring_send_mode_location(send_mode: UringSendMode) -> io::Result<()> {
    if !send_mode.uses_zc() || running_under_qemu() || env_truthy("URING_PLAY_ALLOW_UNSAFE_SEND_ZC")
    {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "send-zc modes are disabled outside QEMU by default after Linux 7.0.8 host bad-page crashes; set URING_PLAY_ALLOW_UNSAFE_SEND_ZC=1 to override",
    ))
}

fn tcp_bench_uring_mux_send(
    addr: &str,
    base_port: u16,
    ports: usize,
    connections_per_port: usize,
    bytes_per_connection: usize,
    chunk_bytes: usize,
    pipeline: usize,
    workers: usize,
    ring_entries: u32,
    send_mode: UringSendMode,
) -> io::Result<()> {
    validate_uring_send_mode_location(send_mode)?;
    let chunk_bytes = chunk_bytes.max(4096);
    let payload_pattern = SendPayloadPattern::from_env(chunk_bytes)?;
    payload_pattern.validate(bytes_per_connection, chunk_bytes)?;
    let source_ports = TcpSourcePortPlan::from_env()?;
    let fixed_file_send = uring_send_fixed_file_enabled();

    let total_connections = tcp_bench_total_connections(ports, connections_per_port)?;
    println!(
        "tcp-bench-uring-mux-send: mode={} addr={addr} base_port={base_port} ports={ports} \
         connections_per_port={connections_per_port} total_connections={total_connections} \
         bytes_per_connection={bytes_per_connection} chunk_bytes={chunk_bytes} \
         pipeline={pipeline} tcp_nodelay={} payload_pattern={} source_ports={} fixed_file={}",
        send_mode.name(),
        yes(tcp_nodelay_enabled()),
        payload_pattern.label(),
        source_ports.label(),
        fixed_file_send
    );
    let connect_pause_ms = env_usize_or("URING_PLAY_CONNECT_PAUSE_MS", 0);
    let workers = tcp_bench_auto_workers(workers, total_connections);
    let connect_specs =
        tcp_bench_mux_connect_specs(base_port, ports, connections_per_port, source_ports)?;
    let shards = tcp_bench_partition_connect_specs(connect_specs, workers);
    let active_workers = shards.iter().filter(|shard| !shard.is_empty()).count();
    let ready_barrier = Arc::new(Barrier::new(active_workers + 1));
    let start_barrier = Arc::new(Barrier::new(active_workers + 1));
    let addr = Arc::new(addr.to_string());
    let mut handles = Vec::with_capacity(workers);
    for (worker_idx, shard) in shards.into_iter().enumerate() {
        if shard.is_empty() {
            continue;
        }
        let worker_ready_barrier = Arc::clone(&ready_barrier);
        let worker_start_barrier = Arc::clone(&start_barrier);
        let worker_addr = Arc::clone(&addr);
        handles.push(thread::spawn(move || {
            let affinity = maybe_pin_current_thread("uring-send-worker", worker_idx);
            let streams = tcp_bench_connect_worker_streams(&worker_addr, &shard)?;
            let streams =
                maybe_run_client_start_handshake(streams, "tcp-bench-uring-mux-send")?;
            if connect_pause_ms > 0 {
                println!(
                    "tcp-bench-uring-mux-send-worker: worker={worker_idx} post_connect_pause_ms={connect_pause_ms}"
                );
                thread::sleep(Duration::from_millis(connect_pause_ms as u64));
            }
            worker_ready_barrier.wait();
            worker_start_barrier.wait();
            uring_send_worker(
                worker_idx,
                affinity,
                streams,
                bytes_per_connection,
                chunk_bytes,
                pipeline,
                ring_entries,
                send_mode,
                payload_pattern,
                fixed_file_send,
            )
        }));
    }
    ready_barrier.wait();
    let started = Instant::now();
    start_barrier.wait();

    let mut worker_results = Vec::with_capacity(handles.len());
    for handle in handles {
        let worker_stats = handle.join().map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "tcp bench uring mux send worker panicked",
            )
        })??;
        worker_results.push(worker_stats);
    }
    let launcher_elapsed = started.elapsed();
    let elapsed = worker_results
        .iter()
        .map(|stats| stats.wall)
        .max()
        .unwrap_or(launcher_elapsed);

    let mut stats = UringSendStats::default();
    for worker_stats in worker_results {
        let wall_secs = worker_stats.wall.as_secs_f64().max(f64::MIN_POSITIVE);
        let cpu_secs = worker_stats.cpu.as_secs_f64();
        let cpu_pct = (cpu_secs / wall_secs) * 100.0;
        println!(
            "tcp-bench-uring-mux-send-worker: worker={} streams={} bytes={} \
             wall_seconds={:.6} thread_cpu_seconds={:.6} cpu_wall_pct={:.1} \
             target_cpu={} affinity_applied={} start_cpu={} end_cpu={} \
             voluntary_ctxt_switches={} involuntary_ctxt_switches={} migrations={}",
            worker_stats.worker,
            worker_stats.streams,
            worker_stats.bytes,
            wall_secs,
            cpu_secs,
            cpu_pct,
            worker_stats.target_cpu,
            worker_stats.affinity_applied,
            worker_stats.start_cpu,
            worker_stats.end_cpu,
            worker_stats.voluntary_switches,
            worker_stats.involuntary_switches,
            worker_stats.migrations
        );
        stats.bytes += worker_stats.bytes;
        stats.zc_notifications += worker_stats.zc_notifications;
        stats.zc_copied_notifications += worker_stats.zc_copied_notifications;
    }
    println!(
        "tcp-bench-uring-mux-send-launcher: active_workers={} launcher_seconds={:.6} hot_seconds={:.6}",
        active_workers,
        launcher_elapsed.as_secs_f64(),
        elapsed.as_secs_f64()
    );
    print_tcp_bench_result("tcp-bench-uring-mux-send", stats.bytes, elapsed);
    if send_mode.uses_zc() {
        println!(
            "tcp-bench-uring-mux-send-zc: notifications={} copied_notifications={}",
            stats.zc_notifications, stats.zc_copied_notifications
        );
    }
    Ok(())
}

fn udp_bench_mux_send(
    addr: &str,
    base_port: u16,
    ports: usize,
    flows_per_port: usize,
    bytes_per_flow: usize,
    datagram_bytes: usize,
    workers: usize,
) -> io::Result<()> {
    let total_flows = tcp_bench_total_connections(ports, flows_per_port)?;
    if bytes_per_flow == 0 || datagram_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bytes-per-flow and datagram-bytes must be greater than zero",
        ));
    }
    if datagram_bytes > 65_507 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "datagram-bytes must be <= 65507",
        ));
    }
    if bytes_per_flow % datagram_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bytes-per-flow must be an exact multiple of datagram-bytes",
        ));
    }

    let source_ports = TcpSourcePortPlan::from_env()?;
    println!(
        "udp-bench-mux-send: addr={addr} base_port={base_port} ports={ports} \
         flows_per_port={flows_per_port} total_flows={total_flows} \
         bytes_per_flow={bytes_per_flow} datagram_bytes={datagram_bytes} \
         source_ports={}",
        source_ports.label()
    );

    let mut flows = Vec::with_capacity(total_flows);
    for lane in 0..ports {
        let port = tcp_bench_port(base_port, lane)?;
        for flow_idx in 0..flows_per_port {
            let global_idx = lane
                .checked_mul(flows_per_port)
                .and_then(|base| base.checked_add(flow_idx))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "UDP flow index overflow")
                })?;
            flows.push((lane, flow_idx, port, source_ports.source_port(global_idx)?));
        }
    }

    let workers = tcp_bench_auto_workers(workers, flows.len());
    let mut shards = (0..workers).map(|_| Vec::new()).collect::<Vec<_>>();
    for (idx, flow) in flows.into_iter().enumerate() {
        shards[idx % workers].push(flow);
    }

    let started = Instant::now();
    let mut handles = Vec::with_capacity(workers);
    for (worker_idx, shard) in shards.into_iter().enumerate() {
        if shard.is_empty() {
            continue;
        }
        let addr = addr.to_string();
        handles.push(thread::spawn(move || -> io::Result<usize> {
            let _affinity = maybe_pin_current_thread("udp-send-worker", worker_idx);
            let buf = vec![0u8; datagram_bytes];
            let mut sent_total = 0usize;
            for (lane, flow_idx, port, source_port) in shard {
                let socket = udp_bench_connect(&addr, port, source_port).map_err(|err| {
                    io::Error::new(
                        err.kind(),
                        format!("udp connect {addr}:{port} lane={lane} flow={flow_idx}: {err}"),
                    )
                })?;
                let mut sent = 0usize;
                while sent < bytes_per_flow {
                    let n = socket.send(&buf)?;
                    if n != datagram_bytes {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            format!("UDP send wrote {n}/{datagram_bytes} bytes"),
                        ));
                    }
                    sent += n;
                }
                sent_total += sent;
            }
            Ok(sent_total)
        }));
    }

    let mut total = 0usize;
    for handle in handles {
        total += handle
            .join()
            .map_err(|_| io::Error::other("UDP mux send worker panicked"))??;
    }
    print_tcp_bench_result("udp-bench-mux-send", total, started.elapsed());
    Ok(())
}

fn raft_append_header(index: u64, payload_len: usize) -> io::Result<[u8; RAFT_APPEND_HEADER_LEN]> {
    let payload_len = u32::try_from(payload_len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "payload too large"))?;
    let mut header = [0u8; RAFT_APPEND_HEADER_LEN];
    header[..8].copy_from_slice(RAFT_APPEND_MAGIC);
    header[8..16].copy_from_slice(&1u64.to_be_bytes());
    header[16..24].copy_from_slice(&index.to_be_bytes());
    header[24..28].copy_from_slice(&payload_len.to_be_bytes());
    header[28..32].copy_from_slice(&0u32.to_be_bytes());
    Ok(header)
}

fn raft_ack_frame(index: u64) -> [u8; RAFT_ACK_LEN] {
    let mut ack = [0u8; RAFT_ACK_LEN];
    ack[..8].copy_from_slice(RAFT_ACK_MAGIC);
    ack[8..16].copy_from_slice(&index.to_be_bytes());
    ack
}

fn parse_be_u64(buf: &[u8]) -> u64 {
    u64::from_be_bytes(buf.try_into().expect("u64 slice length"))
}

fn parse_be_u32(buf: &[u8]) -> u32 {
    u32::from_be_bytes(buf.try_into().expect("u32 slice length"))
}

fn raft_follower(
    bind: &str,
    port: u16,
    expected_entries: u64,
    expected_payload_bytes: usize,
    ack_stride: u64,
) -> io::Result<()> {
    let ack_stride = ack_stride.max(1);
    let listener = TcpListener::bind((bind, port))?;
    println!(
        "raft-follower: listening on {bind}:{port} expected_entries={expected_entries} \
         payload_bytes={expected_payload_bytes} ack_stride={ack_stride}"
    );

    let (mut stream, peer_addr) = listener.accept()?;
    stream.set_nodelay(true)?;
    println!("raft-follower: accepted {peer_addr}");

    let started = Instant::now();
    let mut header = [0u8; RAFT_APPEND_HEADER_LEN];
    let mut payload = vec![0u8; expected_payload_bytes.max(1)];
    let mut entries = 0u64;
    let mut payload_bytes = 0u64;
    let mut last_index = 0u64;

    while entries < expected_entries {
        stream.read_exact(&mut header)?;
        if &header[..8] != RAFT_APPEND_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid raft append magic",
            ));
        }

        let index = parse_be_u64(&header[16..24]);
        let payload_len = parse_be_u32(&header[24..28]) as usize;
        if payload_len != expected_payload_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unexpected payload length for index {index}: \
                     {payload_len}/{expected_payload_bytes}"
                ),
            ));
        }
        if payload_len > payload.len() {
            payload.resize(payload_len, 0);
        }
        stream.read_exact(&mut payload[..payload_len])?;

        entries += 1;
        payload_bytes += payload_len as u64;
        last_index = index;
        if index % ack_stride == 0 || entries == expected_entries {
            stream.write_all(&raft_ack_frame(index))?;
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "raft-follower: ok entries={entries} last_index={last_index} \
         payload_bytes={payload_bytes} seconds={elapsed:.6} MiBps={:.2}",
        (payload_bytes as f64 / (1024.0 * 1024.0)) / elapsed.max(f64::MIN_POSITIVE)
    );
    Ok(())
}

fn raft_replicate_to_peer(
    peer: String,
    entries: u64,
    payload_bytes: usize,
    ack_stride: u64,
) -> io::Result<(String, u64, u64, f64)> {
    let mut stream = TcpStream::connect(&peer)?;
    stream.set_nodelay(true)?;
    let started = Instant::now();
    let payload = vec![0xa5u8; payload_bytes];
    let mut batch = Vec::with_capacity((payload_bytes + RAFT_APPEND_HEADER_LEN) * 64);
    let batch_limit = 4 * 1024 * 1024usize;
    let mut wire_bytes = 0u64;

    for index in 1..=entries {
        let header = raft_append_header(index, payload_bytes)?;
        batch.extend_from_slice(&header);
        batch.extend_from_slice(&payload);
        if batch.len() >= batch_limit {
            stream.write_all(&batch)?;
            wire_bytes += batch.len() as u64;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        stream.write_all(&batch)?;
        wire_bytes += batch.len() as u64;
    }
    stream.shutdown(Shutdown::Write)?;

    let expected_last_ack = entries - (entries % ack_stride.max(1));
    let expected_last_ack = if expected_last_ack == 0 {
        entries
    } else {
        entries.max(expected_last_ack)
    };
    let mut ack = [0u8; RAFT_ACK_LEN];
    let mut last_ack = 0u64;
    while last_ack < expected_last_ack {
        stream.read_exact(&mut ack)?;
        if &ack[..8] != RAFT_ACK_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid raft ack magic from {peer}"),
            ));
        }
        last_ack = parse_be_u64(&ack[8..16]);
    }

    let elapsed = started.elapsed().as_secs_f64();
    Ok((peer, entries, wire_bytes, elapsed))
}

fn raft_leader(
    peers_csv: &str,
    entries: u64,
    payload_bytes: usize,
    ack_stride: u64,
) -> io::Result<()> {
    if entries == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raft-leader entries must be greater than zero",
        ));
    }

    let peers: Vec<String> = peers_csv
        .split(',')
        .map(str::trim)
        .filter(|peer| !peer.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if peers.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raft-leader requires at least one peer",
        ));
    }

    println!(
        "raft-leader: peers={} entries={} payload_bytes={} ack_stride={}",
        peers.len(),
        entries,
        payload_bytes,
        ack_stride
    );
    let started = Instant::now();
    let mut handles = Vec::with_capacity(peers.len());
    for peer in peers {
        handles.push(thread::spawn(move || {
            raft_replicate_to_peer(peer, entries, payload_bytes, ack_stride)
        }));
    }

    let mut total_wire_bytes = 0u64;
    for handle in handles {
        let (peer, peer_entries, wire_bytes, elapsed) = handle
            .join()
            .map_err(|_| io::Error::other("raft peer thread panicked"))??;
        total_wire_bytes += wire_bytes;
        println!(
            "raft-leader: peer={peer} ok entries={peer_entries} wire_bytes={wire_bytes} \
             seconds={elapsed:.6} MiBps={:.2}",
            (wire_bytes as f64 / (1024.0 * 1024.0)) / elapsed.max(f64::MIN_POSITIVE)
        );
    }

    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "raft-leader: ok peers={} total_wire_bytes={} seconds={elapsed:.6} MiBps={:.2}",
        peers_csv
            .split(',')
            .filter(|peer| !peer.trim().is_empty())
            .count(),
        total_wire_bytes,
        (total_wire_bytes as f64 / (1024.0 * 1024.0)) / elapsed.max(f64::MIN_POSITIVE)
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum SlotWalMode {
    Read,
    Write,
}

impl SlotWalMode {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "read" | "r" => Ok(Self::Read),
            "write" | "w" => Ok(Self::Write),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown slot WAL mode {other:?}; use read or write"),
            )),
        }
    }

    fn direction(self) -> io_slots::SlotRw {
        match self {
            Self::Read => io_slots::SlotRw::Read,
            Self::Write => io_slots::SlotRw::Write,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotWalBufferMode {
    SmallPages,
    HugeTlb,
}

impl SlotWalBufferMode {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "small" | "small-pages" | "pages" | "4k" => Ok(Self::SmallPages),
            "hugetlb" | "huge" | "hugepages" | "2m" => Ok(Self::HugeTlb),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown slot WAL buffer mode {other:?}; use small-pages or hugetlb"),
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::SmallPages => "small-pages",
            Self::HugeTlb => "hugetlb",
        }
    }

    fn segment_bytes(self) -> io::Result<usize> {
        match self {
            Self::SmallPages => page_size(),
            Self::HugeTlb => default_hugepage_size(),
        }
    }

    fn allocate(self, count: usize, len: usize) -> io::Result<FixedSendBuffers> {
        self.allocate_for_worker(count, len, None)
    }

    fn allocate_for_worker(
        self,
        count: usize,
        len: usize,
        preferred_numa_node: Option<i32>,
    ) -> io::Result<FixedSendBuffers> {
        match self {
            Self::SmallPages => {
                FixedSendBuffers::new_with_preferred_numa(count, len, preferred_numa_node)
            }
            Self::HugeTlb => {
                FixedSendBuffers::new_hugetlb_with_preferred_numa(count, len, preferred_numa_node)
            }
        }
    }
}

fn format_mib(bytes: usize) -> String {
    format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
}

fn hugepage_meminfo() -> io::Result<(usize, usize, usize)> {
    let meminfo = fs::read_to_string("/proc/meminfo")?;
    let mut total_pages = None;
    let mut free_pages = None;
    let mut hugepage_size = None;
    for line in meminfo.lines() {
        if let Some(value) = line.strip_prefix("HugePages_Total:") {
            total_pages = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
            );
        } else if let Some(value) = line.strip_prefix("HugePages_Free:") {
            free_pages = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
            );
        } else if let Some(value) = line.strip_prefix("Hugepagesize:") {
            let kb = value
                .trim()
                .strip_suffix("kB")
                .unwrap_or(value.trim())
                .trim()
                .parse::<usize>()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            hugepage_size = Some(kb.checked_mul(1024).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Hugepagesize is too large")
            })?);
        }
    }

    Ok((
        total_pages.unwrap_or(0),
        free_pages.unwrap_or(0),
        hugepage_size.unwrap_or(page_size()?),
    ))
}

fn estimate_hugetlb_bytes(buffer_count: usize, len: usize) -> io::Result<usize> {
    let hugepage_size = default_hugepage_size()?;
    let stride = align_up(len.max(1), hugepage_size);
    stride.checked_mul(buffer_count).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "estimated hugetlb reservation is too large",
        )
    })
}

fn warn_small_page_buffer_mode(context: &str, reason: &str) {
    eprintln!(
        "warning: {context} is running with small-page buffers ({reason}); standard high-throughput mode uses hugetlb buffers. Configure enough huge pages or pass hugetlb explicitly for representative WAL/NIC/DMA locality."
    );
}

fn warn_hugetlb_pressure(context: &str, needed_bytes: usize) -> io::Result<()> {
    let (total_pages, free_pages, hugepage_size) = hugepage_meminfo()?;
    let needed_pages = needed_bytes.div_ceil(hugepage_size);
    if free_pages < needed_pages {
        eprintln!(
            "warning: {context} requested hugetlb buffers needing about {} ({needed_pages} huge pages of {}), but HugePages_Free={free_pages} HugePages_Total={total_pages}; allocation may fail.",
            format_mib(needed_bytes),
            format_mib(hugepage_size)
        );
    }
    Ok(())
}

fn standard_slot_wal_buffer_mode(
    context: &str,
    buffer_count: usize,
    len: usize,
) -> io::Result<SlotWalBufferMode> {
    let needed_bytes = estimate_hugetlb_bytes(buffer_count, len)?;
    let (total_pages, free_pages, hugepage_size) = hugepage_meminfo()?;
    let needed_pages = needed_bytes.div_ceil(hugepage_size);
    if needed_pages != 0 && free_pages >= needed_pages {
        return Ok(SlotWalBufferMode::HugeTlb);
    }

    eprintln!(
        "warning: {context} standard mode wants hugetlb buffers needing about {} ({needed_pages} huge pages of {}), but HugePages_Free={free_pages} HugePages_Total={total_pages}; falling back to small-page buffers.",
        format_mib(needed_bytes),
        format_mib(hugepage_size)
    );
    warn_small_page_buffer_mode(context, "not enough free hugetlb pages for standard mode");
    Ok(SlotWalBufferMode::SmallPages)
}

fn parse_slot_wal_buffer_mode_or_standard(
    value: Option<String>,
    context: &str,
    buffer_count: usize,
    len: usize,
) -> io::Result<SlotWalBufferMode> {
    match value {
        Some(value) => {
            let mode = SlotWalBufferMode::parse(&value)?;
            match mode {
                SlotWalBufferMode::SmallPages => {
                    warn_small_page_buffer_mode(context, "small-pages requested explicitly");
                }
                SlotWalBufferMode::HugeTlb => {
                    warn_hugetlb_pressure(context, estimate_hugetlb_bytes(buffer_count, len)?)?;
                }
            }
            Ok(mode)
        }
        None => standard_slot_wal_buffer_mode(context, buffer_count, len),
    }
}

fn checked_buffer_count(units: usize, buffers_per_unit: usize, context: &str) -> io::Result<usize> {
    units.checked_mul(buffers_per_unit).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{context} buffer count overflow"),
        )
    })
}

fn parse_size_arg(value: &str, name: &str) -> io::Result<usize> {
    let value = value.trim();
    if value.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must not be empty"),
        ));
    }

    let lower = value.to_ascii_lowercase();
    let (digits, multiplier) = if let Some(digits) = lower.strip_suffix("kib") {
        (digits, 1024u128)
    } else if let Some(digits) = lower.strip_suffix("kb") {
        (digits, 1000u128)
    } else if let Some(digits) = lower.strip_suffix('k') {
        (digits, 1024u128)
    } else if let Some(digits) = lower.strip_suffix("mib") {
        (digits, 1024u128 * 1024)
    } else if let Some(digits) = lower.strip_suffix("mb") {
        (digits, 1000u128 * 1000)
    } else if let Some(digits) = lower.strip_suffix('m') {
        (digits, 1024u128 * 1024)
    } else if let Some(digits) = lower.strip_suffix("gib") {
        (digits, 1024u128 * 1024 * 1024)
    } else if let Some(digits) = lower.strip_suffix("gb") {
        (digits, 1000u128 * 1000 * 1000)
    } else if let Some(digits) = lower.strip_suffix('g') {
        (digits, 1024u128 * 1024 * 1024)
    } else {
        (lower.as_str(), 1u128)
    };

    let base = digits.replace('_', "").parse::<u128>().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name}={value:?} is not a valid byte count: {err}"),
        )
    })?;
    usize::try_from(base.checked_mul(multiplier).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name}={value:?} is too large"),
        )
    })?)
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name}={value:?} does not fit in usize"),
        )
    })
}

fn parse_bool_arg(value: &str, name: &str) -> io::Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name}={other:?} is not a boolean; use true/false"),
        )),
    }
}

fn env_size_opt(name: &str) -> io::Result<Option<usize>> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => parse_size_arg(&value, name).map(Some),
        _ => Ok(None),
    }
}

fn ensure_sector_aligned(value: usize, name: &str) -> io::Result<()> {
    if value == 0 || value % SECTOR_SIZE != 0 {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be a non-zero multiple of {SECTOR_SIZE}"),
        ))
    } else {
        Ok(())
    }
}

fn block_device_size(fd: i32) -> io::Result<u64> {
    let mut bytes = 0u64;
    let ret = unsafe { libc::ioctl(fd, BLKGETSIZE64, &mut bytes) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(bytes)
    }
}

fn default_hugepage_size() -> io::Result<usize> {
    let meminfo = fs::read_to_string("/proc/meminfo")?;
    for line in meminfo.lines() {
        if let Some(value) = line.strip_prefix("Hugepagesize:") {
            let kb = value
                .trim()
                .strip_suffix("kB")
                .unwrap_or(value.trim())
                .trim()
                .parse::<usize>()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            return kb.checked_mul(1024).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Hugepagesize is too large")
            });
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Hugepagesize missing from /proc/meminfo",
    ))
}

fn linux_dev_major(dev: u64) -> u64 {
    ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)
}

fn linux_dev_minor(dev: u64) -> u64 {
    (dev & 0xff) | ((dev >> 12) & !0xff)
}

fn linux_dev_major_minor(metadata: &fs::Metadata) -> String {
    let dev = metadata.rdev();
    format!("{}:{}", linux_dev_major(dev), linux_dev_minor(dev))
}

fn block_sysfs_dir(metadata: &fs::Metadata) -> PathBuf {
    Path::new("/sys/dev/block").join(linux_dev_major_minor(metadata))
}

fn canonical_block_sysfs_dir(metadata: &fs::Metadata) -> io::Result<PathBuf> {
    fs::canonicalize(block_sysfs_dir(metadata))
}

fn disk_sysfs_dir(metadata: &fs::Metadata) -> io::Result<PathBuf> {
    let block_dir = canonical_block_sysfs_dir(metadata)?;
    if block_dir.join("queue").is_dir() || block_dir.join("mq").is_dir() {
        return Ok(block_dir);
    }

    let parent = block_dir
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "block sysfs parent missing"))?;
    if parent.join("queue").is_dir() || parent.join("mq").is_dir() {
        return Ok(parent.to_path_buf());
    }

    Ok(block_dir)
}

fn queue_sysfs_dir(metadata: &fs::Metadata) -> io::Result<PathBuf> {
    let block_dir = canonical_block_sysfs_dir(metadata)?;
    let direct_queue = block_dir.join("queue");
    if direct_queue.is_dir() {
        return Ok(direct_queue);
    }

    let parent_queue = block_dir
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "block sysfs parent missing"))?
        .join("queue");
    if parent_queue.is_dir() {
        return Ok(parent_queue);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no queue sysfs directory for {}", block_dir.display()),
    ))
}

fn read_sysfs_usize<P: AsRef<Path>>(path: P) -> io::Result<usize> {
    fs::read_to_string(path)?
        .trim()
        .parse::<usize>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn read_sysfs_i32<P: AsRef<Path>>(path: P) -> io::Result<i32> {
    fs::read_to_string(path)?
        .trim()
        .parse::<i32>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn parse_cpu_list(value: &str) -> io::Result<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in value.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start = start.trim().parse::<usize>().map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid CPU list start {start:?}: {err}"),
                )
            })?;
            let end = end.trim().parse::<usize>().map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid CPU list end {end:?}: {err}"),
                )
            })?;
            if start > end {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid CPU range {start}-{end}"),
                ));
            }
            cpus.extend(start..=end);
        } else {
            cpus.push(part.parse::<usize>().map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid CPU list entry {part:?}: {err}"),
                )
            })?);
        }
    }

    cpus.sort_unstable();
    cpus.dedup();
    Ok(cpus)
}

fn format_cpu_list(cpus: &[usize]) -> String {
    if cpus.is_empty() {
        return "none".to_string();
    }

    let mut sorted = cpus.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut ranges = Vec::new();
    let mut start = sorted[0];
    let mut prev = sorted[0];
    for cpu in sorted.into_iter().skip(1) {
        if cpu == prev + 1 {
            prev = cpu;
            continue;
        }
        if start == prev {
            ranges.push(start.to_string());
        } else {
            ranges.push(format!("{start}-{prev}"));
        }
        start = cpu;
        prev = cpu;
    }
    if start == prev {
        ranges.push(start.to_string());
    } else {
        ranges.push(format!("{start}-{prev}"));
    }

    ranges.join(",")
}

fn online_cpu_ids() -> Vec<usize> {
    fs::read_to_string("/sys/devices/system/cpu/online")
        .ok()
        .and_then(|value| parse_cpu_list(&value).ok())
        .filter(|cpus| !cpus.is_empty())
        .unwrap_or_else(|| (0..online_cpu_count()).collect())
}

#[derive(Clone, Debug)]
struct BlockQueueInfo {
    index: usize,
    cpus: Vec<usize>,
}

#[derive(Clone, Debug)]
struct BlockDeviceTopology {
    block_dir: PathBuf,
    disk_dir: PathBuf,
    device_numa_node: Option<i32>,
    queues: Vec<BlockQueueInfo>,
    online_cpus: Vec<usize>,
}

impl BlockDeviceTopology {
    fn from_metadata(metadata: &fs::Metadata) -> io::Result<Self> {
        let block_dir = canonical_block_sysfs_dir(metadata)?;
        let disk_dir = disk_sysfs_dir(metadata)?;
        let online_cpus = online_cpu_ids();
        let device_numa_node = read_device_numa_node(&disk_dir);
        let mut queues = read_block_mq_queues(&disk_dir)?;
        if queues.is_empty() {
            queues.push(BlockQueueInfo {
                index: 0,
                cpus: online_cpus.clone(),
            });
        }

        Ok(Self {
            block_dir,
            disk_dir,
            device_numa_node,
            queues,
            online_cpus,
        })
    }

    fn planned_cpu(&self, worker: usize) -> usize {
        if let Some(cpu) = affinity_target_cpu(worker) {
            return cpu;
        }

        let queue = &self.queues[worker % self.queues.len()];
        if !queue.cpus.is_empty() {
            return queue.cpus[(worker / self.queues.len()) % queue.cpus.len()];
        }

        self.online_cpus[worker % self.online_cpus.len()]
    }

    fn planned_cpus(&self, workers: usize, avoid_smt: bool) -> Vec<usize> {
        let explicit_pin_list = env_truthy("URING_PLAY_PIN_CPUS")
            && env::var("URING_PLAY_PIN_CPU_LIST")
                .ok()
                .is_some_and(|value| !value.trim().is_empty());
        if explicit_pin_list || !avoid_smt {
            return (0..workers)
                .map(|worker| self.planned_cpu(worker))
                .collect();
        }

        let mut planned = Vec::with_capacity(workers);
        for worker in 0..workers {
            let fallback = self.planned_cpu(worker);
            let queue = &self.queues[worker % self.queues.len()];
            let candidates = if queue.cpus.is_empty() {
                &self.online_cpus
            } else {
                &queue.cpus
            };
            let mut selected = None;
            for cpu in candidates.iter().copied() {
                if cpu_has_smt_conflict(cpu, &planned) {
                    continue;
                }
                let cpu_numa = cpu_numa_node(cpu);
                if self.device_numa_node.is_none()
                    || cpu_numa.is_none()
                    || self.device_numa_node == cpu_numa
                {
                    selected = Some(cpu);
                    break;
                }
            }
            if selected.is_none() {
                selected = candidates
                    .iter()
                    .copied()
                    .find(|cpu| !cpu_has_smt_conflict(*cpu, &planned));
            }
            if selected.is_none() {
                selected = self
                    .online_cpus
                    .iter()
                    .copied()
                    .find(|cpu| !cpu_has_smt_conflict(*cpu, &planned));
            }
            planned.push(selected.unwrap_or(fallback));
        }
        planned
    }

    fn queue_for_cpu(&self, worker: usize, cpu: usize) -> &BlockQueueInfo {
        self.queues
            .iter()
            .find(|queue| queue.cpus.contains(&cpu))
            .unwrap_or_else(|| &self.queues[worker % self.queues.len()])
    }
}

fn cpu_has_smt_conflict(cpu: usize, chosen: &[usize]) -> bool {
    let siblings = thread_siblings(cpu);
    chosen.iter().copied().any(|chosen_cpu| {
        chosen_cpu == cpu
            || siblings.contains(&chosen_cpu)
            || thread_siblings(chosen_cpu).contains(&cpu)
    })
}

fn read_block_mq_queues(disk_dir: &Path) -> io::Result<Vec<BlockQueueInfo>> {
    let mq_dir = disk_dir.join("mq");
    let Ok(entries) = fs::read_dir(&mq_dir) else {
        return Ok(Vec::new());
    };

    let mut queues = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(|name| name.to_string()) else {
            continue;
        };
        let Ok(index) = name.parse::<usize>() else {
            continue;
        };
        let cpu_list = read_trimmed_path(entry.path().join("cpu_list")).unwrap_or_default();
        let cpus = parse_cpu_list(&cpu_list)?;
        queues.push(BlockQueueInfo { index, cpus });
    }
    queues.sort_by_key(|queue| queue.index);
    Ok(queues)
}

fn read_device_numa_node(disk_dir: &Path) -> Option<i32> {
    read_sysfs_i32(disk_dir.join("device/numa_node"))
        .ok()
        .filter(|node| *node >= 0)
}

fn cpu_numa_node(cpu: usize) -> Option<i32> {
    let cpu_dir = Path::new("/sys/devices/system/cpu").join(format!("cpu{cpu}"));
    let entries = fs::read_dir(cpu_dir).ok()?;
    entries.flatten().find_map(|entry| {
        let name = entry.file_name().into_string().ok()?;
        let node = name.strip_prefix("node")?.parse::<i32>().ok()?;
        (node >= 0).then_some(node)
    })
}

fn option_i32_label(value: Option<i32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheCopyMicroMode {
    Copy,
    CopyPrefetchT0,
    CopyPrefetchNta,
    CopyNonTemporal,
    CopyNonTemporalPrefetchNta,
    CopyChecksumScalar,
    CopyChecksumAvx2,
    ChecksumScalar,
    ChecksumAvx2,
    WriteOnly,
}

impl CacheCopyMicroMode {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "copy" => Ok(Self::Copy),
            "copy-prefetch-t0" | "prefetch-t0" => Ok(Self::CopyPrefetchT0),
            "copy-prefetch-nta" | "prefetch-nta" => Ok(Self::CopyPrefetchNta),
            "copy-nt" | "nt-copy" | "non-temporal" => Ok(Self::CopyNonTemporal),
            "copy-nt-prefetch-nta" | "nt-prefetch-nta" => Ok(Self::CopyNonTemporalPrefetchNta),
            "copy-checksum-scalar" => Ok(Self::CopyChecksumScalar),
            "copy-checksum-avx2" => {
                if checksum_avx2_available() {
                    Ok(Self::CopyChecksumAvx2)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "copy-checksum-avx2 requested but AVX2 is unavailable",
                    ))
                }
            }
            "checksum" | "checksum-scalar" => Ok(Self::ChecksumScalar),
            "checksum-avx2" => {
                if checksum_avx2_available() {
                    Ok(Self::ChecksumAvx2)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "checksum-avx2 requested but AVX2 is unavailable",
                    ))
                }
            }
            "write" | "write-only" | "memset" => Ok(Self::WriteOnly),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown cache copy microbench mode {other:?}; use copy, copy-prefetch-t0, \
                     copy-prefetch-nta, copy-nt, copy-nt-prefetch-nta, copy-checksum-scalar, \
                     copy-checksum-avx2, checksum-scalar, checksum-avx2, or write-only"
                ),
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::CopyPrefetchT0 => "copy-prefetch-t0",
            Self::CopyPrefetchNta => "copy-prefetch-nta",
            Self::CopyNonTemporal => "copy-nt",
            Self::CopyNonTemporalPrefetchNta => "copy-nt-prefetch-nta",
            Self::CopyChecksumScalar => "copy-checksum-scalar",
            Self::CopyChecksumAvx2 => "copy-checksum-avx2",
            Self::ChecksumScalar => "checksum-scalar",
            Self::ChecksumAvx2 => "checksum-avx2",
            Self::WriteOnly => "write-only",
        }
    }

    fn read_multiplier(self) -> usize {
        match self {
            Self::Copy
            | Self::CopyPrefetchT0
            | Self::CopyPrefetchNta
            | Self::CopyNonTemporal
            | Self::CopyNonTemporalPrefetchNta
            | Self::ChecksumScalar
            | Self::ChecksumAvx2 => 1,
            Self::CopyChecksumScalar | Self::CopyChecksumAvx2 => 2,
            Self::WriteOnly => 0,
        }
    }

    fn write_multiplier(self) -> usize {
        match self {
            Self::ChecksumScalar | Self::ChecksumAvx2 => 0,
            _ => 1,
        }
    }
}

fn cache_prefetch_t0(_ptr: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch(_ptr.cast(), std::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(target_arch = "x86")]
    unsafe {
        std::arch::x86::_mm_prefetch(_ptr.cast(), std::arch::x86::_MM_HINT_T0);
    }
}

fn cache_prefetch_nta(_ptr: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch(_ptr.cast(), std::arch::x86_64::_MM_HINT_NTA);
    }
    #[cfg(target_arch = "x86")]
    unsafe {
        std::arch::x86::_mm_prefetch(_ptr.cast(), std::arch::x86::_MM_HINT_NTA);
    }
}

fn cache_prefetch_range(ptr: *const u8, len: usize, nta: bool) {
    let prefetch_len = len.min(1024);
    let mut offset = 0usize;
    while offset < prefetch_len {
        let prefetch_ptr = unsafe { ptr.add(offset) };
        if nta {
            cache_prefetch_nta(prefetch_ptr);
        } else {
            cache_prefetch_t0(prefetch_ptr);
        }
        offset += 64;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn copy_non_temporal_x86_64(src: *const u8, dst: *mut u8, len: usize) {
    use std::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_sfence, _mm_stream_si128};

    let mut offset = 0usize;
    while offset + 16 <= len {
        let value = unsafe { _mm_loadu_si128(src.add(offset).cast::<__m128i>()) };
        unsafe { _mm_stream_si128(dst.add(offset).cast::<__m128i>(), value) };
        offset += 16;
    }
    if offset < len {
        unsafe { ptr::copy_nonoverlapping(src.add(offset), dst.add(offset), len - offset) };
    }
    unsafe { _mm_sfence() };
}

fn copy_non_temporal(src: *const u8, dst: *mut u8, len: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if (dst as usize) & 15 == 0 {
            unsafe { copy_non_temporal_x86_64(src, dst, len) };
            return;
        }
    }

    unsafe { ptr::copy_nonoverlapping(src, dst, len) };
}

fn cache_copy_micro_apply(
    mode: CacheCopyMicroMode,
    src: *const u8,
    dst: *mut u8,
    len: usize,
) -> u64 {
    match mode {
        CacheCopyMicroMode::Copy => unsafe {
            ptr::copy_nonoverlapping(src, dst, len);
            ptr::read_volatile(dst) as u64
        },
        CacheCopyMicroMode::CopyPrefetchT0 => unsafe {
            cache_prefetch_range(src, len, false);
            ptr::copy_nonoverlapping(src, dst, len);
            ptr::read_volatile(dst) as u64
        },
        CacheCopyMicroMode::CopyPrefetchNta => unsafe {
            cache_prefetch_range(src, len, true);
            ptr::copy_nonoverlapping(src, dst, len);
            ptr::read_volatile(dst) as u64
        },
        CacheCopyMicroMode::CopyNonTemporal => unsafe {
            copy_non_temporal(src, dst, len);
            ptr::read_volatile(dst) as u64
        },
        CacheCopyMicroMode::CopyNonTemporalPrefetchNta => unsafe {
            cache_prefetch_range(src, len, true);
            copy_non_temporal(src, dst, len);
            ptr::read_volatile(dst) as u64
        },
        CacheCopyMicroMode::CopyChecksumScalar => unsafe {
            let data = slice::from_raw_parts(src, len);
            let checksum = checksum_bytes_scalar(data);
            ptr::copy_nonoverlapping(src, dst, len);
            checksum.wrapping_add(ptr::read_volatile(dst) as u64)
        },
        CacheCopyMicroMode::CopyChecksumAvx2 => unsafe {
            let data = slice::from_raw_parts(src, len);
            let checksum = checksum_bytes(ZcrxConsumeMode::ChecksumAvx2, data);
            ptr::copy_nonoverlapping(src, dst, len);
            checksum.wrapping_add(ptr::read_volatile(dst) as u64)
        },
        CacheCopyMicroMode::ChecksumScalar => unsafe {
            checksum_bytes_scalar(slice::from_raw_parts(src, len))
        },
        CacheCopyMicroMode::ChecksumAvx2 => unsafe {
            checksum_bytes(
                ZcrxConsumeMode::ChecksumAvx2,
                slice::from_raw_parts(src, len),
            )
        },
        CacheCopyMicroMode::WriteOnly => unsafe {
            ptr::write_bytes(dst, 0xa5, len);
            ptr::read_volatile(dst) as u64
        },
    }
}

struct CacheCopyMicroWorkerResult {
    worker: usize,
    cpu_plan: Option<usize>,
    bytes_per_worker: usize,
    chunk_bytes: usize,
    chunks: usize,
    iterations: usize,
    logical_bytes: usize,
    read_bytes: usize,
    write_bytes: usize,
    checksum: u64,
    elapsed: Duration,
    cpu: Duration,
    target_cpu: i32,
    affinity_applied: bool,
    start_cpu: i32,
    end_cpu: i32,
    memory_policy: &'static str,
    buffer_alignment: usize,
    voluntary_switches: u64,
    involuntary_switches: u64,
    migrations: u64,
}

#[allow(clippy::too_many_arguments)]
fn cache_copy_micro_worker(
    worker: usize,
    cpu_plan: Option<usize>,
    bytes_per_worker: usize,
    chunk_bytes: usize,
    iterations: usize,
    mode: CacheCopyMicroMode,
    buffer_mode: SlotWalBufferMode,
    pin: bool,
) -> io::Result<CacheCopyMicroWorkerResult> {
    let affinity =
        pin_current_thread_if_requested_to_cpu("cache-copy-worker", worker, pin, cpu_plan);
    let preferred_numa_node = if affinity.target_cpu >= 0 {
        cpu_numa_node(affinity.target_cpu as usize)
    } else {
        None
    };
    let chunks = bytes_per_worker / chunk_bytes;
    let src = buffer_mode.allocate_for_worker(chunks, chunk_bytes, preferred_numa_node)?;
    let dst = buffer_mode.allocate_for_worker(chunks, chunk_bytes, preferred_numa_node)?;
    src.fill_each(chunk_bytes, |buf| unsafe {
        ptr::write_bytes(
            buf.as_mut_ptr(),
            (worker as u8).wrapping_mul(17).wrapping_add(1),
            buf.len(),
        );
    });
    dst.fill_each(chunk_bytes, |buf| unsafe {
        ptr::write_bytes(buf.as_mut_ptr(), 0, buf.len());
    });

    let tid = current_tid();
    let start_switches = read_thread_context_switches(tid).unwrap_or_default();
    let start_cpu_time = thread_cpu_time().unwrap_or_default();
    let start_cpu = current_cpu();
    let started = Instant::now();
    let mut checksum = 0u64;
    for _ in 0..iterations {
        for chunk in 0..chunks {
            checksum = checksum.wrapping_add(cache_copy_micro_apply(
                mode,
                src.ptr(chunk).cast_const(),
                dst.ptr(chunk),
                chunk_bytes,
            ));
        }
    }
    std::hint::black_box(checksum);
    let elapsed = started.elapsed();
    let cpu = thread_cpu_time().unwrap_or(start_cpu_time) - start_cpu_time;
    let end_cpu = current_cpu();
    let end_switches = read_thread_context_switches(tid).unwrap_or(start_switches);
    let logical_bytes = bytes_per_worker
        .checked_mul(iterations)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "logical byte overflow"))?;
    let read_bytes = logical_bytes
        .checked_mul(mode.read_multiplier())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "read byte overflow"))?;
    let write_bytes = logical_bytes
        .checked_mul(mode.write_multiplier())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "write byte overflow"))?;
    let buffer_alignment = address_alignment(src.base_addr().min(dst.base_addr()));
    Ok(CacheCopyMicroWorkerResult {
        worker,
        cpu_plan,
        bytes_per_worker,
        chunk_bytes,
        chunks,
        iterations,
        logical_bytes,
        read_bytes,
        write_bytes,
        checksum,
        elapsed,
        cpu,
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
        start_cpu,
        end_cpu,
        memory_policy: src.memory_policy(),
        buffer_alignment,
        voluntary_switches: end_switches
            .voluntary
            .saturating_sub(start_switches.voluntary),
        involuntary_switches: end_switches
            .involuntary
            .saturating_sub(start_switches.involuntary),
        migrations: end_switches
            .migrations
            .saturating_sub(start_switches.migrations),
    })
}

#[allow(clippy::too_many_arguments)]
fn run_cache_copy_microbench_with_plan(
    label: &str,
    workers: usize,
    cpu_plan: Option<Vec<usize>>,
    bytes_per_worker: usize,
    chunk_bytes: usize,
    iterations: usize,
    mode: CacheCopyMicroMode,
    buffer_mode: SlotWalBufferMode,
    pin: bool,
) -> io::Result<Vec<CacheCopyMicroWorkerResult>> {
    if workers == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workers must be non-zero",
        ));
    }
    if chunk_bytes == 0 || bytes_per_worker == 0 || bytes_per_worker % chunk_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "bytes-per-worker={bytes_per_worker} must be a non-zero multiple of chunk-bytes={chunk_bytes}"
            ),
        ));
    }
    if iterations == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "iterations must be non-zero",
        ));
    }
    let cpu_label = cpu_plan
        .as_ref()
        .map(|cpus| format_cpu_list(cpus))
        .unwrap_or_else(|| "configured".to_string());
    println!(
        "cache-copy-microbench-plan: label={label} workers={workers} cpu_plan={cpu_label} \
         bytes_per_worker={bytes_per_worker} chunk_bytes={chunk_bytes} iterations={iterations} \
         mode={} buffer_mode={} pin={}",
        mode.label(),
        buffer_mode.as_str(),
        yes(pin)
    );

    let mut handles = Vec::with_capacity(workers);
    for worker in 0..workers {
        let cpu = cpu_plan.as_ref().and_then(|cpus| {
            if cpus.is_empty() {
                None
            } else {
                Some(cpus[worker % cpus.len()])
            }
        });
        handles.push(thread::spawn(move || {
            cache_copy_micro_worker(
                worker,
                cpu,
                bytes_per_worker,
                chunk_bytes,
                iterations,
                mode,
                buffer_mode,
                pin,
            )
        }));
    }

    let mut results = Vec::with_capacity(workers);
    for handle in handles {
        results.push(handle.join().map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "cache copy microbench worker panicked",
            )
        })??);
    }
    results.sort_by_key(|result| result.worker);

    let mut total_logical = 0usize;
    let mut total_read = 0usize;
    let mut total_write = 0usize;
    let mut total_cpu = Duration::ZERO;
    let mut max_elapsed = Duration::ZERO;
    let mut checksum = 0u64;
    for result in &results {
        let seconds = result.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        let cpu_secs = result.cpu.as_secs_f64();
        let cpu_pct = if seconds > 0.0 {
            (cpu_secs / seconds) * 100.0
        } else {
            0.0
        };
        println!(
            "cache-copy-microbench-worker: label={label} worker={} cpu_plan={} \
             bytes_per_worker={} chunk_bytes={} chunks={} iterations={} \
             logical_bytes={} read_bytes={} write_bytes={} checksum={} \
             seconds={seconds:.6} thread_cpu_seconds={cpu_secs:.6} cpu_wall_pct={cpu_pct:.1} \
             logical_Gbitps={:.3} read_Gbitps={:.3} write_Gbitps={:.3} \
             target_cpu={} affinity_applied={} start_cpu={} end_cpu={} \
             memory_policy={} buffer_alignment={} voluntary_ctxt_switches={} \
             involuntary_ctxt_switches={} migrations={}",
            result.worker,
            result
                .cpu_plan
                .map(|cpu| cpu.to_string())
                .unwrap_or_else(|| "configured".to_string()),
            result.bytes_per_worker,
            result.chunk_bytes,
            result.chunks,
            result.iterations,
            result.logical_bytes,
            result.read_bytes,
            result.write_bytes,
            result.checksum,
            (result.logical_bytes as f64 * 8.0 / 1_000_000_000.0) / seconds,
            (result.read_bytes as f64 * 8.0 / 1_000_000_000.0) / seconds,
            (result.write_bytes as f64 * 8.0 / 1_000_000_000.0) / seconds,
            result.target_cpu,
            result.affinity_applied,
            result.start_cpu,
            result.end_cpu,
            result.memory_policy,
            result.buffer_alignment,
            result.voluntary_switches,
            result.involuntary_switches,
            result.migrations
        );
        total_logical = total_logical.saturating_add(result.logical_bytes);
        total_read = total_read.saturating_add(result.read_bytes);
        total_write = total_write.saturating_add(result.write_bytes);
        total_cpu += result.cpu;
        max_elapsed = max_elapsed.max(result.elapsed);
        checksum = checksum.wrapping_add(result.checksum);
    }
    let seconds = max_elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
    let cpu_secs = total_cpu.as_secs_f64();
    println!(
        "cache-copy-microbench: label={label} workers={workers} mode={} buffer_mode={} \
         logical_bytes={total_logical} read_bytes={total_read} write_bytes={total_write} \
         checksum={checksum} seconds={seconds:.6} total_thread_cpu_seconds={cpu_secs:.6} \
         logical_Gbitps={:.3} read_Gbitps={:.3} write_Gbitps={:.3}",
        mode.label(),
        buffer_mode.as_str(),
        (total_logical as f64 * 8.0 / 1_000_000_000.0) / seconds,
        (total_read as f64 * 8.0 / 1_000_000_000.0) / seconds,
        (total_write as f64 * 8.0 / 1_000_000_000.0) / seconds
    );
    Ok(results)
}

fn cache_copy_microbench(
    workers: usize,
    bytes_per_worker: usize,
    chunk_bytes: usize,
    iterations: usize,
    mode: CacheCopyMicroMode,
    buffer_mode: SlotWalBufferMode,
    pin: bool,
) -> io::Result<()> {
    run_cache_copy_microbench_with_plan(
        "configured",
        workers,
        None,
        bytes_per_worker,
        chunk_bytes,
        iterations,
        mode,
        buffer_mode,
        pin,
    )?;
    Ok(())
}

fn thread_siblings(cpu: usize) -> Vec<usize> {
    read_trimmed_path(format!(
        "/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
    ))
    .and_then(|value| parse_cpu_list(&value).ok())
    .filter(|siblings| !siblings.is_empty())
    .unwrap_or_else(|| vec![cpu])
}

fn first_smt_cpu_pair() -> Option<(usize, usize)> {
    let online = online_cpu_ids();
    for cpu in &online {
        let siblings = thread_siblings(*cpu);
        if let Some(sibling) = siblings
            .into_iter()
            .find(|sibling| sibling != cpu && online.contains(sibling))
        {
            return Some((*cpu, sibling));
        }
    }
    None
}

fn first_non_sibling_cpu_pair(base: usize, sibling: usize) -> Option<(usize, usize)> {
    let online = online_cpu_ids();
    let siblings = thread_siblings(base);
    let base_numa = cpu_numa_node(base);
    online
        .iter()
        .copied()
        .find(|cpu| {
            *cpu != base
                && *cpu != sibling
                && !siblings.contains(cpu)
                && base_numa.is_some()
                && cpu_numa_node(*cpu) == base_numa
        })
        .or_else(|| {
            online
                .iter()
                .copied()
                .find(|cpu| *cpu != base && *cpu != sibling && !siblings.contains(cpu))
        })
        .map(|cpu| (base, cpu))
}

fn cache_smt_microbench(
    bytes_per_worker: usize,
    chunk_bytes: usize,
    iterations: usize,
    mode: CacheCopyMicroMode,
    buffer_mode: SlotWalBufferMode,
) -> io::Result<()> {
    let (base, sibling) = first_smt_cpu_pair().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no online SMT sibling CPU pair found",
        )
    })?;
    let (separate_a, separate_b) = first_non_sibling_cpu_pair(base, sibling).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no online non-sibling CPU pair found for SMT comparison",
        )
    })?;
    println!(
        "cache-smt-microbench-plan: smt_pair={base},{sibling} smt_siblings={} \
         separate_pair={separate_a},{separate_b} separate_siblings={} base_numa={} separate_numa={}",
        format_cpu_list(&thread_siblings(base)),
        format_cpu_list(&thread_siblings(separate_a)),
        option_i32_label(cpu_numa_node(base)),
        option_i32_label(cpu_numa_node(separate_b))
    );
    run_cache_copy_microbench_with_plan(
        "smt-siblings",
        2,
        Some(vec![base, sibling]),
        bytes_per_worker,
        chunk_bytes,
        iterations,
        mode,
        buffer_mode,
        true,
    )?;
    run_cache_copy_microbench_with_plan(
        "separate-cores",
        2,
        Some(vec![separate_a, separate_b]),
        bytes_per_worker,
        chunk_bytes,
        iterations,
        mode,
        buffer_mode,
        true,
    )?;
    Ok(())
}

#[cfg(test)]
mod topology_tests {
    use super::*;

    #[test]
    fn cpu_list_parser_handles_ranges_and_dedupes() {
        assert_eq!(
            parse_cpu_list("0-2,4,4,6-7").unwrap(),
            vec![0, 1, 2, 4, 6, 7]
        );
    }

    #[test]
    fn cpu_list_formatter_compacts_ranges() {
        assert_eq!(format_cpu_list(&[4, 2, 1, 0, 7, 6]), "0-2,4,6-7");
    }

    #[test]
    fn wal_region_validator_rejects_overlap() {
        let regions = [
            WalRegionPlan {
                worker: 0,
                base_offset: 0,
                len_bytes: 4096,
            },
            WalRegionPlan {
                worker: 1,
                base_offset: 2048,
                len_bytes: 4096,
            },
        ];
        assert!(validate_wal_regions(&regions, 16 * 1024).is_err());
    }

    #[test]
    fn tcp_mux_live_selector_finds_balanced_combination() {
        let conn_counts = vec![
            vec![4, 0, 0, 0],
            vec![2, 0, 2, 0],
            vec![0, 2, 0, 2],
            vec![0, 0, 4, 0],
        ];
        let (selected, counts) =
            tcp_mux_find_best_conn_combination(&conn_counts, 2, 4).expect("best combination");
        assert_eq!(selected, vec![1, 2]);
        assert_eq!(counts, vec![2, 2, 2, 2]);
    }

    #[test]
    fn tcp_mux_lane_selector_can_choose_different_candidates_per_lane() {
        let lane_conn_counts = vec![vec![vec![1, 0], vec![0, 1]], vec![vec![1, 0], vec![0, 1]]];
        let (selected, counts, mode, combinations) =
            tcp_mux_find_best_lane_conn_combinations(&lane_conn_counts, 1, 2, 100)
                .expect("best per-lane combination");
        assert_eq!(selected, vec![vec![0], vec![1]]);
        assert_eq!(counts, vec![1, 1]);
        assert_eq!(mode, "per-lane-exhaustive");
        assert_eq!(combinations, 4);
    }

    #[test]
    fn tcp_mux_combination_count_respects_cap() {
        assert_eq!(tcp_mux_combination_count_capped(17, 2, 1_000_000), 136);
        assert_eq!(tcp_mux_combination_count_capped(17, 4, 1_000_000), 2380);
        assert_eq!(tcp_mux_combination_count_capped(64, 8, 1000), 1001);
        assert_eq!(tcp_mux_combination_product_capped(4, 8, 1_000_000), 65536);
        assert_eq!(
            tcp_mux_combination_product_capped(28, 8, 1_000_000),
            1_000_001
        );
    }

    #[test]
    fn zcrx_wal_write_user_data_round_trips() {
        let user_data = tcp_wal_zcrx_write_user_data(17);
        assert_eq!(tcp_wal_zcrx_write_slot(user_data), Some(17));
        assert_eq!(tcp_wal_zcrx_write_slot(17), None);
    }

    #[test]
    fn zcrx_auto_slot_stride_scales_for_large_areas() {
        let area_size = 64usize * 1024 * 1024 * 1024;
        let stride = tcp_wal_zcrx_auto_slot_stride(area_size, 4096, 512, false, 0).unwrap();
        assert_eq!(stride, 4 * 1024 * 1024);
        assert!(area_size / stride <= IORING_MAX_REG_BUFFERS);
    }

    #[test]
    fn zcrx_configured_slot_stride_must_divide_area() {
        assert!(tcp_wal_zcrx_auto_slot_stride(8 * 1024 * 1024, 12 * 1024, 4096, true, 0).is_err());
    }

    #[test]
    fn zcrx_auto_slot_stride_reserves_bounce_buffers() {
        let stride = tcp_wal_zcrx_auto_slot_stride(64 * 1024 * 1024, 4096, 512, false, 64).unwrap();
        assert_eq!(stride, 8192);
        assert!(64 * 1024 * 1024 / stride + 64 <= IORING_MAX_REG_BUFFERS);
    }

    #[test]
    fn zcrx_auto_slot_stride_allows_full_table_without_registered_bounce() {
        let stride = tcp_wal_zcrx_auto_slot_stride(64 * 1024 * 1024, 4096, 512, false, 0).unwrap();
        assert_eq!(stride, 4096);
        assert_eq!(64 * 1024 * 1024 / stride, IORING_MAX_REG_BUFFERS);
    }

    #[test]
    fn zcrx_initial_slot_stride_tracks_rx_buffer_size_by_default() {
        assert_eq!(
            tcp_wal_zcrx_initial_slot_stride(4096, false, 8192, 512),
            8192
        );
        assert_eq!(
            tcp_wal_zcrx_initial_slot_stride(4096, false, 4096, 512),
            4096
        );
        assert_eq!(
            tcp_wal_zcrx_initial_slot_stride(4096, true, 8192, 512),
            4096
        );
    }

    #[test]
    fn zcrx_direct_segments_allow_aligned_subslot_offsets() {
        let range = tcp_wal_zcrx_direct_segment_range(4096, 4096, 4096, 8192, 4)
            .unwrap()
            .expect("direct subslot segment range");
        assert_eq!(
            range,
            TcpWalZcrxDirectRange {
                aligned_offset: 4096,
                physical_len: 4096,
                first_buf_index: 0,
                segments: 1,
            }
        );

        let range = tcp_wal_zcrx_direct_segment_range(4096, 8192, 4096, 8192, 4)
            .unwrap()
            .expect("direct cross-slot segment range");
        assert_eq!(
            range,
            TcpWalZcrxDirectRange {
                aligned_offset: 4096,
                physical_len: 8192,
                first_buf_index: 0,
                segments: 2,
            }
        );

        let range = tcp_wal_zcrx_direct_segment_range(8192, 8192, 4096, 8192, 4)
            .unwrap()
            .expect("direct segment range");
        assert_eq!(
            range,
            TcpWalZcrxDirectRange {
                aligned_offset: 8192,
                physical_len: 8192,
                first_buf_index: 1,
                segments: 1,
            }
        );
    }

    #[test]
    fn zcrx_direct_segments_wait_for_busy_slots() {
        let mut busy = vec![false; 4];
        assert!(!tcp_wal_zcrx_direct_slots_busy(0, 2, &busy).unwrap());

        busy[1] = true;
        assert!(tcp_wal_zcrx_direct_slots_busy(0, 2, &busy).unwrap());
        assert!(!tcp_wal_zcrx_direct_slots_busy(2, 1, &busy).unwrap());
        assert!(tcp_wal_zcrx_direct_slots_busy(3, 2, &busy).is_err());
    }

    #[test]
    fn zcrx_direct_mode_requires_recycle_headroom() {
        assert!(
            tcp_wal_zcrx_direct_fits_without_recycling(
                2,
                16 * 1024 * 1024,
                64 * 1024 * 1024,
                8192,
                16
            )
            .unwrap()
        );
        assert!(
            !tcp_wal_zcrx_direct_fits_without_recycling(
                4,
                16 * 1024 * 1024,
                64 * 1024 * 1024,
                8192,
                16
            )
            .unwrap()
        );
    }

    #[test]
    fn zcrx_direct_segments_pad_unaligned_ranges() {
        let range = tcp_wal_zcrx_direct_segment_range(512, 4096, 4096, 8192, 4)
            .unwrap()
            .expect("padded direct range");
        assert_eq!(
            range,
            TcpWalZcrxDirectRange {
                aligned_offset: 0,
                physical_len: 8192,
                first_buf_index: 0,
                segments: 1,
            }
        );

        let range = tcp_wal_zcrx_direct_segment_range(4096, 512, 4096, 8192, 4)
            .unwrap()
            .expect("padded direct range");
        assert_eq!(
            range,
            TcpWalZcrxDirectRange {
                aligned_offset: 4096,
                physical_len: 4096,
                first_buf_index: 0,
                segments: 1,
            }
        );
    }

    #[test]
    fn zcrx_direct_full_chunk_rejects_partial_or_unaligned_frames() {
        assert!(
            tcp_wal_zcrx_direct_full_chunk_segment_range(0, 4096, 4096, 4096, 8192, 4)
                .unwrap()
                .is_some()
        );
        assert!(
            tcp_wal_zcrx_direct_full_chunk_segment_range(0, 2048, 4096, 4096, 8192, 4)
                .unwrap()
                .is_none()
        );
        assert!(
            tcp_wal_zcrx_direct_full_chunk_segment_range(512, 4096, 4096, 4096, 8192, 4)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn observed_wal_regions_follow_stream_counts() {
        let regions = make_observed_wal_regions(&[2, 0, 1], 4096, 8192, 8192 + 3 * 4096)
            .expect("observed regions");
        assert_eq!(
            regions,
            vec![
                WalRegionPlan {
                    worker: 0,
                    base_offset: 8192,
                    len_bytes: 8192,
                },
                WalRegionPlan {
                    worker: 1,
                    base_offset: 16384,
                    len_bytes: 0,
                },
                WalRegionPlan {
                    worker: 2,
                    base_offset: 16384,
                    len_bytes: 4096,
                },
            ]
        );
    }

    #[test]
    fn observed_wal_regions_reject_too_small_region() {
        assert!(make_observed_wal_regions(&[2], 4096, 0, 4096).is_err());
    }
}

fn ensure_aligned_to(value: usize, alignment: usize, name: &str) -> io::Result<()> {
    if value == 0 || value % alignment != 0 {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be a non-zero multiple of {alignment}"),
        ))
    } else {
        Ok(())
    }
}

fn validate_slot_wal_geometry_alignment(
    metadata: &fs::Metadata,
    total_bytes: usize,
    chunk_bytes: usize,
) -> io::Result<usize> {
    let queue_dir = queue_sysfs_dir(metadata)?;
    let logical_block = read_sysfs_usize(queue_dir.join("logical_block_size"))?;
    let physical_block = read_sysfs_usize(queue_dir.join("physical_block_size"))?;
    let minimum_io = read_sysfs_usize(queue_dir.join("minimum_io_size")).unwrap_or(logical_block);
    let optimal_io = read_sysfs_usize(queue_dir.join("optimal_io_size")).unwrap_or(0);
    let required_alignment = logical_block
        .max(physical_block)
        .max(minimum_io)
        .max(SECTOR_SIZE);

    ensure_aligned_to(total_bytes, required_alignment, "total-bytes")?;
    ensure_aligned_to(chunk_bytes, required_alignment, "chunk-bytes")?;

    let block_dir = block_sysfs_dir(metadata);
    let alignment_offset = read_sysfs_usize(block_dir.join("alignment_offset")).unwrap_or(0);
    if alignment_offset != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("block device alignment_offset={alignment_offset}; refusing unaligned raw IO"),
        ));
    }

    if let Ok(start_sectors) = read_sysfs_usize(block_dir.join("start")) {
        let start_bytes_mod = (start_sectors as u128).saturating_mul(logical_block as u128)
            % required_alignment as u128;
        if start_bytes_mod != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "partition start is misaligned: start_sectors={start_sectors} \
                     logical_block_size={logical_block} required_alignment={required_alignment}"
                ),
            ));
        }
    }

    if optimal_io != 0 && chunk_bytes % optimal_io != 0 {
        eprintln!(
            "warning: chunk-bytes={chunk_bytes} is aligned to required geometry \
             ({required_alignment}) but not optimal_io_size={optimal_io}"
        );
    }

    Ok(required_alignment)
}

fn validate_slot_wal_queue_limits(
    metadata: &fs::Metadata,
    chunk_bytes: usize,
    segment_bytes: usize,
    buffer_mode: SlotWalBufferMode,
) -> io::Result<()> {
    let queue_dir = queue_sysfs_dir(metadata)?;

    if let Ok(max_segments) = read_sysfs_usize(queue_dir.join("max_segments")) {
        let needed_segments = chunk_bytes.div_ceil(segment_bytes);
        if needed_segments > max_segments {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "chunk-bytes={chunk_bytes} needs {needed_segments} {} segments, but block \
                     queue max_segments={max_segments}; use <= {} bytes or a larger buffer backing",
                    buffer_mode.as_str(),
                    max_segments.saturating_mul(segment_bytes)
                ),
            ));
        }
    }

    if let Ok(max_hw_sectors_kb) = read_sysfs_usize(queue_dir.join("max_hw_sectors_kb")) {
        let max_hw_bytes = max_hw_sectors_kb.saturating_mul(1024);
        if chunk_bytes > max_hw_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "chunk-bytes={chunk_bytes} exceeds block queue max_hw_sectors_kb={max_hw_sectors_kb}"
                ),
            ));
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SlotWalTargetKind {
    NullBlock,
    PartUuid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlotWalTarget {
    open_path: PathBuf,
    label: String,
    kind: SlotWalTargetKind,
}

impl SlotWalTarget {
    fn null_block(path: &str) -> Self {
        Self {
            open_path: PathBuf::from(path),
            label: path.to_owned(),
            kind: SlotWalTargetKind::NullBlock,
        }
    }

    fn from_partuuid(uuid: &str) -> io::Result<Self> {
        let uuid = normalize_partuuid(uuid)?;
        Ok(Self {
            open_path: Path::new(BY_PARTUUID_DIR).join(&uuid),
            label: format!("PARTUUID={uuid}"),
            kind: SlotWalTargetKind::PartUuid(uuid),
        })
    }

    fn open_path(&self) -> &Path {
        &self.open_path
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn is_null_block(&self) -> bool {
        matches!(self.kind, SlotWalTargetKind::NullBlock)
    }

    fn partuuid(&self) -> Option<&str> {
        match &self.kind {
            SlotWalTargetKind::PartUuid(uuid) => Some(uuid),
            SlotWalTargetKind::NullBlock => None,
        }
    }
}

fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    if value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&value[prefix.len()..])
    } else {
        None
    }
}

fn normalize_partuuid(value: &str) -> io::Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || !trimmed
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PARTUUID must be non-empty and contain only ASCII letters, digits, and '-'",
        ));
    }

    Ok(trimmed.to_ascii_lowercase())
}

fn parse_slot_wal_target(arg: &str) -> io::Result<SlotWalTarget> {
    if arg.starts_with("/dev/nullb") {
        return Ok(SlotWalTarget::null_block(arg));
    }

    if let Some(uuid) =
        strip_prefix_ci(arg, "PARTUUID=").or_else(|| strip_prefix_ci(arg, "partuuid:"))
    {
        return SlotWalTarget::from_partuuid(uuid);
    }

    let path = Path::new(arg);
    if let Ok(remainder) = path.strip_prefix(BY_PARTUUID_DIR) {
        if remainder.components().count() == 1 {
            if let Some(uuid) = remainder.file_name().and_then(|name| name.to_str()) {
                return SlotWalTarget::from_partuuid(uuid);
            }
        }
    }

    if arg.contains('-') && !arg.contains('/') {
        return SlotWalTarget::from_partuuid(arg);
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "slot WAL bench targets must be /dev/nullbN for synthetic tests or \
         PARTUUID=<uuid> for real raw block devices",
    ))
}

fn raw_partition_allowlist_path() -> PathBuf {
    if let Ok(path) = env::var("URING_PLAY_RAW_PARTITION_ALLOWLIST") {
        return PathBuf::from(path);
    }

    let cwd_path = PathBuf::from(RAW_PARTITION_ALLOWLIST);
    if cwd_path.exists() {
        return cwd_path;
    }

    Path::new(env!("CARGO_MANIFEST_DIR")).join(RAW_PARTITION_ALLOWLIST)
}

fn parse_partuuid_token(value: &str) -> io::Result<String> {
    let value = strip_prefix_ci(value.trim(), "PARTUUID=")
        .or_else(|| strip_prefix_ci(value.trim(), "partuuid:"))
        .unwrap_or(value.trim());
    normalize_partuuid(value)
}

fn allowed_raw_partuuids() -> io::Result<Vec<String>> {
    let path = raw_partition_allowlist_path();
    let contents = fs::read_to_string(&path).map_err(|err| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "cannot read raw partition allowlist {}: {err}",
                path.display()
            ),
        )
    })?;

    let mut allowed = Vec::new();
    for (line_idx, line) in contents.lines().enumerate() {
        let token = line.split('#').next().unwrap_or("").trim();
        if token.is_empty() {
            continue;
        }

        allowed.push(parse_partuuid_token(token).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid PARTUUID in raw partition allowlist {} line {}: {err}",
                    path.display(),
                    line_idx + 1
                ),
            )
        })?);
    }

    if allowed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("raw partition allowlist {} is empty", path.display()),
        ));
    }

    Ok(allowed)
}

fn ensure_partuuid_allowlisted(target_uuid: &str) -> io::Result<()> {
    let allowed = allowed_raw_partuuids()?;
    if allowed.iter().any(|uuid| uuid == target_uuid) {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "PARTUUID={target_uuid} is not in {}; refusing real raw block target",
            raw_partition_allowlist_path().display()
        ),
    ))
}

fn known_block_signature(prefix: &[u8]) -> Option<&'static str> {
    if prefix.get(0..6) == Some(b"LUKS\xba\xbe") {
        return Some("LUKS");
    }
    if prefix.get(0..4) == Some(b"XFSB") {
        return Some("XFS");
    }
    if prefix.get(3..11) == Some(b"NTFS    ") {
        return Some("NTFS");
    }
    if prefix.get(3..11) == Some(b"EXFAT   ") {
        return Some("exFAT");
    }
    if prefix.get(3..9) == Some(b"FVE-FS") {
        return Some("BitLocker");
    }
    if matches!(prefix.get(54..59), Some(b"FAT12" | b"FAT16"))
        || prefix.get(82..87) == Some(b"FAT32")
    {
        return Some("FAT");
    }
    if prefix.get(1080..1082) == Some(&[0x53, 0xef]) {
        return Some("ext filesystem");
    }
    if prefix.get(0x10040..0x10048) == Some(b"_BHRfS_M") {
        return Some("Btrfs");
    }
    if prefix.get(32..36) == Some(b"NXSB") {
        return Some("APFS");
    }

    None
}

fn ensure_no_known_filesystem_signature(target: &SlotWalTarget) -> io::Result<()> {
    let mut file = fs::File::open(target.open_path())?;
    let mut prefix = vec![0u8; 128 * 1024];
    let read = file.read(&mut prefix)?;
    prefix.truncate(read);

    if let Some(signature) = known_block_signature(&prefix) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing raw write to {} because it has a {signature} signature",
                target.label()
            ),
        ));
    }

    Ok(())
}

fn validate_raw_block_write_allowed(target: &SlotWalTarget) -> io::Result<()> {
    if target.is_null_block() {
        return Ok(());
    }

    let Some(target_uuid) = target.partuuid() else {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "raw block writes require a PARTUUID target",
        ));
    };

    ensure_partuuid_allowlisted(target_uuid)?;

    if !env_truthy("URING_PLAY_ALLOW_RAW_BLOCK_WRITE") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "raw block writes to PARTUUID={target_uuid} are destructive; set \
                 URING_PLAY_ALLOW_RAW_BLOCK_WRITE=1 to override"
            ),
        ));
    }

    let expected = env::var("URING_PLAY_RAW_TARGET_PARTUUID").map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "raw block writes to PARTUUID={target_uuid} require explicit confirmation: \
                 set URING_PLAY_RAW_TARGET_PARTUUID={target_uuid}"
            ),
        )
    })?;
    let expected = normalize_partuuid(&expected)?;
    if expected != target_uuid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "URING_PLAY_RAW_TARGET_PARTUUID={expected} does not match PARTUUID={target_uuid}"
            ),
        ));
    }

    Ok(())
}

fn validate_slot_wal_write_target_safety(
    target: &SlotWalTarget,
    metadata: &fs::Metadata,
) -> io::Result<()> {
    validate_raw_block_write_allowed(target)?;
    if target.is_null_block() {
        return Ok(());
    }

    let major_minor = linux_dev_major_minor(metadata);
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.get(2) == Some(&major_minor.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refusing raw write to mounted {}", target.label()),
            ));
        }
    }

    let holders_dir = format!("/sys/dev/block/{major_minor}/holders");
    if let Ok(mut entries) = fs::read_dir(&holders_dir)
        && entries.next().transpose()?.is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing raw write to {} while it has device holders",
                target.label()
            ),
        ));
    }

    let target_canonical = fs::canonicalize(target.open_path()).ok();
    if let Ok(swaps) = fs::read_to_string("/proc/swaps") {
        for line in swaps.lines().skip(1) {
            let Some(swap_path) = line.split_whitespace().next() else {
                continue;
            };
            if swap_path == target.open_path().to_string_lossy()
                || target_canonical
                    .as_ref()
                    .is_some_and(|path| fs::canonicalize(swap_path).ok().as_ref() == Some(path))
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("refusing raw write to active swap {}", target.label()),
                ));
            }
        }
    }

    ensure_no_known_filesystem_signature(target)?;

    Ok(())
}

fn fill_slot_wal_buffers(buffers: &FixedSendBuffers, chunk_bytes: usize) {
    buffers.fill_each(chunk_bytes, |buf| {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for word in buf.chunks_exact_mut(8) {
            seed = seed
                .wrapping_mul(0xbf58_476d_1ce4_e5b9)
                .wrapping_add(0x94d0_49bb_1331_11ebu64);
            word.copy_from_slice(&seed.to_le_bytes());
        }
        let remainder = buf.len() & 7;
        if remainder != 0 {
            let start = buf.len() - remainder;
            let tail = seed.to_le_bytes();
            buf[start..].copy_from_slice(&tail[..remainder]);
        }
    });
}

#[derive(Clone, Copy)]
struct SlotWalWorkerResult {
    worker: usize,
    bytes: usize,
    ops: usize,
    elapsed: Duration,
    base_offset: u64,
    region_bytes: usize,
    slot_count: usize,
    target_cpu: i32,
    affinity_applied: bool,
    local_cpu: i32,
    worker_numa_node: Option<i32>,
    completion_batch: usize,
    buffer_base: usize,
    buffer_stride: usize,
    buffer_map_len: usize,
    buffer_alignment: usize,
    buffer_mode: SlotWalBufferMode,
    segment_bytes: usize,
    memory_policy: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UringWriteMode {
    Write,
    WriteFixed,
    WriteFixedFile,
}

impl UringWriteMode {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "write" | "normal" | "plain" => Ok(Self::Write),
            "fixed" | "write-fixed" | "fixedbuf" | "fixedbufs" => Ok(Self::WriteFixed),
            "fixed-file" | "fixedfile" | "fixedbufs-registerfiles" | "fio-fixed" => {
                Ok(Self::WriteFixedFile)
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown uring write mode {other:?}; use write, fixed, or fixed-file"),
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::WriteFixed => "fixed",
            Self::WriteFixedFile => "fixed-file",
        }
    }

    fn uses_registered_buffers(self) -> bool {
        matches!(self, Self::WriteFixed | Self::WriteFixedFile)
    }

    fn uses_registered_file(self) -> bool {
        matches!(self, Self::WriteFixedFile)
    }
}

struct UringWriteWorkerResult {
    worker: usize,
    bytes: usize,
    ops: usize,
    elapsed: Duration,
    base_offset: u64,
    completion_batch: usize,
    target_cpu: i32,
    affinity_applied: bool,
    local_cpu: i32,
    buffer_base: usize,
    buffer_stride: usize,
    buffer_map_len: usize,
    buffer_alignment: usize,
    memory_policy: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WalRegionPlan {
    worker: usize,
    base_offset: u64,
    len_bytes: usize,
}

impl WalRegionPlan {
    fn end_offset(self) -> io::Result<u64> {
        self.base_offset
            .checked_add(self.len_bytes as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WAL region end overflow"))
    }
}

fn validate_slot_wal_common(
    target_arg: &str,
    total_bytes: usize,
    chunk_bytes: usize,
    mode: SlotWalMode,
    buffer_mode: SlotWalBufferMode,
) -> io::Result<(SlotWalTarget, u64, usize, usize)> {
    ensure_sector_aligned(total_bytes, "total-bytes")?;
    ensure_sector_aligned(chunk_bytes, "chunk-bytes")?;
    if total_bytes % chunk_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "total-bytes must be an exact multiple of chunk-bytes",
        ));
    }

    let target = parse_slot_wal_target(target_arg)?;
    if let Some(partuuid) = target.partuuid() {
        ensure_partuuid_allowlisted(partuuid)?;
    }
    let metadata = fs::metadata(target.open_path())?;
    if !metadata.file_type().is_block_device() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "slot WAL bench target must resolve to a raw block device",
        ));
    }
    if matches!(mode, SlotWalMode::Write) {
        validate_slot_wal_write_target_safety(&target, &metadata)?;
    }
    let required_alignment =
        validate_slot_wal_geometry_alignment(&metadata, total_bytes, chunk_bytes)?;
    let segment_bytes = buffer_mode.segment_bytes()?;
    validate_slot_wal_queue_limits(&metadata, chunk_bytes, segment_bytes, buffer_mode)?;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(target.open_path())?;
    let device_bytes = block_device_size(file.as_raw_fd())?;
    if total_bytes as u64 > device_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("total-bytes exceeds block device size {device_bytes}"),
        ));
    }

    Ok((target, device_bytes, required_alignment, segment_bytes))
}

fn validate_slot_wal_plan_common(
    target_arg: &str,
    total_bytes: usize,
    chunk_bytes: usize,
    buffer_mode: SlotWalBufferMode,
) -> io::Result<(SlotWalTarget, fs::Metadata, u64, usize, usize)> {
    ensure_sector_aligned(total_bytes, "total-bytes")?;
    ensure_sector_aligned(chunk_bytes, "chunk-bytes")?;
    if total_bytes % chunk_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "total-bytes must be an exact multiple of chunk-bytes",
        ));
    }

    let target = parse_slot_wal_target(target_arg)?;
    if let Some(partuuid) = target.partuuid() {
        ensure_partuuid_allowlisted(partuuid)?;
    }
    let metadata = fs::metadata(target.open_path())?;
    if !metadata.file_type().is_block_device() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "slot WAL target must resolve to a raw block device",
        ));
    }

    let required_alignment =
        validate_slot_wal_geometry_alignment(&metadata, total_bytes, chunk_bytes)?;
    let segment_bytes = buffer_mode.segment_bytes()?;
    validate_slot_wal_queue_limits(&metadata, chunk_bytes, segment_bytes, buffer_mode)?;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(target.open_path())?;
    let device_bytes = block_device_size(file.as_raw_fd())?;
    if total_bytes as u64 > device_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("total-bytes exceeds block device size {device_bytes}"),
        ));
    }

    Ok((
        target,
        metadata,
        device_bytes,
        required_alignment,
        segment_bytes,
    ))
}

fn make_linear_wal_regions(
    workers: usize,
    bytes_per_worker: usize,
) -> io::Result<Vec<WalRegionPlan>> {
    if workers == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workers must be non-zero",
        ));
    }
    let mut regions = Vec::with_capacity(workers);
    for worker in 0..workers {
        let base_offset = (worker as u64)
            .checked_mul(bytes_per_worker as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?;
        regions.push(WalRegionPlan {
            worker,
            base_offset,
            len_bytes: bytes_per_worker,
        });
    }
    Ok(regions)
}

fn validate_wal_regions(regions: &[WalRegionPlan], device_bytes: u64) -> io::Result<()> {
    if regions.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "at least one WAL region is required",
        ));
    }

    let mut sorted = regions.to_vec();
    sorted.sort_by_key(|region| region.base_offset);
    let mut previous_end = None;
    for region in &sorted {
        if region.len_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("worker {} has an empty WAL region", region.worker),
            ));
        }
        let end = region.end_offset()?;
        if end > device_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "worker {} WAL region [{}..{}) exceeds block device size {device_bytes}",
                    region.worker, region.base_offset, end
                ),
            ));
        }
        if let Some((prev_worker, prev_end)) = previous_end
            && region.base_offset < prev_end
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "WAL regions overlap: worker {prev_worker} ends at {prev_end}, \
                     worker {} starts at {}",
                    region.worker, region.base_offset
                ),
            ));
        }
        previous_end = Some((region.worker, end));
    }

    Ok(())
}

fn address_alignment(addr: usize) -> usize {
    if addr == 0 {
        0
    } else {
        1usize << addr.trailing_zeros()
    }
}

fn validate_slot_wal_pipeline(pipeline: usize, ring_entries: u32) -> io::Result<()> {
    if pipeline == 0 || pipeline > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("pipeline must be between 1 and {}", u16::MAX),
        ));
    }
    if pipeline > ring_entries as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pipeline must be <= ring-entries",
        ));
    }
    Ok(())
}

const BATON_SOURCE_CQE_TAG: u64 = 1u64 << 63;
const BATON_WRITE_CQE_TAG: u64 = 1u64 << 62;
const BATON_CREDIT_CQE_TAG: u64 = 1u64 << 61;
const BATON_VALUE_MASK: u64 = BATON_CREDIT_CQE_TAG - 1;

#[derive(Clone)]
enum BatonSink {
    Null,
    Block(SlotWalTarget),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatonMode {
    RoundTrip,
    Credit { batch: usize },
}

impl BatonMode {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "roundtrip" | "return" | "token" | "per-token" | "1" => Ok(Self::RoundTrip),
            "credit" | "credit:64" | "batch" | "batch:64" => Ok(Self::Credit { batch: 64 }),
            other => {
                if let Some(batch) = other
                    .strip_prefix("credit:")
                    .or_else(|| other.strip_prefix("batch:"))
                    .or_else(|| other.strip_prefix("credits:"))
                {
                    let batch = batch.parse::<usize>().map_err(|err| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid baton credit batch {batch:?}: {err}"),
                        )
                    })?;
                    if batch == 0 || batch > u32::MAX as usize {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "baton credit batch must be between 1 and u32::MAX",
                        ));
                    }
                    return Ok(Self::Credit { batch });
                }

                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "unknown baton mode {other:?}; use roundtrip, credit, or credit:<batch>"
                    ),
                ))
            }
        }
    }

    fn label(self) -> String {
        match self {
            Self::RoundTrip => "roundtrip".to_string(),
            Self::Credit { batch } => format!("credit:{batch}"),
        }
    }

    fn credit_batch(self) -> Option<usize> {
        match self {
            Self::RoundTrip => None,
            Self::Credit { batch } => Some(batch),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatonPinMode {
    Off,
    Pair,
    Hctx,
    HctxLocal,
}

impl BatonPinMode {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "0" | "false" | "no" | "off" | "unpin" => Ok(Self::Off),
            "1" | "true" | "yes" | "pin" | "pair" => Ok(Self::Pair),
            "hctx" | "mq" | "blk-mq" | "block-mq" | "hctx-spread" => Ok(Self::Hctx),
            "hctx-local" | "mq-local" | "blk-mq-local" => Ok(Self::HctxLocal),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown baton pin mode {other:?}; use true, false, hctx, or hctx-local"),
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Pair => "pair",
            Self::Hctx => "hctx",
            Self::HctxLocal => "hctx-local",
        }
    }

    fn pin_requested(self) -> bool {
        !matches!(self, Self::Off)
    }
}

#[derive(Clone, Copy)]
struct BatonPinAssignment {
    producer_cpu: Option<usize>,
    writer_cpu: Option<usize>,
}

struct BatonPinPlan {
    mode: BatonPinMode,
    assignments: Vec<BatonPinAssignment>,
    detail: String,
}

struct BatonTargetPlan {
    label: String,
    worker_sinks: Vec<BatonSink>,
    regions: Vec<WalRegionPlan>,
    segment_bytes: usize,
}

fn baton_hctx_cpu_groups(target: &SlotWalTarget) -> io::Result<Vec<(usize, Vec<usize>)>> {
    let metadata = fs::metadata(target.open_path())?;
    let mq_dir = disk_sysfs_dir(&metadata)?.join("mq");
    let online_cpus = online_cpu_count();
    let mut groups = Vec::new();

    for entry in fs::read_dir(&mq_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(queue_id) = name.parse::<usize>() else {
            continue;
        };
        let cpu_list_path = entry.path().join("cpu_list");
        let Ok(cpu_list) = fs::read_to_string(&cpu_list_path) else {
            continue;
        };
        let mut cpus = parse_cpu_list(&cpu_list)?;
        cpus.retain(|cpu| *cpu < online_cpus);
        if cpus.is_empty() {
            continue;
        }
        // null_blk exposes its poll hctx as an all-CPU map. For baton writes we
        // want the normal submit hctxs that current CPU selection hashes into.
        if cpus.len() >= online_cpus {
            continue;
        }
        groups.push((queue_id, cpus));
    }

    groups.sort_by_key(|(queue_id, _)| *queue_id);
    Ok(groups)
}

fn baton_pin_plan(
    worker_sinks: &[BatonSink],
    mode: BatonPinMode,
    workers: usize,
) -> io::Result<BatonPinPlan> {
    let default_assignments = || {
        vec![
            BatonPinAssignment {
                producer_cpu: None,
                writer_cpu: None,
            };
            workers
        ]
    };

    match mode {
        BatonPinMode::Off | BatonPinMode::Pair => Ok(BatonPinPlan {
            mode,
            assignments: default_assignments(),
            detail: "default-index-map".to_string(),
        }),
        BatonPinMode::Hctx | BatonPinMode::HctxLocal => {
            let mut hctx_cache = BTreeMap::<String, Vec<(usize, Vec<usize>)>>::new();
            let mut target_worker_counts = BTreeMap::<String, usize>::new();
            let mut assignments = Vec::with_capacity(workers);
            for worker in 0..workers {
                let BatonSink::Block(target) = &worker_sinks[worker] else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "hctx pin mode requires block sinks for every worker",
                    ));
                };
                let key = target.open_path().display().to_string();
                if !hctx_cache.contains_key(&key) {
                    let groups = baton_hctx_cpu_groups(target)?;
                    if groups.is_empty() {
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("no blk-mq hctx CPU map found for {}", target.label()),
                        ));
                    }
                    hctx_cache.insert(key.clone(), groups);
                }
                let groups = hctx_cache
                    .get(&key)
                    .expect("hctx cache entry inserted above");
                let pin_index = if matches!(mode, BatonPinMode::HctxLocal) {
                    let local_worker = target_worker_counts.entry(key).or_insert(0);
                    let local_index = *local_worker;
                    *local_worker += 1;
                    local_index
                } else {
                    worker
                };
                let (_, cpus) = &groups[pin_index % groups.len()];
                let round = pin_index / groups.len();
                let writer_cpu = cpus[round % cpus.len()];
                let producer_cpu = if cpus.len() > 1 {
                    Some(cpus[(round + 1) % cpus.len()])
                } else {
                    None
                };
                assignments.push(BatonPinAssignment {
                    producer_cpu,
                    writer_cpu: Some(writer_cpu),
                });
            }

            let hctxs: Vec<String> = hctx_cache
                .iter()
                .map(|(target, groups)| {
                    let maps: Vec<String> = groups
                        .iter()
                        .map(|(queue_id, cpus)| format!("{queue_id}:{}", format_cpu_list(cpus)))
                        .collect();
                    format!("{target} hctxs={} [{}]", groups.len(), maps.join(" "))
                })
                .collect();
            Ok(BatonPinPlan {
                mode,
                assignments,
                detail: hctxs.join("; "),
            })
        }
    }
}

#[derive(Clone, Copy)]
struct BatonProducerResult {
    worker: usize,
    tokens: usize,
    bytes: usize,
    elapsed: Duration,
    target_cpu: i32,
    affinity_applied: bool,
    start_cpu: i32,
    end_cpu: i32,
    cpu: Duration,
    ignored_source_cqes: usize,
    ring_stats: RawRingStats,
}

#[derive(Clone, Copy)]
struct BatonWriterResult {
    worker: usize,
    tokens: usize,
    bytes: usize,
    elapsed: Duration,
    target_cpu: i32,
    affinity_applied: bool,
    start_cpu: i32,
    end_cpu: i32,
    cpu: Duration,
    writes: usize,
    ignored_source_cqes: usize,
    buffer_base: usize,
    buffer_stride: usize,
    buffer_map_len: usize,
    memory_policy: &'static str,
    ring_stats: RawRingStats,
}

#[derive(Clone, Copy)]
struct BatonWorkerResult {
    producer: BatonProducerResult,
    writer: BatonWriterResult,
}

fn baton_source_user_data(slot: usize) -> u64 {
    BATON_SOURCE_CQE_TAG | slot as u64
}

fn baton_write_user_data(slot: usize) -> u64 {
    BATON_WRITE_CQE_TAG | slot as u64
}

fn baton_slot_from_user_data(user_data: u64, pipeline: usize, label: &str) -> io::Result<usize> {
    let slot = (user_data & BATON_VALUE_MASK) as usize;
    if slot >= pipeline {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} returned invalid slot user_data={user_data:#x} slot={slot}"),
        ));
    }
    Ok(slot)
}

fn baton_value_from_user_data(user_data: u64, label: &str) -> io::Result<usize> {
    if user_data & !BATON_VALUE_MASK != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} returned tagged user_data={user_data:#x}"),
        ));
    }
    usize::try_from(user_data & BATON_VALUE_MASK).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} value does not fit usize: {user_data:#x}"),
        )
    })
}

fn baton_send_token(
    ring: &mut RawRing,
    target_ring_fd: i32,
    slot: usize,
    chunk_bytes: u32,
    skip_source_cqe: bool,
) -> io::Result<()> {
    ring.queue_msg_ring(
        target_ring_fd,
        chunk_bytes,
        slot as u64,
        0,
        skip_source_cqe,
        baton_source_user_data(slot),
    )
}

fn baton_send_value(
    ring: &mut RawRing,
    target_ring_fd: i32,
    value: usize,
    chunk_bytes: u32,
    skip_source_cqe: bool,
) -> io::Result<()> {
    if value as u64 > BATON_VALUE_MASK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "baton value exceeds user_data value mask",
        ));
    }
    ring.queue_msg_ring(
        target_ring_fd,
        chunk_bytes,
        value as u64,
        0,
        skip_source_cqe,
        baton_source_user_data(value),
    )
}

fn baton_send_credit(
    ring: &mut RawRing,
    target_ring_fd: i32,
    credit_count: usize,
    skip_source_cqe: bool,
) -> io::Result<()> {
    if credit_count == 0 || credit_count > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "baton credit count must fit in MSG_RING result",
        ));
    }
    ring.queue_msg_ring(
        target_ring_fd,
        credit_count as u32,
        BATON_CREDIT_CQE_TAG,
        0,
        skip_source_cqe,
        baton_source_user_data(credit_count),
    )
}

fn baton_next_completion_ignoring_source(
    ring: &mut RawRing,
    ignored_source_cqes: &mut usize,
) -> io::Result<IoUringCqe32> {
    loop {
        let cqe = ring.wait_cqe()?;
        if cqe.user_data & BATON_SOURCE_CQE_TAG != 0 {
            if cqe.res < 0 {
                return Err(io::Error::from_raw_os_error(-cqe.res));
            }
            *ignored_source_cqes += 1;
            continue;
        }
        return Ok(cqe);
    }
}

#[allow(clippy::too_many_arguments)]
fn uring_baton_producer_worker(
    worker: usize,
    producer_fd_tx: mpsc::Sender<io::Result<i32>>,
    writer_fd_rx: mpsc::Receiver<i32>,
    release_rx: mpsc::Receiver<()>,
    tokens: usize,
    chunk_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    pin_mode: BatonPinMode,
    planned_cpu: Option<usize>,
    baton_mode: BatonMode,
    skip_source_cqe: bool,
    ring_stats_enabled: bool,
) -> io::Result<BatonProducerResult> {
    let affinity = pin_current_thread_if_requested_to_cpu(
        "uring-baton-producer",
        worker * 2,
        pin_mode.pin_requested(),
        planned_cpu,
    );
    let start_cpu = current_cpu();
    let start_thread_cpu = thread_cpu_time().unwrap_or_default();
    let mut ring = RawRing::new_with_stats(
        ring_entries,
        ring_entries.saturating_mul(2),
        ring_stats_enabled,
    )?;
    let producer_fd = ring.fd();
    producer_fd_tx
        .send(Ok(producer_fd))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "baton producer fd channel"))?;
    let writer_fd = writer_fd_rx
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "baton writer fd channel"))?;
    release_rx
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "baton release channel"))?;

    let chunk_bytes_u32 = u32::try_from(chunk_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk-bytes must fit in u32 for MSG_RING res",
        )
    })?;
    let mut free_slots: Vec<usize> = (0..pipeline).rev().collect();
    let mut submitted = 0usize;
    let mut returned = 0usize;
    let mut inflight = 0usize;
    let mut ignored_source_cqes = 0usize;
    let started = Instant::now();

    match baton_mode {
        BatonMode::RoundTrip => {
            while returned < tokens {
                while submitted < tokens && !free_slots.is_empty() {
                    let slot = free_slots.pop().expect("free slot");
                    baton_send_token(&mut ring, writer_fd, slot, chunk_bytes_u32, skip_source_cqe)?;
                    submitted += 1;
                    inflight += 1;
                }
                ring.submit_pending()?;
                if inflight == 0 {
                    break;
                }

                let mut drained = 0usize;
                loop {
                    let cqe = match ring.try_pop_cqe() {
                        Some(cqe) => cqe,
                        None if drained == 0 => baton_next_completion_ignoring_source(
                            &mut ring,
                            &mut ignored_source_cqes,
                        )?,
                        None => break,
                    };
                    drained += 1;
                    if cqe.user_data & BATON_SOURCE_CQE_TAG != 0 {
                        if cqe.res < 0 {
                            return Err(io::Error::from_raw_os_error(-cqe.res));
                        }
                        ignored_source_cqes += 1;
                        continue;
                    }
                    if cqe.res < 0 {
                        return Err(io::Error::from_raw_os_error(-cqe.res));
                    }
                    let slot = baton_slot_from_user_data(cqe.user_data, pipeline, "baton return")?;
                    returned += 1;
                    inflight -= 1;
                    free_slots.push(slot);
                }
            }
        }
        BatonMode::Credit { .. } => {
            let mut free_credit = pipeline;
            while returned < tokens {
                while submitted < tokens && free_credit > 0 {
                    baton_send_value(
                        &mut ring,
                        writer_fd,
                        submitted,
                        chunk_bytes_u32,
                        skip_source_cqe,
                    )?;
                    submitted += 1;
                    inflight += 1;
                    free_credit -= 1;
                }
                ring.submit_pending()?;

                let mut drained = 0usize;
                loop {
                    let cqe = match ring.try_pop_cqe() {
                        Some(cqe) => cqe,
                        None if drained == 0 => baton_next_completion_ignoring_source(
                            &mut ring,
                            &mut ignored_source_cqes,
                        )?,
                        None => break,
                    };
                    drained += 1;
                    if cqe.user_data & BATON_SOURCE_CQE_TAG != 0 {
                        if cqe.res < 0 {
                            return Err(io::Error::from_raw_os_error(-cqe.res));
                        }
                        ignored_source_cqes += 1;
                        continue;
                    }
                    if cqe.res < 0 {
                        return Err(io::Error::from_raw_os_error(-cqe.res));
                    }
                    if cqe.user_data & BATON_CREDIT_CQE_TAG == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "baton credit mode expected credit CQE, got user_data={:#x}",
                                cqe.user_data
                            ),
                        ));
                    }
                    let credit = usize::try_from(cqe.res).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("negative baton credit result {}", cqe.res),
                        )
                    })?;
                    if credit == 0 || credit > inflight {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid baton credit {credit} with inflight={inflight}"),
                        ));
                    }
                    returned += credit;
                    inflight -= credit;
                    free_credit += credit;
                }
            }
        }
    }

    let elapsed = started.elapsed();
    let end_cpu = current_cpu();
    let cpu = thread_cpu_time()
        .unwrap_or(start_thread_cpu)
        .saturating_sub(start_thread_cpu);
    let ring_stats = ring.stats();
    Ok(BatonProducerResult {
        worker,
        tokens,
        bytes: tokens * chunk_bytes,
        elapsed,
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
        start_cpu,
        end_cpu,
        cpu,
        ignored_source_cqes,
        ring_stats,
    })
}

#[allow(clippy::too_many_arguments)]
fn uring_baton_writer_worker(
    worker: usize,
    sink: BatonSink,
    base_offset: u64,
    producer_fd_rx: mpsc::Receiver<i32>,
    writer_fd_tx: mpsc::Sender<io::Result<i32>>,
    release_rx: mpsc::Receiver<()>,
    tokens: usize,
    chunk_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    buffer_mode: SlotWalBufferMode,
    pin_mode: BatonPinMode,
    planned_cpu: Option<usize>,
    baton_mode: BatonMode,
    skip_source_cqe: bool,
    ring_stats_enabled: bool,
) -> io::Result<BatonWriterResult> {
    let affinity = pin_current_thread_if_requested_to_cpu(
        "uring-baton-writer",
        worker * 2 + 1,
        pin_mode.pin_requested(),
        planned_cpu,
    );
    let start_cpu = current_cpu();
    let start_thread_cpu = thread_cpu_time().unwrap_or_default();
    let chunk_bytes_u32 = u32::try_from(chunk_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk-bytes must fit in u32 for SQE len field",
        )
    })?;
    let mut ring = RawRing::new_with_stats(
        ring_entries,
        ring_entries.saturating_mul(2),
        ring_stats_enabled,
    )?;

    let mut file = None;
    let mut slot_ids = Vec::new();
    let preferred_numa_node = if affinity.target_cpu >= 0 {
        cpu_numa_node(affinity.target_cpu as usize)
    } else {
        None
    };
    let buffers = buffer_mode.allocate_for_worker(pipeline, chunk_bytes, preferred_numa_node)?;
    fill_slot_wal_buffers(&buffers, chunk_bytes);
    let buffer_base = buffers.base_addr();
    let buffer_stride = buffers.stride();
    let buffer_map_len = buffers.map_len();
    let memory_policy = buffers.memory_policy();

    if let BatonSink::Block(target) = &sink {
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
            .open(target.open_path())?;
        let fd = opened.as_raw_fd();
        let mut fds = [fd];
        ring.register_files(&mut fds)?;
        let mut iovecs = buffers.iovecs(chunk_bytes);
        ring.register_buffers(&mut iovecs)?;
        for buf_index in 0..pipeline {
            slot_ids.push(ring.register_io_slot(buf_index as u32, 0)?);
        }
        file = Some(opened);
    }

    let writer_fd = ring.fd();
    writer_fd_tx
        .send(Ok(writer_fd))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "baton writer fd channel"))?;
    let producer_fd = producer_fd_rx
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "baton producer fd channel"))?;
    release_rx
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "baton release channel"))?;

    let started = Instant::now();
    let mut received_tokens = 0usize;
    let mut returned_tokens = 0usize;
    let mut writes = 0usize;
    let mut ignored_source_cqes = 0usize;
    let mut completed_prefix = 0usize;
    let mut pending_credit = 0usize;
    let mut completed_tokens = if baton_mode.credit_batch().is_some() {
        vec![false; pipeline]
    } else {
        Vec::new()
    };

    while returned_tokens < tokens {
        let mut drained = 0usize;
        loop {
            let cqe = match ring.try_pop_cqe() {
                Some(cqe) => cqe,
                None if drained == 0 => {
                    baton_next_completion_ignoring_source(&mut ring, &mut ignored_source_cqes)?
                }
                None => break,
            };
            drained += 1;
            if cqe.user_data & BATON_SOURCE_CQE_TAG != 0 {
                if cqe.res < 0 {
                    return Err(io::Error::from_raw_os_error(-cqe.res));
                }
                ignored_source_cqes += 1;
                continue;
            }
            if cqe.res < 0 {
                return Err(io::Error::from_raw_os_error(-cqe.res));
            }

            if cqe.user_data & BATON_WRITE_CQE_TAG != 0 {
                let value = cqe.user_data & BATON_VALUE_MASK;
                let token = value as usize;
                let slot = match baton_mode {
                    BatonMode::RoundTrip => {
                        baton_slot_from_user_data(cqe.user_data, pipeline, "baton write")?
                    }
                    BatonMode::Credit { .. } => token % pipeline,
                };
                if cqe.res != chunk_bytes_u32 as i32 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        format!(
                            "short baton slot write: slot={slot} res={} expected={chunk_bytes}",
                            cqe.res
                        ),
                    ));
                }
                writes += 1;
                match baton_mode {
                    BatonMode::RoundTrip => {
                        baton_send_token(
                            &mut ring,
                            producer_fd,
                            slot,
                            chunk_bytes_u32,
                            skip_source_cqe,
                        )?;
                        returned_tokens += 1;
                    }
                    BatonMode::Credit { batch } => {
                        if token >= tokens {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("baton write token {token} exceeds token count {tokens}"),
                            ));
                        }
                        let complete_slot = token % pipeline;
                        if completed_tokens[complete_slot] {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "baton credit completion bitmap collision at slot {complete_slot}"
                                ),
                            ));
                        }
                        completed_tokens[complete_slot] = true;
                        while completed_prefix < tokens
                            && completed_tokens[completed_prefix % pipeline]
                        {
                            completed_tokens[completed_prefix % pipeline] = false;
                            completed_prefix += 1;
                            pending_credit += 1;
                        }
                        if pending_credit >= batch || completed_prefix == tokens {
                            baton_send_credit(
                                &mut ring,
                                producer_fd,
                                pending_credit,
                                skip_source_cqe,
                            )?;
                            returned_tokens += pending_credit;
                            pending_credit = 0;
                        }
                    }
                }
                continue;
            }

            let token = match baton_mode {
                BatonMode::RoundTrip => {
                    baton_slot_from_user_data(cqe.user_data, pipeline, "baton incoming")?
                }
                BatonMode::Credit { .. } => {
                    let token = baton_value_from_user_data(cqe.user_data, "baton incoming")?;
                    if token >= tokens {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("baton incoming token {token} exceeds token count {tokens}"),
                        ));
                    }
                    token
                }
            };
            let slot = token % pipeline;
            received_tokens += 1;
            match &sink {
                BatonSink::Null => match baton_mode {
                    BatonMode::RoundTrip => {
                        baton_send_token(
                            &mut ring,
                            producer_fd,
                            slot,
                            chunk_bytes_u32,
                            skip_source_cqe,
                        )?;
                        returned_tokens += 1;
                    }
                    BatonMode::Credit { batch } => {
                        completed_tokens[slot] = true;
                        while completed_prefix < tokens
                            && completed_tokens[completed_prefix % pipeline]
                        {
                            completed_tokens[completed_prefix % pipeline] = false;
                            completed_prefix += 1;
                            pending_credit += 1;
                        }
                        if pending_credit >= batch || completed_prefix == tokens {
                            baton_send_credit(
                                &mut ring,
                                producer_fd,
                                pending_credit,
                                skip_source_cqe,
                            )?;
                            returned_tokens += pending_credit;
                            pending_credit = 0;
                        }
                    }
                },
                BatonSink::Block(_) => {
                    let file_offset = base_offset
                        .checked_add((received_tokens - 1) as u64 * chunk_bytes as u64)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "baton WAL offset overflow")
                        })?;
                    ring.queue_slot_rw(
                        slot_ids[slot],
                        0,
                        file_offset,
                        chunk_bytes_u32,
                        io_slots::SlotRw::Write,
                        baton_write_user_data(token),
                    )?;
                }
            }
        }
    }

    ring.submit_pending()?;
    let elapsed = started.elapsed();

    for slot_id in slot_ids {
        ring.unregister_io_slot(slot_id)?;
    }
    if file.is_some() {
        ring.unregister_buffers()?;
        ring.unregister_files()?;
    }

    let end_cpu = current_cpu();
    let cpu = thread_cpu_time()
        .unwrap_or(start_thread_cpu)
        .saturating_sub(start_thread_cpu);
    let ring_stats = ring.stats();
    Ok(BatonWriterResult {
        worker,
        tokens: received_tokens,
        bytes: received_tokens * chunk_bytes,
        elapsed,
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
        start_cpu,
        end_cpu,
        cpu,
        writes,
        ignored_source_cqes,
        buffer_base,
        buffer_stride,
        buffer_map_len,
        memory_policy,
        ring_stats,
    })
}

#[allow(clippy::too_many_arguments)]
fn uring_baton_worker_pair(
    worker: usize,
    sink: BatonSink,
    base_offset: u64,
    tokens: usize,
    chunk_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    buffer_mode: SlotWalBufferMode,
    pin_mode: BatonPinMode,
    pin_assignment: BatonPinAssignment,
    baton_mode: BatonMode,
    skip_source_cqe: bool,
    ring_stats_enabled: bool,
    release_rx: mpsc::Receiver<()>,
) -> io::Result<BatonWorkerResult> {
    let (producer_fd_tx, producer_fd_rx) = mpsc::channel::<io::Result<i32>>();
    let (writer_fd_tx, writer_fd_rx) = mpsc::channel::<io::Result<i32>>();
    let (producer_writer_fd_tx, producer_writer_fd_rx) = mpsc::channel::<i32>();
    let (writer_producer_fd_tx, writer_producer_fd_rx) = mpsc::channel::<i32>();
    let (producer_release_tx, producer_release_rx) = mpsc::channel::<()>();
    let (writer_release_tx, writer_release_rx) = mpsc::channel::<()>();

    let producer_handle = thread::spawn(move || {
        uring_baton_producer_worker(
            worker,
            producer_fd_tx,
            producer_writer_fd_rx,
            producer_release_rx,
            tokens,
            chunk_bytes,
            pipeline,
            ring_entries,
            pin_mode,
            pin_assignment.producer_cpu,
            baton_mode,
            skip_source_cqe,
            ring_stats_enabled,
        )
    });
    let writer_handle = thread::spawn(move || {
        uring_baton_writer_worker(
            worker,
            sink,
            base_offset,
            writer_producer_fd_rx,
            writer_fd_tx,
            writer_release_rx,
            tokens,
            chunk_bytes,
            pipeline,
            ring_entries,
            buffer_mode,
            pin_mode,
            pin_assignment.writer_cpu,
            baton_mode,
            skip_source_cqe,
            ring_stats_enabled,
        )
    });

    let producer_fd = producer_fd_rx
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "producer fd channel"))??;
    let writer_fd = writer_fd_rx
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer fd channel"))??;
    producer_writer_fd_tx
        .send(writer_fd)
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "producer writer-fd channel"))?;
    writer_producer_fd_tx
        .send(producer_fd)
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer producer-fd channel"))?;

    release_rx
        .recv()
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker pair release channel"))?;
    producer_release_tx
        .send(())
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "producer release channel"))?;
    writer_release_tx
        .send(())
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer release channel"))?;

    let producer = producer_handle
        .join()
        .map_err(|_| io::Error::other("uring baton producer thread panicked"))??;
    let writer = writer_handle
        .join()
        .map_err(|_| io::Error::other("uring baton writer thread panicked"))??;
    Ok(BatonWorkerResult { producer, writer })
}

fn uring_baton_target_plan(
    target_arg: &str,
    workers: usize,
    bytes_per_worker: usize,
    chunk_bytes: usize,
    buffer_mode: SlotWalBufferMode,
) -> io::Result<BatonTargetPlan> {
    if target_arg == "null" || target_arg == "discard" || target_arg == "none" {
        let segment_bytes = buffer_mode.segment_bytes()?;
        return Ok(BatonTargetPlan {
            label: "null".to_string(),
            worker_sinks: vec![BatonSink::Null; workers],
            regions: make_linear_wal_regions(workers, bytes_per_worker)?,
            segment_bytes,
        });
    }

    let target_args: Vec<&str> = target_arg
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect();
    if target_args.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "baton target list is empty",
        ));
    }

    if target_args.len() == 1 {
        let total_bytes = workers.checked_mul(bytes_per_worker).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "baton aggregate byte count overflow",
            )
        })?;
        let (target, device_bytes, _required_alignment, segment_bytes) = validate_slot_wal_common(
            target_args[0],
            total_bytes,
            chunk_bytes,
            SlotWalMode::Write,
            buffer_mode,
        )?;
        let regions = make_linear_wal_regions(workers, bytes_per_worker)?;
        validate_wal_regions(&regions, device_bytes)?;
        return Ok(BatonTargetPlan {
            label: target.label().to_string(),
            worker_sinks: vec![BatonSink::Block(target); workers],
            regions,
            segment_bytes,
        });
    }

    let mut target_worker_counts = vec![0usize; target_args.len()];
    for worker in 0..workers {
        target_worker_counts[worker % target_args.len()] += 1;
    }

    let mut targets = Vec::with_capacity(target_args.len());
    let mut segment_bytes = None;
    for (target_idx, target_arg) in target_args.iter().enumerate() {
        let target_bytes = target_worker_counts[target_idx]
            .checked_mul(bytes_per_worker)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "baton target byte count overflow",
                )
            })?;
        let (target, _device_bytes, _required_alignment, target_segment_bytes) =
            validate_slot_wal_common(
                target_arg,
                target_bytes,
                chunk_bytes,
                SlotWalMode::Write,
                buffer_mode,
            )?;
        if let Some(segment_bytes) = segment_bytes {
            if segment_bytes != target_segment_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "multi-target baton segment sizes differ",
                ));
            }
        } else {
            segment_bytes = Some(target_segment_bytes);
        }
        targets.push(target);
    }

    let mut worker_sinks = Vec::with_capacity(workers);
    let mut regions = Vec::with_capacity(workers);
    let mut local_worker_index = vec![0usize; targets.len()];
    for worker in 0..workers {
        let target_idx = worker % targets.len();
        let base_offset = local_worker_index[target_idx]
            .checked_mul(bytes_per_worker)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?
            as u64;
        local_worker_index[target_idx] += 1;
        worker_sinks.push(BatonSink::Block(targets[target_idx].clone()));
        regions.push(WalRegionPlan {
            worker,
            base_offset,
            len_bytes: bytes_per_worker,
        });
    }

    let labels: Vec<&str> = targets.iter().map(|target| target.label()).collect();
    Ok(BatonTargetPlan {
        label: labels.join(","),
        worker_sinks,
        regions,
        segment_bytes: segment_bytes.unwrap_or(buffer_mode.segment_bytes()?),
    })
}

#[allow(clippy::too_many_arguments)]
fn uring_baton_bench(
    target_arg: &str,
    workers: usize,
    bytes_per_worker: usize,
    chunk_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    buffer_mode: SlotWalBufferMode,
    pin_mode: BatonPinMode,
    baton_mode: BatonMode,
) -> io::Result<()> {
    if workers == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workers must be non-zero",
        ));
    }
    validate_slot_wal_pipeline(pipeline, ring_entries)?;
    if let Some(batch) = baton_mode.credit_batch()
        && batch > pipeline
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "baton credit batch must be <= pipeline to avoid credit starvation",
        ));
    }
    ensure_sector_aligned(bytes_per_worker, "bytes-per-worker")?;
    ensure_sector_aligned(chunk_bytes, "chunk-bytes")?;
    if bytes_per_worker % chunk_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bytes-per-worker must be an exact multiple of chunk-bytes",
        ));
    }
    let tokens = bytes_per_worker / chunk_bytes;
    if tokens as u64 > BATON_VALUE_MASK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tokens per worker exceed baton user_data value range",
        ));
    }
    let target_plan = uring_baton_target_plan(
        target_arg,
        workers,
        bytes_per_worker,
        chunk_bytes,
        buffer_mode,
    )?;
    let skip_source_cqe = env_enabled_or("URING_PLAY_BATON_SKIP_SOURCE_CQE", true);
    let ring_stats_enabled = env_truthy("URING_PLAY_BATON_RING_STATS");
    let pin_plan = baton_pin_plan(&target_plan.worker_sinks, pin_mode, workers)?;

    println!(
        "uring-baton-bench: sink={} workers={workers} bytes_per_worker={bytes_per_worker} \
         total_bytes={} chunk_bytes={chunk_bytes} tokens_per_worker={tokens} \
         pipeline={pipeline} ring_entries={ring_entries} buffer_mode={} \
         segment_bytes={} pin_mode={} baton_mode={} pin_detail=\"{}\" \
         skip_source_cqe={} ring_stats={} msg_ring_op={IORING_OP_MSG_RING}",
        target_plan.label,
        workers * bytes_per_worker,
        buffer_mode.as_str(),
        target_plan.segment_bytes,
        pin_plan.mode.as_str(),
        baton_mode.label(),
        pin_plan.detail,
        yes(skip_source_cqe),
        yes(ring_stats_enabled)
    );

    let mut release_senders = Vec::with_capacity(workers);
    let mut handles = Vec::with_capacity(workers);
    for region in target_plan.regions {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        release_senders.push(release_tx);
        let sink = target_plan.worker_sinks[region.worker].clone();
        let pin_mode = pin_plan.mode;
        let pin_assignment = pin_plan.assignments[region.worker];
        handles.push(thread::spawn(move || {
            uring_baton_worker_pair(
                region.worker,
                sink,
                region.base_offset,
                tokens,
                chunk_bytes,
                pipeline,
                ring_entries,
                buffer_mode,
                pin_mode,
                pin_assignment,
                baton_mode,
                skip_source_cqe,
                ring_stats_enabled,
                release_rx,
            )
        }));
    }

    let started = Instant::now();
    for release_tx in release_senders {
        release_tx
            .send(())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "baton release broadcast"))?;
    }

    let mut results = Vec::with_capacity(workers);
    for handle in handles {
        results.push(
            handle
                .join()
                .map_err(|_| io::Error::other("uring baton worker pair thread panicked"))??,
        );
    }
    results.sort_by_key(|result| result.producer.worker);
    let wall = started.elapsed();
    let wall_seconds = wall.as_secs_f64().max(f64::MIN_POSITIVE);
    let total_bytes: usize = results.iter().map(|result| result.writer.bytes).sum();
    let total_tokens: usize = results.iter().map(|result| result.writer.tokens).sum();
    let total_writes: usize = results.iter().map(|result| result.writer.writes).sum();

    for result in &results {
        let producer_secs = result.producer.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        let writer_secs = result.writer.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        println!(
            "uring-baton-producer: worker={} tokens={} bytes={} seconds={producer_secs:.6} \
             token_ops_per_sec={:.0} Gbitps={:.3} target_cpu={} affinity_applied={} \
             start_cpu={} end_cpu={} thread_cpu_seconds={:.6} ignored_source_cqes={}",
            result.producer.worker,
            result.producer.tokens,
            result.producer.bytes,
            result.producer.tokens as f64 / producer_secs,
            result.producer.bytes as f64 * 8.0 / 1_000_000_000.0 / producer_secs,
            result.producer.target_cpu,
            result.producer.affinity_applied,
            result.producer.start_cpu,
            result.producer.end_cpu,
            result.producer.cpu.as_secs_f64(),
            result.producer.ignored_source_cqes
        );
        println!(
            "uring-baton-writer: worker={} tokens={} bytes={} writes={} seconds={writer_secs:.6} \
             token_ops_per_sec={:.0} Gbitps={:.3} target_cpu={} affinity_applied={} \
             start_cpu={} end_cpu={} thread_cpu_seconds={:.6} buffer_base=0x{:x} \
             buffer_stride={} buffer_map_len={} memory_policy={} ignored_source_cqes={}",
            result.writer.worker,
            result.writer.tokens,
            result.writer.bytes,
            result.writer.writes,
            result.writer.tokens as f64 / writer_secs,
            result.writer.bytes as f64 * 8.0 / 1_000_000_000.0 / writer_secs,
            result.writer.target_cpu,
            result.writer.affinity_applied,
            result.writer.start_cpu,
            result.writer.end_cpu,
            result.writer.cpu.as_secs_f64(),
            result.writer.buffer_base,
            result.writer.buffer_stride,
            result.writer.buffer_map_len,
            result.writer.memory_policy,
            result.writer.ignored_source_cqes
        );
        if ring_stats_enabled {
            let ps = result.producer.ring_stats;
            let ws = result.writer.ring_stats;
            println!(
                "uring-baton-ring-stats: role=producer worker={} sqes={} submitted={} \
                 submit_syscalls={} wait_syscalls={} wait_cqe_calls={} cqes={} \
                 try_pop_empty={} submit_short={} cqe_spin_loops={}",
                result.producer.worker,
                ps.sqes_queued,
                ps.sqes_submitted,
                ps.submit_syscalls,
                ps.wait_syscalls,
                ps.wait_cqe_calls,
                ps.cqes_popped,
                ps.try_pop_empty,
                ps.submit_short,
                ps.cqe_spin_loops,
            );
            println!(
                "uring-baton-ring-stats: role=writer worker={} sqes={} submitted={} \
                 submit_syscalls={} wait_syscalls={} wait_cqe_calls={} cqes={} \
                 try_pop_empty={} submit_short={} cqe_spin_loops={}",
                result.writer.worker,
                ws.sqes_queued,
                ws.sqes_submitted,
                ws.submit_syscalls,
                ws.wait_syscalls,
                ws.wait_cqe_calls,
                ws.cqes_popped,
                ws.try_pop_empty,
                ws.submit_short,
                ws.cqe_spin_loops,
            );
        }
    }

    println!(
        "uring-baton-bench-summary: sink={} workers={workers} tokens={total_tokens} \
         writes={total_writes} bytes={total_bytes} wall_seconds={wall_seconds:.6} \
         token_ops_per_sec={:.0} MiBps={:.2} Gbitps={:.3}",
        target_plan.label,
        total_tokens as f64 / wall_seconds,
        total_bytes as f64 / (1024.0 * 1024.0) / wall_seconds,
        total_bytes as f64 * 8.0 / 1_000_000_000.0 / wall_seconds
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn uring_write_bench_worker(
    target: SlotWalTarget,
    worker: usize,
    base_offset: u64,
    total_bytes: usize,
    chunk_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    buffer_mode: SlotWalBufferMode,
    write_mode: UringWriteMode,
    pin_workers: bool,
) -> io::Result<UringWriteWorkerResult> {
    let affinity = pin_current_thread_if_requested("uring-write-worker", worker, pin_workers);
    let chunk_bytes_u32 = u32::try_from(chunk_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk-bytes must fit in u32 for write SQEs",
        )
    })?;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(target.open_path())?;
    let fd = file.as_raw_fd();

    let mut ring = RawRing::new(ring_entries, ring_entries.saturating_mul(2))?;
    let mut registered_fds = [fd];
    if write_mode.uses_registered_file() {
        ring.register_files(&mut registered_fds)?;
    }

    let preferred_numa_node = if affinity.target_cpu >= 0 {
        cpu_numa_node(affinity.target_cpu as usize)
    } else {
        None
    };
    let buffers = buffer_mode.allocate_for_worker(pipeline, chunk_bytes, preferred_numa_node)?;
    fill_slot_wal_buffers(&buffers, chunk_bytes);
    let buffer_base = buffers.base_addr();
    let buffer_stride = buffers.stride();
    let buffer_map_len = buffers.map_len();
    let buffer_alignment = address_alignment(buffer_base);
    let memory_policy = buffers.memory_policy();
    if write_mode.uses_registered_buffers() {
        let mut iovecs = buffers.iovecs(chunk_bytes);
        ring.register_buffers(&mut iovecs)?;
    }

    let completion_batch =
        env_usize_or("URING_PLAY_URING_WRITE_COMPLETION_BATCH", 64).clamp(1, pipeline);
    let total_ops = total_bytes / chunk_bytes;
    let mut free_slots: Vec<usize> = (0..pipeline).rev().collect();
    let mut submitted = 0usize;
    let mut completed = 0usize;
    let local_cpu = current_cpu();
    let started = Instant::now();

    while completed < total_ops {
        while submitted < total_ops && !free_slots.is_empty() {
            let slot = free_slots.pop().expect("free slot");
            let file_offset = (submitted as u64)
                .checked_mul(chunk_bytes as u64)
                .and_then(|offset| base_offset.checked_add(offset))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?;
            let buf = buffers.ptr(slot).cast_const();
            match write_mode {
                UringWriteMode::Write => {
                    ring.queue_write(fd, buf, chunk_bytes_u32, file_offset, slot as u64)?;
                }
                UringWriteMode::WriteFixed => {
                    ring.queue_write_fixed(
                        fd,
                        buf,
                        chunk_bytes_u32,
                        file_offset,
                        slot as u16,
                        slot as u64,
                    )?;
                }
                UringWriteMode::WriteFixedFile => {
                    ring.queue_write_fixed_file(
                        0,
                        buf,
                        chunk_bytes_u32,
                        file_offset,
                        slot as u16,
                        slot as u64,
                    )?;
                }
            }
            submitted += 1;
        }

        ring.submit_pending()?;
        for batch_idx in 0..completion_batch {
            let cqe = if batch_idx == 0 {
                match ring.try_pop_cqe() {
                    Some(cqe) => cqe,
                    None => ring.wait_cqe()?,
                }
            } else {
                match ring.try_pop_cqe() {
                    Some(cqe) => cqe,
                    None => break,
                }
            };
            let slot = cqe.user_data as usize;
            if slot >= pipeline {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("uring write CQE returned invalid slot user_data={slot}"),
                ));
            }
            if cqe.res < 0 {
                return Err(io::Error::from_raw_os_error(-cqe.res));
            }
            if cqe.res != chunk_bytes_u32 as i32 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!(
                        "short uring write completion: res={} expected={chunk_bytes}",
                        cqe.res
                    ),
                ));
            }
            completed += 1;
            free_slots.push(slot);
            if completed == total_ops {
                break;
            }
        }
    }

    let elapsed = started.elapsed();
    if write_mode.uses_registered_buffers() {
        ring.unregister_buffers()?;
    }
    if write_mode.uses_registered_file() {
        ring.unregister_files()?;
    }

    Ok(UringWriteWorkerResult {
        worker,
        bytes: total_bytes,
        ops: total_ops,
        elapsed,
        base_offset,
        completion_batch,
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
        local_cpu,
        buffer_base,
        buffer_stride,
        buffer_map_len,
        buffer_alignment,
        memory_policy,
    })
}

#[allow(clippy::too_many_arguments)]
fn uring_write_bench(
    target_arg: &str,
    workers: usize,
    bytes_per_worker: usize,
    chunk_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    buffer_mode: SlotWalBufferMode,
    write_mode: UringWriteMode,
    pin_workers: bool,
) -> io::Result<()> {
    if workers == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workers must be non-zero",
        ));
    }
    validate_slot_wal_pipeline(pipeline, ring_entries)?;
    ensure_sector_aligned(bytes_per_worker, "bytes-per-worker")?;
    ensure_sector_aligned(chunk_bytes, "chunk-bytes")?;
    if bytes_per_worker % chunk_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bytes-per-worker must be an exact multiple of chunk-bytes",
        ));
    }
    let total_bytes = bytes_per_worker.checked_mul(workers).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "aggregate byte count overflow")
    })?;
    let (target, device_bytes, required_alignment, segment_bytes) = validate_slot_wal_common(
        target_arg,
        total_bytes,
        chunk_bytes,
        SlotWalMode::Write,
        buffer_mode,
    )?;
    let regions = make_linear_wal_regions(workers, bytes_per_worker)?;
    validate_wal_regions(&regions, device_bytes)?;

    println!(
        "uring-write-bench: target={} workers={workers} bytes_per_worker={bytes_per_worker} \
         total_bytes={total_bytes} chunk_bytes={chunk_bytes} pipeline_per_worker={pipeline} \
         total_pipeline={} ring_entries={ring_entries} buffers={} segment_bytes={segment_bytes} \
         required_alignment={required_alignment} write_mode={} pin_workers={pin_workers}",
        target.label(),
        workers * pipeline,
        buffer_mode.as_str(),
        write_mode.as_str()
    );

    let started = Instant::now();
    let mut handles = Vec::with_capacity(workers);
    for region in regions {
        let target = target.clone();
        handles.push(thread::spawn(move || {
            uring_write_bench_worker(
                target,
                region.worker,
                region.base_offset,
                region.len_bytes,
                chunk_bytes,
                pipeline,
                ring_entries,
                buffer_mode,
                write_mode,
                pin_workers,
            )
        }));
    }

    let mut results = Vec::with_capacity(workers);
    for handle in handles {
        results.push(
            handle
                .join()
                .map_err(|_| io::Error::other("uring write worker thread panicked"))??,
        );
    }
    results.sort_by_key(|result| result.worker);

    let wall_seconds = started.elapsed().as_secs_f64().max(f64::MIN_POSITIVE);
    let total_ops: usize = results.iter().map(|result| result.ops).sum();
    let total_bytes: usize = results.iter().map(|result| result.bytes).sum();
    for result in &results {
        let seconds = result.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        println!(
            "uring-write-worker: worker={} bytes={} ops={} seconds={seconds:.6} \
             ops_per_sec={:.0} base_offset={} completion_batch={} target_cpu={} \
             affinity_applied={} local_cpu={} buffer_base=0x{:x} buffer_alignment={} \
             buffer_stride={} buffer_map_len={} memory_policy={}",
            result.worker,
            result.bytes,
            result.ops,
            result.ops as f64 / seconds,
            result.base_offset,
            result.completion_batch,
            result.target_cpu,
            result.affinity_applied,
            result.local_cpu,
            result.buffer_base,
            result.buffer_alignment,
            result.buffer_stride,
            result.buffer_map_len,
            result.memory_policy
        );
    }
    println!(
        "uring-write-bench-summary: target={} workers={workers} write_mode={} \
         tokens={total_ops} writes={total_ops} bytes={total_bytes} wall_seconds={wall_seconds:.6} \
         ops_per_sec={:.0} MiBps={:.2} Gbitps={:.3}",
        target.label(),
        write_mode.as_str(),
        total_ops as f64 / wall_seconds,
        total_bytes as f64 / (1024.0 * 1024.0) / wall_seconds,
        total_bytes as f64 * 8.0 / 1_000_000_000.0 / wall_seconds
    );
    Ok(())
}

fn slot_wal_bench_worker(
    target: SlotWalTarget,
    worker: usize,
    base_offset: u64,
    total_bytes: usize,
    chunk_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    mode: SlotWalMode,
    buffer_mode: SlotWalBufferMode,
    pin: bool,
) -> io::Result<SlotWalWorkerResult> {
    let affinity = pin_current_thread_if_requested("slot-wal-worker", worker, pin);
    let chunk_bytes_u32 = u32::try_from(chunk_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk-bytes must fit in u32 for the SQE len field",
        )
    })?;

    let file = OpenOptions::new()
        .read(true)
        .write(matches!(mode, SlotWalMode::Write))
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(target.open_path())?;
    let fd = file.as_raw_fd();

    let mut ring = RawRing::new(ring_entries, ring_entries.saturating_mul(2))?;
    let mut fds = [fd];
    ring.register_files(&mut fds)?;

    // Allocate and first-touch after optional CPU pinning. The mmap base is page
    // aligned, which is also cacheline aligned for the 4K path.
    let preferred_numa_node = if affinity.target_cpu >= 0 {
        cpu_numa_node(affinity.target_cpu as usize)
    } else {
        None
    };
    let segment_bytes = buffer_mode.segment_bytes()?;
    let buffers = buffer_mode.allocate_for_worker(pipeline, chunk_bytes, preferred_numa_node)?;
    if matches!(mode, SlotWalMode::Write) {
        fill_slot_wal_buffers(&buffers, chunk_bytes);
    }
    let buffer_base = buffers.base_addr();
    let buffer_stride = buffers.stride();
    let buffer_map_len = buffers.map_len();
    let buffer_alignment = address_alignment(buffer_base);
    let local_cpu = current_cpu();
    let worker_numa_node = preferred_numa_node.or_else(|| {
        (local_cpu >= 0)
            .then(|| cpu_numa_node(local_cpu as usize))
            .flatten()
    });
    let memory_policy = buffers.memory_policy();
    let mut iovecs = buffers.iovecs(chunk_bytes);
    ring.register_buffers(&mut iovecs)?;

    let mut slot_ids = Vec::with_capacity(pipeline);
    for buf_index in 0..pipeline {
        slot_ids.push(ring.register_io_slot(buf_index as u32, 0)?);
    }

    let completion_batch = env_usize_or("URING_PLAY_SLOT_COMPLETION_BATCH", 1).clamp(1, pipeline);
    let total_ops = total_bytes / chunk_bytes;
    let mut free_slots: Vec<usize> = (0..pipeline).rev().collect();
    let mut submitted = 0usize;
    let mut completed = 0usize;
    let started = Instant::now();

    while completed < total_ops {
        while submitted < total_ops && !free_slots.is_empty() {
            let slot = free_slots.pop().expect("free slot");
            let file_offset = (submitted as u64)
                .checked_mul(chunk_bytes as u64)
                .and_then(|offset| base_offset.checked_add(offset))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?;
            ring.queue_slot_rw(
                slot_ids[slot],
                0,
                file_offset,
                chunk_bytes_u32,
                mode.direction(),
                slot as u64,
            )?;
            submitted += 1;
        }

        ring.submit_pending()?;
        for batch_idx in 0..completion_batch {
            let cqe = if batch_idx == 0 {
                match ring.try_pop_cqe() {
                    Some(cqe) => cqe,
                    None => ring.wait_cqe()?,
                }
            } else {
                match ring.try_pop_cqe() {
                    Some(cqe) => cqe,
                    None => break,
                }
            };
            let slot = cqe.user_data as usize;
            if slot >= pipeline {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("slot WAL CQE returned invalid slot user_data={slot}"),
                ));
            }
            if cqe.res < 0 {
                return Err(io::Error::from_raw_os_error(-cqe.res));
            }
            if cqe.res != chunk_bytes_u32 as i32 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!(
                        "short slot WAL completion: res={} expected={chunk_bytes}",
                        cqe.res
                    ),
                ));
            }
            completed += 1;
            free_slots.push(slot);
            if completed == total_ops {
                break;
            }
        }
    }

    let elapsed = started.elapsed();

    for slot_id in slot_ids {
        ring.unregister_io_slot(slot_id)?;
    }
    ring.unregister_buffers()?;
    ring.unregister_files()?;
    Ok(SlotWalWorkerResult {
        worker,
        bytes: total_bytes,
        ops: total_ops,
        elapsed,
        base_offset,
        region_bytes: total_bytes,
        slot_count: pipeline,
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
        local_cpu,
        worker_numa_node,
        completion_batch,
        buffer_base,
        buffer_stride,
        buffer_map_len,
        buffer_alignment,
        buffer_mode,
        segment_bytes,
        memory_policy,
    })
}

fn slot_wal_bench(
    target_arg: &str,
    total_bytes: usize,
    chunk_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    mode: SlotWalMode,
    buffer_mode: SlotWalBufferMode,
) -> io::Result<()> {
    validate_slot_wal_pipeline(pipeline, ring_entries)?;
    let (target, _device_bytes, required_alignment, segment_bytes) =
        validate_slot_wal_common(target_arg, total_bytes, chunk_bytes, mode, buffer_mode)?;

    let result = slot_wal_bench_worker(
        target.clone(),
        0,
        0,
        total_bytes,
        chunk_bytes,
        pipeline,
        ring_entries,
        mode,
        buffer_mode,
        false,
    )?;
    let seconds = result.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
    let mib = total_bytes as f64 / (1024.0 * 1024.0);
    println!(
        "slot-wal-bench: target={} mode={} total_bytes={total_bytes} \
         chunk_bytes={chunk_bytes} ops={} pipeline={pipeline} \
         ring_entries={ring_entries} buffers={} segment_bytes={segment_bytes} \
         required_alignment={required_alignment} slot_backend={} \
         completion_batch={} \
         buffer_base=0x{:x} buffer_alignment={} buffer_stride={} \
         buffer_map_len={} memory_policy={} seconds={seconds:.6} \
         MiBps={:.2} ops_per_sec={:.0}",
        target.label(),
        mode.as_str(),
        result.ops,
        buffer_mode.as_str(),
        io_slots::submission_backend_label(),
        result.completion_batch,
        result.buffer_base,
        result.buffer_alignment,
        result.buffer_stride,
        result.buffer_map_len,
        result.memory_policy,
        mib / seconds,
        result.ops as f64 / seconds
    );
    Ok(())
}

fn slot_rw_same_slot_test(
    target_arg: &str,
    ops: usize,
    chunk_bytes: usize,
    inflight: usize,
    ring_entries: u32,
    buffer_mode: SlotWalBufferMode,
) -> io::Result<()> {
    if ops == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ops must be non-zero",
        ));
    }
    if inflight < 2 || inflight > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("inflight must be between 2 and {}", u16::MAX),
        ));
    }
    if inflight > ring_entries as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inflight must be <= ring-entries",
        ));
    }
    let chunk_bytes_u32 = u32::try_from(chunk_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk-bytes must fit in u32 for the SQE len field",
        )
    })?;
    let total_bytes = ops.checked_mul(chunk_bytes).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "aggregate byte count overflow")
    })?;
    let (target, _device_bytes, required_alignment, segment_bytes) = validate_slot_wal_common(
        target_arg,
        total_bytes,
        chunk_bytes,
        SlotWalMode::Write,
        buffer_mode,
    )?;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(target.open_path())?;
    let fd = file.as_raw_fd();

    let mut ring = RawRing::new(ring_entries, ring_entries.saturating_mul(2))?;
    let mut fds = [fd];
    ring.register_files(&mut fds)?;

    let buffers = buffer_mode.allocate(1, chunk_bytes)?;
    fill_slot_wal_buffers(&buffers, chunk_bytes);
    let buffer_base = buffers.base_addr();
    let buffer_stride = buffers.stride();
    let buffer_map_len = buffers.map_len();
    let buffer_alignment = address_alignment(buffer_base);
    let memory_policy = buffers.memory_policy();
    let mut iovecs = buffers.iovecs(chunk_bytes);
    ring.register_buffers(&mut iovecs)?;
    let slot_id = ring.register_io_slot(0, 0)?;

    let started = Instant::now();
    let mut submitted = 0usize;
    let mut completed = 0usize;
    let mut max_observed_inflight = 0usize;
    let mut first_error = None;

    while completed < ops || completed < submitted {
        while first_error.is_none()
            && submitted < ops
            && submitted.saturating_sub(completed) < inflight
        {
            let file_offset = (submitted as u64)
                .checked_mul(chunk_bytes as u64)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset overflow"))?;
            ring.queue_slot_rw(
                slot_id,
                0,
                file_offset,
                chunk_bytes_u32,
                io_slots::SlotRw::Write,
                submitted as u64,
            )?;
            submitted += 1;
            max_observed_inflight = max_observed_inflight.max(submitted - completed);
        }

        if completed == submitted {
            break;
        }

        let cqe = ring.wait_cqe()?;
        let seq = cqe.user_data as usize;
        if seq >= ops && first_error.is_none() {
            first_error = Some(format!("invalid same-slot CQE user_data={seq} ops={ops}"));
        }
        if cqe.res < 0 && first_error.is_none() {
            first_error = Some(format!(
                "same-slot write seq={seq} failed with errno {}",
                -cqe.res
            ));
        } else if cqe.res != chunk_bytes_u32 as i32 && first_error.is_none() {
            first_error = Some(format!(
                "short same-slot write completion: seq={seq} res={} expected={chunk_bytes}",
                cqe.res
            ));
        }
        completed += 1;
    }

    let elapsed = started.elapsed();
    let seconds = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);

    ring.unregister_io_slot(slot_id)?;
    ring.unregister_buffers()?;
    ring.unregister_files()?;

    if let Some(error) = first_error {
        return Err(io::Error::new(io::ErrorKind::WriteZero, error));
    }

    println!(
        "slot-rw-same-slot-test: ok target={} ops={ops} total_bytes={total_bytes} \
         chunk_bytes={chunk_bytes} inflight={inflight} max_observed_inflight={max_observed_inflight} \
         ring_entries={ring_entries} buffers={} segment_bytes={segment_bytes} \
         required_alignment={required_alignment} slot_backend={} \
         buffer_base=0x{buffer_base:x} buffer_alignment={buffer_alignment} \
         buffer_stride={buffer_stride} buffer_map_len={buffer_map_len} \
         memory_policy={memory_policy} seconds={seconds:.6} ops_per_sec={:.0} MiBps={:.2}",
        target.label(),
        buffer_mode.as_str(),
        io_slots::submission_backend_label(),
        ops as f64 / seconds,
        total_bytes as f64 / (1024.0 * 1024.0) / seconds
    );
    Ok(())
}

fn slot_wal_sharded_bench(
    target_arg: &str,
    workers: usize,
    bytes_per_worker: usize,
    chunk_bytes: usize,
    pipeline: usize,
    ring_entries: u32,
    mode: SlotWalMode,
    buffer_mode: SlotWalBufferMode,
    pin_workers: bool,
) -> io::Result<()> {
    if workers == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workers must be non-zero",
        ));
    }
    validate_slot_wal_pipeline(pipeline, ring_entries)?;
    ensure_sector_aligned(bytes_per_worker, "bytes-per-worker")?;
    ensure_sector_aligned(chunk_bytes, "chunk-bytes")?;
    if bytes_per_worker % chunk_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bytes-per-worker must be an exact multiple of chunk-bytes",
        ));
    }
    let total_bytes = bytes_per_worker.checked_mul(workers).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "aggregate byte count overflow")
    })?;
    let (target, device_bytes, required_alignment, segment_bytes) =
        validate_slot_wal_common(target_arg, total_bytes, chunk_bytes, mode, buffer_mode)?;
    if total_bytes as u64 > device_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("aggregate total-bytes exceeds block device size {device_bytes}"),
        ));
    }
    let regions = make_linear_wal_regions(workers, bytes_per_worker)?;
    validate_wal_regions(&regions, device_bytes)?;
    for region in &regions {
        println!(
            "slot-wal-sharded-region: worker={} base_offset={} len_bytes={} end_offset={}",
            region.worker,
            region.base_offset,
            region.len_bytes,
            region.end_offset()?
        );
    }

    let started = Instant::now();
    let mut handles = Vec::with_capacity(workers);
    for region in regions {
        let target = target.clone();
        handles.push(thread::spawn(move || {
            slot_wal_bench_worker(
                target,
                region.worker,
                region.base_offset,
                region.len_bytes,
                chunk_bytes,
                pipeline,
                ring_entries,
                mode,
                buffer_mode,
                pin_workers,
            )
        }));
    }

    let mut results = Vec::with_capacity(workers);
    for handle in handles {
        results.push(
            handle
                .join()
                .map_err(|_| io::Error::other("slot WAL worker thread panicked"))??,
        );
    }
    results.sort_by_key(|result| result.worker);

    let wall = started.elapsed();
    let wall_seconds = wall.as_secs_f64().max(f64::MIN_POSITIVE);
    let total_ops: usize = results.iter().map(|result| result.ops).sum();
    let total_bytes: usize = results.iter().map(|result| result.bytes).sum();
    let slowest = results
        .iter()
        .map(|result| result.elapsed)
        .max()
        .unwrap_or_default()
        .as_secs_f64()
        .max(f64::MIN_POSITIVE);
    for result in &results {
        println!(
            "slot-wal-sharded-worker: worker={} bytes={} ops={} seconds={:.6} \
             ops_per_sec={:.0} base_offset={} region_bytes={} slots={} \
             completion_batch={} target_cpu={} affinity_applied={} local_cpu={} worker_numa_node={} \
             buffer_mode={} segment_bytes={} buffer_base=0x{:x} \
             buffer_alignment={} buffer_stride={} buffer_map_len={} memory_policy={}",
            result.worker,
            result.bytes,
            result.ops,
            result.elapsed.as_secs_f64(),
            result.ops as f64 / result.elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
            result.base_offset,
            result.region_bytes,
            result.slot_count,
            result.completion_batch,
            result.target_cpu,
            result.affinity_applied,
            result.local_cpu,
            option_i32_label(result.worker_numa_node),
            result.buffer_mode.as_str(),
            result.segment_bytes,
            result.buffer_base,
            result.buffer_alignment,
            result.buffer_stride,
            result.buffer_map_len,
            result.memory_policy
        );
    }
    println!(
        "slot-wal-sharded-bench: target={} mode={} workers={workers} \
         bytes_per_worker={bytes_per_worker} total_bytes={total_bytes} \
         chunk_bytes={chunk_bytes} ops={total_ops} pipeline_per_worker={pipeline} \
         total_pipeline={} completion_batch={} ring_entries={ring_entries} buffers={} \
         segment_bytes={segment_bytes} required_alignment={required_alignment} \
         slot_backend={} \
         pin_workers={pin_workers} wall_seconds={wall_seconds:.6} \
         slowest_worker_seconds={slowest:.6} MiBps={:.2} ops_per_sec={:.0}",
        target.label(),
        mode.as_str(),
        pipeline * workers,
        results
            .first()
            .map(|result| result.completion_batch)
            .unwrap_or(1),
        buffer_mode.as_str(),
        io_slots::submission_backend_label(),
        total_bytes as f64 / (1024.0 * 1024.0) / wall_seconds,
        total_ops as f64 / wall_seconds
    );
    Ok(())
}

fn slot_topology_plan(
    target_arg: &str,
    workers: usize,
    bytes_per_worker: usize,
    chunk_bytes: usize,
    pipeline: usize,
    buffer_mode: SlotWalBufferMode,
) -> io::Result<()> {
    if workers == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workers must be non-zero",
        ));
    }
    if pipeline == 0 || pipeline > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("pipeline must be between 1 and {}", u16::MAX),
        ));
    }
    ensure_sector_aligned(bytes_per_worker, "bytes-per-worker")?;
    ensure_sector_aligned(chunk_bytes, "chunk-bytes")?;
    if bytes_per_worker % chunk_bytes != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bytes-per-worker must be an exact multiple of chunk-bytes",
        ));
    }
    let total_bytes = bytes_per_worker.checked_mul(workers).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "aggregate byte count overflow")
    })?;
    let (target, metadata, device_bytes, required_alignment, segment_bytes) =
        validate_slot_wal_plan_common(target_arg, total_bytes, chunk_bytes, buffer_mode)?;
    let topology = BlockDeviceTopology::from_metadata(&metadata)?;
    let regions = make_linear_wal_regions(workers, bytes_per_worker)?;
    validate_wal_regions(&regions, device_bytes)?;
    let env_pin = env_truthy("URING_PLAY_PIN_CPUS");
    let numa_mismatches = regions
        .iter()
        .filter(|region| {
            let cpu = topology.planned_cpu(region.worker);
            topology.device_numa_node.is_some()
                && cpu_numa_node(cpu).is_some()
                && cpu_numa_node(cpu) != topology.device_numa_node
        })
        .count();

    println!(
        "slot-topology-plan: target={} device_bytes={} block_sysfs={} disk_sysfs={} \
         queue_count={} device_numa_node={} workers={workers} \
         bytes_per_worker={bytes_per_worker} total_bytes={total_bytes} \
         chunk_bytes={chunk_bytes} pipeline_per_worker={pipeline} total_slots={} \
         buffer_mode={} segment_bytes={segment_bytes} required_alignment={required_alignment} \
         pin_env={} numa_mismatches={}",
        target.label(),
        device_bytes,
        topology.block_dir.display(),
        topology.disk_dir.display(),
        topology.queues.len(),
        option_i32_label(topology.device_numa_node),
        pipeline * workers,
        buffer_mode.as_str(),
        env_pin,
        numa_mismatches
    );
    for queue in &topology.queues {
        println!(
            "slot-topology-queue: queue={} cpus={}",
            queue.index,
            format_cpu_list(&queue.cpus)
        );
    }
    for region in &regions {
        let cpu = topology.planned_cpu(region.worker);
        let queue = topology.queue_for_cpu(region.worker, cpu);
        let cpu_numa = cpu_numa_node(cpu);
        let end_offset = region.end_offset()?;
        let numa_local = topology.device_numa_node.is_none()
            || cpu_numa.is_none()
            || topology.device_numa_node == cpu_numa;
        println!(
            "slot-topology-worker: worker={} cpu={} cpu_numa_node={} \
             nvme_queue={} queue_cpus={} ring={} slots={} region_base={} \
             region_len={} region_end={} numa_local={}",
            region.worker,
            cpu,
            option_i32_label(cpu_numa),
            queue.index,
            format_cpu_list(&queue.cpus),
            region.worker,
            pipeline,
            region.base_offset,
            region.len_bytes,
            end_offset,
            numa_local
        );
    }

    Ok(())
}

fn parse_u8_arg(value: &str) -> Result<u8, std::num::ParseIntError> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u8::from_str_radix(hex, 16)
    } else {
        value.parse::<u8>()
    }
}

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        None | Some("probe") => probe(),
        Some("rdma-probe") => {
            let netdev = args.next();
            rdma_probe(netdev.as_deref())
        }
        Some("rdma-plan") => {
            let fabric = args
                .next()
                .map(|value| RdmaFabricPlan::parse(&value))
                .transpose()?
                .unwrap_or(RdmaFabricPlan::Auto);
            let peers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(2);
            let lanes_per_peer = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4);
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(0);
            rdma_plan(fabric, peers, lanes_per_peer, workers)
        }
        Some("rdma-rxe-add") => {
            let netdev = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: rdma-rxe-add <netdev> [rxe-name]",
                )
            })?;
            let name = args.next();
            rdma_rxe_add(&netdev, name.as_deref())
        }
        Some("rdma-rxe-del") => {
            let name = args.next().unwrap_or_else(|| "rxe0".to_string());
            rdma_rxe_del(&name)
        }
        Some("rdma-rxe-smoke") | Some("rdma-smoke") => {
            let netdev = args.next().unwrap_or_else(|| "lo".to_string());
            let device = args.next();
            let gid_idx = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let iters = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1000);
            let size = args
                .next()
                .map(|value| parse_size_arg(&value, "size"))
                .transpose()?
                .unwrap_or(4096);
            let port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(18651);
            rdma_rxe_smoke(&netdev, device.as_deref(), gid_idx, iters, size, port)
        }
        Some("libfabric-plan") | Some("ofi-plan") => {
            let provider = args.next().unwrap_or_else(|| "tcp".to_string());
            let endpoint = args.next().unwrap_or_else(|| "rdm".to_string());
            let peers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(2);
            let lanes_per_peer = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(32);
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(0);
            libfabric_plan(&provider, &endpoint, peers, lanes_per_peer, workers)
        }
        Some("libfabric-smoke") | Some("ofi-smoke") => {
            let provider = args.next().unwrap_or_else(|| "tcp".to_string());
            let endpoint = args.next().unwrap_or_else(|| "rdm".to_string());
            let addr = args.next().unwrap_or_else(|| "127.0.0.1".to_string());
            let iters = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1000);
            let size = args
                .next()
                .map(|value| parse_size_arg(&value, "size"))
                .transpose()?
                .unwrap_or(4096);
            let port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(18691);
            let domain = args.next();
            libfabric_smoke(
                &provider,
                &endpoint,
                &addr,
                iters,
                size,
                port,
                domain.as_deref(),
            )
        }
        Some("path-plan") | Some("transport-plan") => {
            let transport = args
                .next()
                .map(|value| TransportPathPlan::parse(&value))
                .transpose()?
                .unwrap_or(TransportPathPlan::AwsTcpPublic);
            let target_peer_gbps = args
                .next()
                .map(|value| parse_gbps_arg(&value, "target-peer-gbps"))
                .transpose()?
                .unwrap_or(40.0);
            let peers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(2);
            let min_lanes_per_peer = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(0);
            transport_path_plan(
                transport,
                target_peer_gbps,
                peers,
                min_lanes_per_peer,
                workers,
            )
        }
        Some("register-ifq") => {
            let ifname = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: register-ifq <ifname> [rxq]",
                )
            })?;
            let rxq = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(0);
            register_ifq(&ifname, rxq)
        }
        Some("stress-register-ifq") => {
            let ifname = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: stress-register-ifq <ifname> [iterations] [rxq]",
                )
            })?;
            let iterations = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(100);
            let rxq = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(0);
            stress_register_ifq(&ifname, iterations, rxq)
        }
        Some("recv-zc-server") => {
            let ifname = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: recv-zc-server <ifname> [rxq] [port] [expected-bytes] [fixed-byte]",
                )
            })?;
            let rxq = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(0);
            let port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(8000);
            let expected_bytes = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(64 * 1024);
            let fixed_byte = args
                .next()
                .map(|value| parse_u8_arg(&value))
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
            recv_zc_server(&ifname, rxq, port, expected_bytes, fixed_byte)
        }
        Some("tcp-send") => {
            let addr = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: tcp-send <addr> [port] [bytes] [fixed-byte]",
                )
            })?;
            let port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(8000);
            let bytes = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(64 * 1024);
            let fixed_byte = args
                .next()
                .map(|value| parse_u8_arg(&value))
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
            tcp_send(&addr, port, bytes, fixed_byte)
        }
        Some("tcp-sink-server") => {
            let bind = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: tcp-sink-server <bind> [port] [connections] [expected-bytes]",
                )
            })?;
            let port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(8000);
            let connections = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let expected_bytes = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(64 * 1024);
            tcp_sink_server(&bind, port, connections, expected_bytes)
        }
        Some("tcp-bench-server") => {
            let bind = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: tcp-bench-server <bind> [port] [connections] [expected-bytes]",
                )
            })?;
            let port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(8000);
            let connections = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let expected_bytes = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024 * 1024 * 1024);
            tcp_bench_server(&bind, port, connections, expected_bytes)
        }
        Some("tcp-bench-send") => {
            let addr = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: tcp-bench-send <addr> [port] [connections] [bytes-per-connection] [chunk-bytes]",
                )
            })?;
            let port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(8000);
            let connections = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let bytes_per_connection = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024 * 1024 * 1024);
            let chunk_bytes = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024 * 1024);
            tcp_bench_send(&addr, port, connections, bytes_per_connection, chunk_bytes)
        }
        Some("tcp-bench-mux-server") => {
            let bind = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: tcp-bench-mux-server <bind> [base-port] [ports] [connections-per-port] [expected-bytes]",
                )
            })?;
            let base_port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(8000);
            let ports = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(16);
            let connections_per_port = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let expected_bytes = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024 * 1024 * 1024);
            tcp_bench_mux_server(
                &bind,
                base_port,
                ports,
                connections_per_port,
                expected_bytes,
            )
        }
        Some("tcp-bench-mux-send") => {
            let addr = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: tcp-bench-mux-send <addr> [base-port] [ports] [connections-per-port] [bytes-per-connection] [chunk-bytes]",
                )
            })?;
            let base_port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(8000);
            let ports = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(16);
            let connections_per_port = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let bytes_per_connection = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024 * 1024 * 1024);
            let chunk_bytes = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024 * 1024);
            tcp_bench_mux_send(
                &addr,
                base_port,
                ports,
                connections_per_port,
                bytes_per_connection,
                chunk_bytes,
            )
        }
        Some("tcp-bench-uring-mux-server") => {
            let bind = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: tcp-bench-uring-mux-server <bind> [base-port] [ports] [connections-per-port] [expected-bytes] [workers] [recv-bytes] [ring-entries] [recv-mode] [ifname] [rxq] [rxq-count]",
                )
            })?;
            let base_port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(8000);
            let ports = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(16);
            let connections_per_port = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let expected_bytes = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024 * 1024 * 1024);
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(0);
            let recv_bytes = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024 * 1024);
            let ring_entries = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4096);
            let recv_mode = args.next().unwrap_or_else(|| "recv".to_string());
            match recv_mode.as_str() {
                "recv" | "normal" => tcp_bench_uring_mux_server(
                    &bind,
                    base_port,
                    ports,
                    connections_per_port,
                    expected_bytes,
                    workers,
                    recv_bytes,
                    ring_entries,
                ),
                "zcrx" | "recv-zc" => {
                    let ifname = args.next().unwrap_or_else(|| "raft0".to_string());
                    let rxq = args
                        .next()
                        .map(|value| value.parse::<u32>())
                        .transpose()
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                        .unwrap_or(0);
                    let rxq_count = args
                        .next()
                        .map(|value| value.parse::<usize>())
                        .transpose()
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                        .unwrap_or(1);
                    tcp_bench_uring_zcrx_mux_server(
                        &ifname,
                        rxq,
                        rxq_count,
                        &bind,
                        base_port,
                        ports,
                        connections_per_port,
                        expected_bytes,
                        recv_bytes,
                        ring_entries,
                    )
                }
                other => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown recv mode {other:?}; use recv or zcrx"),
                )),
            }
        }
        Some("tcp-wal-mux-server") => {
            let target = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: tcp-wal-mux-server <PARTUUID=uuid|/dev/nullbN> <bind> [base-port] [ports] [connections-per-port] [bytes-per-connection] [chunk-bytes] [pipeline] [workers] [ring-entries] [buffer-mode] [pin-workers]",
                )
            })?;
            let bind = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: tcp-wal-mux-server <PARTUUID=uuid|/dev/nullbN> <bind> [base-port] [ports] [connections-per-port] [bytes-per-connection] [chunk-bytes] [pipeline] [workers] [ring-entries] [buffer-mode] [pin-workers]",
                )
            })?;
            let base_port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(9200);
            let ports = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4);
            let connections_per_port = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let bytes_per_connection = args
                .next()
                .map(|value| parse_size_arg(&value, "bytes-per-connection"))
                .transpose()?
                .unwrap_or(64 * 1024 * 1024);
            let chunk_bytes = args
                .next()
                .map(|value| parse_size_arg(&value, "chunk-bytes"))
                .transpose()?
                .unwrap_or(4096);
            let pipeline = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(32);
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(0);
            let ring_entries = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024);
            let estimated_workers = if workers == 0 { ports.max(1) } else { workers };
            let buffer_mode = parse_slot_wal_buffer_mode_or_standard(
                args.next(),
                "tcp-wal-mux-server",
                checked_buffer_count(estimated_workers, pipeline, "tcp-wal-mux-server")?,
                chunk_bytes,
            )?;
            let pin_workers = args
                .next()
                .map(|value| matches!(value.as_str(), "1" | "yes" | "true" | "pin"))
                .unwrap_or(true);
            tcp_wal_mux_server(
                &target,
                &bind,
                base_port,
                ports,
                connections_per_port,
                bytes_per_connection,
                chunk_bytes,
                pipeline,
                workers,
                ring_entries,
                buffer_mode,
                pin_workers,
            )
        }
        Some("udp-wal-mux-server") => {
            let target = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: udp-wal-mux-server <PARTUUID=uuid|/dev/nullbN> <bind> [base-port] [ports] [flows-per-port] [bytes-per-flow] [chunk-bytes] [pipeline] [workers] [ring-entries] [buffer-mode] [pin-workers]",
                )
            })?;
            let bind = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: udp-wal-mux-server <PARTUUID=uuid|/dev/nullbN> <bind> [base-port] [ports] [flows-per-port] [bytes-per-flow] [chunk-bytes] [pipeline] [workers] [ring-entries] [buffer-mode] [pin-workers]",
                )
            })?;
            let base_port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(9400);
            let ports = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4);
            let flows_per_port = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let bytes_per_flow = args
                .next()
                .map(|value| parse_size_arg(&value, "bytes-per-flow"))
                .transpose()?
                .unwrap_or(64 * 1024 * 1024);
            let chunk_bytes = args
                .next()
                .map(|value| parse_size_arg(&value, "chunk-bytes"))
                .transpose()?
                .unwrap_or(256 * 1024);
            let pipeline = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(32);
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(0);
            let ring_entries = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024);
            let estimated_workers = if workers == 0 { ports.max(1) } else { workers };
            let buffer_mode = parse_slot_wal_buffer_mode_or_standard(
                args.next(),
                "udp-wal-mux-server",
                checked_buffer_count(estimated_workers, pipeline, "udp-wal-mux-server")?,
                chunk_bytes,
            )?;
            let pin_workers = args
                .next()
                .map(|value| matches!(value.as_str(), "1" | "yes" | "true" | "pin"))
                .unwrap_or(true);
            udp_wal_mux_server(
                &target,
                &bind,
                base_port,
                ports,
                flows_per_port,
                bytes_per_flow,
                chunk_bytes,
                pipeline,
                workers,
                ring_entries,
                buffer_mode,
                pin_workers,
            )
        }
        Some("tcp-bench-uring-mux-send") => {
            let addr = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: tcp-bench-uring-mux-send <addr> [base-port] [ports] [connections-per-port] [bytes-per-connection] [chunk-bytes] [pipeline] [workers] [ring-entries] [send-mode]",
                )
            })?;
            let base_port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(8000);
            let ports = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(16);
            let connections_per_port = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let bytes_per_connection = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024 * 1024 * 1024);
            let chunk_bytes = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024 * 1024);
            let pipeline = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4);
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(0);
            let ring_entries = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4096);
            let send_mode = args
                .next()
                .map(|value| UringSendMode::parse(&value))
                .transpose()?
                .unwrap_or(UringSendMode::Send);
            tcp_bench_uring_mux_send(
                &addr,
                base_port,
                ports,
                connections_per_port,
                bytes_per_connection,
                chunk_bytes,
                pipeline,
                workers,
                ring_entries,
                send_mode,
            )
        }
        Some("udp-bench-mux-send") => {
            let addr = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: udp-bench-mux-send <addr> [base-port] [ports] [flows-per-port] [bytes-per-flow] [datagram-bytes] [workers]",
                )
            })?;
            let base_port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(9400);
            let ports = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4);
            let flows_per_port = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let bytes_per_flow = args
                .next()
                .map(|value| parse_size_arg(&value, "bytes-per-flow"))
                .transpose()?
                .unwrap_or(64 * 1024 * 1024);
            let datagram_bytes = args
                .next()
                .map(|value| parse_size_arg(&value, "datagram-bytes"))
                .transpose()?
                .unwrap_or(32 * 1024);
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(0);
            udp_bench_mux_send(
                &addr,
                base_port,
                ports,
                flows_per_port,
                bytes_per_flow,
                datagram_bytes,
                workers,
            )
        }
        Some("cache-copy-microbench") => {
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4);
            let bytes_per_worker = args
                .next()
                .map(|value| parse_size_arg(&value, "bytes-per-worker"))
                .transpose()?
                .unwrap_or(256 * 1024 * 1024);
            let chunk_bytes = args
                .next()
                .map(|value| parse_size_arg(&value, "chunk-bytes"))
                .transpose()?
                .unwrap_or(8192);
            let iterations = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let mode = args
                .next()
                .map(|value| CacheCopyMicroMode::parse(&value))
                .transpose()?
                .unwrap_or(CacheCopyMicroMode::Copy);
            let chunks_per_worker = if chunk_bytes == 0 {
                1
            } else {
                bytes_per_worker.div_ceil(chunk_bytes).max(1)
            };
            let buffers_per_worker =
                checked_buffer_count(chunks_per_worker, 2, "cache-copy-microbench")?;
            let buffer_mode = parse_slot_wal_buffer_mode_or_standard(
                args.next(),
                "cache-copy-microbench",
                checked_buffer_count(workers, buffers_per_worker, "cache-copy-microbench")?,
                chunk_bytes,
            )?;
            let pin = args
                .next()
                .map(|value| parse_bool_arg(&value, "pin"))
                .transpose()?
                .unwrap_or(true);
            cache_copy_microbench(
                workers,
                bytes_per_worker,
                chunk_bytes,
                iterations,
                mode,
                buffer_mode,
                pin,
            )
        }
        Some("cache-smt-microbench") => {
            let bytes_per_worker = args
                .next()
                .map(|value| parse_size_arg(&value, "bytes-per-worker"))
                .transpose()?
                .unwrap_or(256 * 1024 * 1024);
            let chunk_bytes = args
                .next()
                .map(|value| parse_size_arg(&value, "chunk-bytes"))
                .transpose()?
                .unwrap_or(8192);
            let iterations = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1);
            let mode = args
                .next()
                .map(|value| CacheCopyMicroMode::parse(&value))
                .transpose()?
                .unwrap_or(CacheCopyMicroMode::Copy);
            let chunks_per_worker = if chunk_bytes == 0 {
                1
            } else {
                bytes_per_worker.div_ceil(chunk_bytes).max(1)
            };
            let buffers_per_worker =
                checked_buffer_count(chunks_per_worker, 2, "cache-smt-microbench")?;
            let buffer_mode = parse_slot_wal_buffer_mode_or_standard(
                args.next(),
                "cache-smt-microbench",
                checked_buffer_count(2, buffers_per_worker, "cache-smt-microbench")?,
                chunk_bytes,
            )?;
            cache_smt_microbench(bytes_per_worker, chunk_bytes, iterations, mode, buffer_mode)
        }
        Some("uring-baton-bench") => {
            let target = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: uring-baton-bench <null|PARTUUID=uuid|/dev/nullbN> [workers] \
                     [bytes-per-worker] [chunk-bytes] [pipeline-per-worker] [ring-entries] \
                     [buffer-mode] [pin-mode:true|false|hctx] [baton-mode:roundtrip|credit:N]",
                )
            })?;
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4);
            let bytes_per_worker = args
                .next()
                .map(|value| parse_size_arg(&value, "bytes-per-worker"))
                .transpose()?
                .unwrap_or(1024 * 1024 * 1024);
            let chunk_bytes = args
                .next()
                .map(|value| parse_size_arg(&value, "chunk-bytes"))
                .transpose()?
                .unwrap_or(4096);
            let pipeline = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(256);
            let ring_entries = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024);
            let buffer_mode = parse_slot_wal_buffer_mode_or_standard(
                args.next(),
                "uring-baton-bench",
                checked_buffer_count(workers, pipeline, "uring-baton-bench")?,
                chunk_bytes,
            )?;
            let pin_mode = args
                .next()
                .map(|value| BatonPinMode::parse(&value))
                .transpose()?
                .unwrap_or(BatonPinMode::Pair);
            let baton_mode = args
                .next()
                .map(|value| BatonMode::parse(&value))
                .transpose()?
                .unwrap_or(BatonMode::RoundTrip);
            uring_baton_bench(
                &target,
                workers,
                bytes_per_worker,
                chunk_bytes,
                pipeline,
                ring_entries,
                buffer_mode,
                pin_mode,
                baton_mode,
            )
        }
        Some("uring-write-bench") => {
            let target = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: uring-write-bench <PARTUUID=uuid|/dev/nullbN> [workers] \
                     [bytes-per-worker] [chunk-bytes] [pipeline-per-worker] [ring-entries] \
                     [buffer-mode] [write-mode:write|fixed|fixed-file] [pin-workers]",
                )
            })?;
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(16);
            let bytes_per_worker = args
                .next()
                .map(|value| parse_size_arg(&value, "bytes-per-worker"))
                .transpose()?
                .unwrap_or(512 * 1024 * 1024);
            let chunk_bytes = args
                .next()
                .map(|value| parse_size_arg(&value, "chunk-bytes"))
                .transpose()?
                .unwrap_or(4096);
            let pipeline = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(256);
            let ring_entries = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(512);
            let buffer_mode = parse_slot_wal_buffer_mode_or_standard(
                args.next(),
                "uring-write-bench",
                checked_buffer_count(workers, pipeline, "uring-write-bench")?,
                chunk_bytes,
            )?;
            let write_mode = args
                .next()
                .map(|value| UringWriteMode::parse(&value))
                .transpose()?
                .unwrap_or(UringWriteMode::WriteFixedFile);
            let pin_workers = args
                .next()
                .map(|value| matches!(value.as_str(), "1" | "yes" | "true" | "pin"))
                .unwrap_or(true);
            uring_write_bench(
                &target,
                workers,
                bytes_per_worker,
                chunk_bytes,
                pipeline,
                ring_entries,
                buffer_mode,
                write_mode,
                pin_workers,
            )
        }
        Some("slot-wal-bench") => {
            let path = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: slot-wal-bench <PARTUUID=uuid|/dev/nullbN> [total-bytes] \
                     [chunk-bytes] [pipeline] [ring-entries] [mode] [buffer-mode]",
                )
            })?;
            let total_bytes = args
                .next()
                .map(|value| parse_size_arg(&value, "total-bytes"))
                .transpose()?
                .unwrap_or(1024 * 1024 * 1024);
            let chunk_bytes = args
                .next()
                .map(|value| parse_size_arg(&value, "chunk-bytes"))
                .transpose()?
                .unwrap_or(1024 * 1024);
            let pipeline = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(64);
            let ring_entries = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024);
            let mode = args
                .next()
                .map(|value| SlotWalMode::parse(&value))
                .transpose()?
                .unwrap_or(SlotWalMode::Write);
            let buffer_mode = parse_slot_wal_buffer_mode_or_standard(
                args.next(),
                "slot-wal-bench",
                pipeline,
                chunk_bytes,
            )?;
            slot_wal_bench(
                &path,
                total_bytes,
                chunk_bytes,
                pipeline,
                ring_entries,
                mode,
                buffer_mode,
            )
        }
        Some("slot-rw-same-slot-test") => {
            let path = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: slot-rw-same-slot-test <PARTUUID=uuid|/dev/nullbN> [ops] \
                     [chunk-bytes] [inflight] [ring-entries] [buffer-mode]",
                )
            })?;
            let ops = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4096);
            let chunk_bytes = args
                .next()
                .map(|value| parse_size_arg(&value, "chunk-bytes"))
                .transpose()?
                .unwrap_or(4096);
            let inflight = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(64);
            let ring_entries = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(256);
            let buffer_mode = parse_slot_wal_buffer_mode_or_standard(
                args.next(),
                "slot-rw-same-slot-test",
                1,
                chunk_bytes,
            )?;
            slot_rw_same_slot_test(&path, ops, chunk_bytes, inflight, ring_entries, buffer_mode)
        }
        Some("slot-wal-sharded-bench") => {
            let path = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: slot-wal-sharded-bench <PARTUUID=uuid|/dev/nullbN> [workers] \
                     [bytes-per-worker] [chunk-bytes] [pipeline-per-worker] [ring-entries] \
                     [mode] [buffer-mode] [pin-workers]",
                )
            })?;
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4);
            let bytes_per_worker = args
                .next()
                .map(|value| parse_size_arg(&value, "bytes-per-worker"))
                .transpose()?
                .unwrap_or(1024 * 1024 * 1024);
            let chunk_bytes = args
                .next()
                .map(|value| parse_size_arg(&value, "chunk-bytes"))
                .transpose()?
                .unwrap_or(4096);
            let pipeline = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(16);
            let ring_entries = args
                .next()
                .map(|value| value.parse::<u32>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(1024);
            let mode = args
                .next()
                .map(|value| SlotWalMode::parse(&value))
                .transpose()?
                .unwrap_or(SlotWalMode::Write);
            let buffer_mode = parse_slot_wal_buffer_mode_or_standard(
                args.next(),
                "slot-wal-sharded-bench",
                checked_buffer_count(workers, pipeline, "slot-wal-sharded-bench")?,
                chunk_bytes,
            )?;
            let pin_workers = args
                .next()
                .map(|value| matches!(value.as_str(), "1" | "yes" | "true" | "pin"))
                .unwrap_or(true);
            slot_wal_sharded_bench(
                &path,
                workers,
                bytes_per_worker,
                chunk_bytes,
                pipeline,
                ring_entries,
                mode,
                buffer_mode,
                pin_workers,
            )
        }
        Some("slot-topology-plan") => {
            let path = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: slot-topology-plan <PARTUUID=uuid|/dev/nullbN> [workers] \
                     [bytes-per-worker] [chunk-bytes] [pipeline-per-worker] [buffer-mode]",
                )
            })?;
            let workers = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4);
            let bytes_per_worker = args
                .next()
                .map(|value| parse_size_arg(&value, "bytes-per-worker"))
                .transpose()?
                .unwrap_or(1024 * 1024 * 1024);
            let chunk_bytes = args
                .next()
                .map(|value| parse_size_arg(&value, "chunk-bytes"))
                .transpose()?
                .unwrap_or(4096);
            let pipeline = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(16);
            let buffer_mode = parse_slot_wal_buffer_mode_or_standard(
                args.next(),
                "slot-topology-plan",
                checked_buffer_count(workers, pipeline, "slot-topology-plan")?,
                chunk_bytes,
            )?;
            slot_topology_plan(
                &path,
                workers,
                bytes_per_worker,
                chunk_bytes,
                pipeline,
                buffer_mode,
            )
        }
        Some("raft-follower") => {
            let bind = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: raft-follower <bind> [port] [entries] [payload-bytes] [ack-stride]",
                )
            })?;
            let port = args
                .next()
                .map(|value| value.parse::<u16>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(9100);
            let entries = args
                .next()
                .map(|value| value.parse::<u64>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4096);
            let payload_bytes = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4096);
            let ack_stride = args
                .next()
                .map(|value| value.parse::<u64>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(64);
            raft_follower(&bind, port, entries, payload_bytes, ack_stride)
        }
        Some("raft-leader") => {
            let peers = args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: raft-leader <peer1:port,peer2:port,...> [entries] \
                     [payload-bytes] [ack-stride]",
                )
            })?;
            let entries = args
                .next()
                .map(|value| value.parse::<u64>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4096);
            let payload_bytes = args
                .next()
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(4096);
            let ack_stride = args
                .next()
                .map(|value| value.parse::<u64>())
                .transpose()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?
                .unwrap_or(64);
            raft_leader(&peers, entries, payload_bytes, ack_stride)
        }
        Some(other) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unknown command {other:?}; use probe, rdma-probe, rdma-plan, path-plan, \
                 libfabric-plan, libfabric-smoke, rdma-rxe-add, rdma-rxe-del, \
                 rdma-rxe-smoke, register-ifq, stress-register-ifq, \
                 recv-zc-server, tcp-send, tcp-sink-server, tcp-bench-send, \
                 tcp-bench-server, tcp-bench-mux-send, tcp-bench-mux-server, \
                 tcp-bench-uring-mux-send, tcp-bench-uring-mux-server, \
                 tcp-wal-mux-server, udp-wal-mux-server, udp-bench-mux-send, \
                 uring-baton-bench, uring-write-bench, \
                 cache-copy-microbench, cache-smt-microbench, \
                 slot-wal-bench, slot-rw-same-slot-test, slot-wal-sharded-bench, \
                 slot-topology-plan, \
                 raft-follower, or raft-leader"
            ),
        )),
    }
}
