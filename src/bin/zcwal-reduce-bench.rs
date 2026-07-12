use std::env;
use std::ffi::c_void;
use std::fmt;
use std::fs;
use std::io;
use std::ptr;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use zcutils::dirty_pool::{ZcDirtyExtentMap, ZcDirtyExtentRef, ZcDirtyLatestMap, ZcDirtyRecordRef};

const DEFAULT_LANES: usize = 8;
const DEFAULT_WORKERS: usize = 8;
const DEFAULT_RECORD_BYTES: usize = 4096;
const DEFAULT_EXTENT_RECORDS: usize = 256;
const DEFAULT_RECORDS_PER_LANE: usize = 65_536;
const DEFAULT_BLOCK_RECORDS_PER_LANE: usize = 65_536;
const DEFAULT_FORWARD_WINDOW: usize = 1024;
const DEFAULT_READ_REPEATS: usize = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Combine,
    Reduce,
    Read,
    HotRead,
    Mixed,
}

impl Mode {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "combine" | "wal" => Ok(Self::Combine),
            "reduce" | "sync" => Ok(Self::Reduce),
            "read" => Ok(Self::Read),
            "hot-read" | "cache-read" | "dirty-read" => Ok(Self::HotRead),
            "mixed" | "rw" => Ok(Self::Mixed),
            _ => Err(invalid_input(format!(
                "unknown mode {value:?}; expected combine, reduce, read, hot-read, or mixed"
            ))),
        }
    }

    fn needs_wal(self) -> bool {
        matches!(
            self,
            Self::Combine | Self::Reduce | Self::HotRead | Self::Mixed
        )
    }

    fn needs_blockstore(self) -> bool {
        matches!(self, Self::Reduce | Self::Read | Self::Mixed)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Combine => out.write_str("combine"),
            Self::Reduce => out.write_str("reduce"),
            Self::Read => out.write_str("read"),
            Self::HotRead => out.write_str("hot-read"),
            Self::Mixed => out.write_str("mixed"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Pattern {
    Sequential,
    Random,
}

impl Pattern {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "seq" | "sequential" => Ok(Self::Sequential),
            "rand" | "random" => Ok(Self::Random),
            _ => Err(invalid_input(format!(
                "unknown pattern {value:?}; expected seq or random"
            ))),
        }
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sequential => out.write_str("seq"),
            Self::Random => out.write_str("random"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadAccess {
    Copy,
    Ref,
    ForwardRef,
    ForwardExtent,
}

impl ReadAccess {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "copy" | "materialize" | "materialized" => Ok(Self::Copy),
            "ref" | "reference" | "descriptor" | "desc" => Ok(Self::Ref),
            "forward-ref" | "send-ref" | "forward" | "send-descriptor" => Ok(Self::ForwardRef),
            "forward-extent" | "extent-ref" | "send-extent" | "extent-descriptor" => {
                Ok(Self::ForwardExtent)
            }
            _ => Err(invalid_input(format!(
                "unknown read access {value:?}; expected copy, ref, forward-ref, or forward-extent"
            ))),
        }
    }

    fn from_env() -> io::Result<Self> {
        match env::var("URING_PLAY_ZCWAL_REDUCE_READ_ACCESS") {
            Ok(value) if !value.trim().is_empty() => Self::parse(value.trim()),
            _ => Ok(Self::Copy),
        }
    }
}

impl fmt::Display for ReadAccess {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Copy => out.write_str("copy"),
            Self::Ref => out.write_str("ref"),
            Self::ForwardRef => out.write_str("forward-ref"),
            Self::ForwardExtent => out.write_str("forward-extent"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WriteAccess {
    Lease,
}

impl WriteAccess {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "copy" | "wal-copy" | "materialize" => Err(invalid_input(
                "fatal: write-access=copy is disabled here; we have not decided whether copy will be allowed in this dirty-pool path or belongs in another layer of the code",
            )),
            "lease" | "ref" | "reference" | "descriptor" | "rx-lease" => Ok(Self::Lease),
            _ => Err(invalid_input(format!(
                "unknown write access {value:?}; expected copy or lease"
            ))),
        }
    }

    fn from_env() -> io::Result<Self> {
        match env::var("URING_PLAY_ZCWAL_REDUCE_WRITE_ACCESS") {
            Ok(value) if !value.trim().is_empty() => Self::parse(value.trim()),
            _ => Ok(Self::Lease),
        }
    }

    fn copies_payload(self) -> bool {
        false
    }
}

impl fmt::Display for WriteAccess {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lease => out.write_str("lease"),
        }
    }
}

#[derive(Clone, Debug)]
struct Config {
    mode: Mode,
    pattern: Pattern,
    read_access: ReadAccess,
    write_access: WriteAccess,
    lanes: usize,
    workers: usize,
    records_per_lane: usize,
    record_bytes: usize,
    extent_records: usize,
    block_records_per_lane: usize,
    read_pct: u32,
    read_repeats: usize,
    reduce_every_extents: usize,
    forward_window: usize,
    pin: bool,
    cpu_list: Vec<usize>,
    thp: bool,
    hugetlb: bool,
    verify: bool,
    bulk_extents: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct ThreadSwitches {
    voluntary: u64,
    involuntary: u64,
    migrations: u64,
}

#[derive(Clone, Copy, Debug)]
struct Affinity {
    target_cpu: i32,
    applied: bool,
}

#[derive(Debug, Default)]
struct WorkerStats {
    worker: usize,
    lanes: Vec<usize>,
    target_cpu: i32,
    affinity_applied: bool,
    start_cpu: i32,
    end_cpu: i32,
    wall: Duration,
    cpu: Duration,
    ops: u64,
    writes: u64,
    reads: u64,
    dirty_hits: u64,
    dirty_misses: u64,
    wal_bytes: u64,
    wal_ref_bytes: u64,
    reduce_bytes: u64,
    read_bytes: u64,
    read_ref_bytes: u64,
    forward_ref_bytes: u64,
    forward_events: u64,
    forward_completions: u64,
    forward_max_inflight: usize,
    wal_extents: u64,
    reduced_extents: u64,
    contiguous_reduce_extents: u64,
    scattered_reduce_records: u64,
    max_extent_records: usize,
    checksum: u64,
    voluntary_switches: u64,
    involuntary_switches: u64,
    migrations: u64,
}

struct MmapArena {
    label: String,
    ptr: *mut u8,
    len: usize,
}

impl MmapArena {
    fn new(label: impl Into<String>, len: usize, thp: bool, hugetlb: bool) -> io::Result<Self> {
        let label = label.into();
        if len == 0 {
            return Ok(Self {
                label,
                ptr: ptr::null_mut(),
                len,
            });
        }

        let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        if hugetlb {
            flags |= libc::MAP_HUGETLB;
        }

        let mut ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                -1,
                0,
            )
        };

        if ptr == libc::MAP_FAILED && hugetlb {
            eprintln!(
                "PERF WARNING: {label} MAP_HUGETLB allocation of {} failed: {}; falling back to anonymous THP-capable mmap",
                fmt_bytes(len as u64),
                io::Error::last_os_error()
            );
            ptr = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
        }

        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        if thp {
            let ret = unsafe { libc::madvise(ptr, len, libc::MADV_HUGEPAGE) };
            if ret != 0 {
                eprintln!(
                    "PERF WARNING: {label} MADV_HUGEPAGE failed for {}: {}",
                    fmt_bytes(len as u64),
                    io::Error::last_os_error()
                );
            }
        }

        unsafe {
            ptr::write_bytes(ptr as *mut u8, 0, len);
        }

        Ok(Self {
            label,
            ptr: ptr as *mut u8,
            len,
        })
    }

    fn as_ptr_at(&self, offset: usize) -> *const u8 {
        debug_assert!(offset <= self.len);
        unsafe { self.ptr.add(offset) }
    }

    fn as_mut_ptr_at(&mut self, offset: usize) -> *mut u8 {
        debug_assert!(offset <= self.len);
        unsafe { self.ptr.add(offset) }
    }
}

impl Drop for MmapArena {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.len != 0 {
            let ret = unsafe { libc::munmap(self.ptr as *mut c_void, self.len) };
            if ret != 0 {
                eprintln!(
                    "PERF WARNING: failed to munmap {} arena {}: {}",
                    self.label,
                    fmt_bytes(self.len as u64),
                    io::Error::last_os_error()
                );
            }
        }
    }
}

struct ReduceBlockStore {
    arena: MmapArena,
    record_bytes: usize,
    records: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReadRef {
    dirty: bool,
    token: u64,
    sequence: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct ForwardDescriptor {
    token: u64,
    sequence: u64,
    flags: u64,
}

struct RefForwarder {
    ring: Vec<ForwardDescriptor>,
    head: usize,
    tail: usize,
    in_flight: usize,
    completion_mix: u64,
}

impl RefForwarder {
    fn new(window: usize) -> Self {
        Self {
            ring: vec![ForwardDescriptor::default(); window],
            head: 0,
            tail: 0,
            in_flight: 0,
            completion_mix: 0,
        }
    }

    fn forward(&mut self, read_ref: ReadRef, bytes: usize, stats: &mut WorkerStats) {
        if self.in_flight == self.ring.len() {
            self.complete_one(stats);
        }

        let slot = self.tail % self.ring.len();
        self.ring[slot] = ForwardDescriptor {
            token: read_ref.token,
            sequence: read_ref.sequence,
            flags: u64::from(read_ref.dirty),
        };
        self.tail = self.tail.wrapping_add(1);
        self.in_flight = self.in_flight.saturating_add(1);
        stats.forward_events = stats.forward_events.saturating_add(1);
        stats.forward_ref_bytes = stats.forward_ref_bytes.saturating_add(bytes as u64);
        stats.forward_max_inflight = stats.forward_max_inflight.max(self.in_flight);
    }

    fn forward_extent(
        &mut self,
        read_ref: ReadRef,
        records: usize,
        bytes: usize,
        stats: &mut WorkerStats,
    ) {
        if self.in_flight == self.ring.len() {
            self.complete_one(stats);
        }

        let slot = self.tail % self.ring.len();
        self.ring[slot] = ForwardDescriptor {
            token: read_ref.token ^ (records as u64).rotate_left(23),
            sequence: read_ref.sequence,
            flags: u64::from(read_ref.dirty) | ((records as u64) << 1),
        };
        self.tail = self.tail.wrapping_add(1);
        self.in_flight = self.in_flight.saturating_add(1);
        stats.forward_events = stats.forward_events.saturating_add(1);
        stats.forward_ref_bytes = stats.forward_ref_bytes.saturating_add(bytes as u64);
        stats.forward_max_inflight = stats.forward_max_inflight.max(self.in_flight);
    }

    fn drain(&mut self, stats: &mut WorkerStats) {
        while self.in_flight != 0 {
            self.complete_one(stats);
        }
    }

    fn complete_one(&mut self, stats: &mut WorkerStats) {
        debug_assert!(self.in_flight != 0);
        let slot = self.head % self.ring.len();
        let desc = self.ring[slot];
        self.head = self.head.wrapping_add(1);
        self.in_flight = self.in_flight.saturating_sub(1);
        self.completion_mix = self.completion_mix.wrapping_add(
            desc.token
                ^ desc.sequence.rotate_left((self.head & 63) as u32)
                ^ desc.flags.wrapping_mul(0x9e37_79b9_7f4a_7c15),
        );
        stats.forward_completions = stats.forward_completions.saturating_add(1);
        stats.checksum = stats.checksum.wrapping_add(self.completion_mix);
    }
}

impl ReduceBlockStore {
    fn new(
        lane: usize,
        records: usize,
        record_bytes: usize,
        thp: bool,
        hugetlb: bool,
    ) -> io::Result<Self> {
        let bytes = records
            .checked_mul(record_bytes)
            .ok_or_else(|| invalid_input("blockstore arena size overflow"))?;
        Ok(Self {
            arena: MmapArena::new(format!("lane{lane}-reduce-blockstore"), bytes, thp, hugetlb)?,
            record_bytes,
            records,
        })
    }

    fn read_record_into(&self, logical_record: usize, out: &mut [u8]) {
        debug_assert_eq!(out.len(), self.record_bytes);
        debug_assert!(logical_record < self.records);
        unsafe {
            ptr::copy_nonoverlapping(
                self.arena
                    .as_ptr_at(logical_record.saturating_mul(self.record_bytes)),
                out.as_mut_ptr(),
                self.record_bytes,
            );
        }
    }

    fn record_ref_token(&self, logical_record: usize) -> u64 {
        debug_assert!(logical_record < self.records);
        let offset = logical_record.saturating_mul(self.record_bytes);
        let ptr = self.arena.as_ptr_at(offset) as usize as u64;
        0xb10c_570f_0000_0000u64.wrapping_add(logical_record as u64)
            ^ ptr.rotate_left(17)
            ^ self.record_bytes as u64
    }

    fn write_contiguous_from(&mut self, first_record: usize, src: *const u8, records: usize) {
        debug_assert!(first_record + records <= self.records);
        unsafe {
            ptr::copy_nonoverlapping(
                src,
                self.arena
                    .as_mut_ptr_at(first_record.saturating_mul(self.record_bytes)),
                records.saturating_mul(self.record_bytes),
            );
        }
    }

    fn write_one_from(&mut self, logical_record: usize, src: *const u8) {
        debug_assert!(logical_record < self.records);
        unsafe {
            ptr::copy_nonoverlapping(
                src,
                self.arena
                    .as_mut_ptr_at(logical_record.saturating_mul(self.record_bytes)),
                self.record_bytes,
            );
        }
    }
}

struct WalCombiner {
    lane: usize,
    record_bytes: usize,
    extent_records: usize,
    write_access: WriteAccess,
    wal: Option<MmapArena>,
    leased_ptrs: Vec<usize>,
    wal_logicals: Vec<usize>,
    dirty: ZcDirtyLatestMap,
    next_wal_record: usize,
    pending_extent_start: usize,
    pending_extent_records: usize,
    wal_extents: u64,
    max_extent_records: usize,
    extent_refs: ZcDirtyExtentMap,
}

impl WalCombiner {
    fn new(
        lane: usize,
        wal_records: usize,
        block_records: usize,
        record_bytes: usize,
        extent_records: usize,
        write_access: WriteAccess,
        thp: bool,
        hugetlb: bool,
    ) -> io::Result<Self> {
        let bytes = wal_records
            .checked_mul(record_bytes)
            .ok_or_else(|| invalid_input("wal arena size overflow"))?;
        let wal = write_access
            .copies_payload()
            .then(|| MmapArena::new(format!("lane{lane}-combined-wal"), bytes, thp, hugetlb))
            .transpose()?;
        let leased_ptrs = if write_access.copies_payload() {
            Vec::new()
        } else {
            vec![0usize; wal_records]
        };
        Ok(Self {
            lane,
            record_bytes,
            extent_records,
            write_access,
            wal,
            leased_ptrs,
            wal_logicals: vec![usize::MAX; wal_records],
            dirty: ZcDirtyLatestMap::new(block_records),
            next_wal_record: 0,
            pending_extent_start: 0,
            pending_extent_records: 0,
            wal_extents: 0,
            max_extent_records: 0,
            extent_refs: ZcDirtyExtentMap::new(),
        })
    }

    fn append_record(&mut self, logical_record: usize, source: &[u8]) -> io::Result<bool> {
        if self.next_wal_record >= self.wal_logicals.len() {
            return Err(invalid_input(format!(
                "lane {} exhausted wal capacity {}; increase --records-per-lane or read percentage",
                self.lane,
                self.wal_logicals.len()
            )));
        }

        let wal_record = self.next_wal_record;
        let offset = wal_record.saturating_mul(self.record_bytes);
        if self.write_access.copies_payload() {
            unsafe {
                ptr::copy_nonoverlapping(
                    source.as_ptr(),
                    self.wal.as_mut().unwrap().as_mut_ptr_at(offset),
                    self.record_bytes,
                );
            }
        } else {
            self.leased_ptrs[wal_record] = source.as_ptr() as usize;
        }
        self.wal_logicals[wal_record] = logical_record;
        self.admit_dirty(logical_record, wal_record);
        self.next_wal_record = self.next_wal_record.saturating_add(1);
        self.pending_extent_records = self.pending_extent_records.saturating_add(1);

        let extent_full = self.pending_extent_records >= self.extent_records;
        if extent_full {
            self.finish_pending_extent();
        }
        Ok(extent_full)
    }

    fn append_extent<F>(
        &mut self,
        source: &[u8],
        records: usize,
        mut logical_at: F,
    ) -> io::Result<()>
    where
        F: FnMut(usize) -> usize,
    {
        if records == 0 {
            return Ok(());
        }
        if self.next_wal_record.saturating_add(records) > self.wal_logicals.len() {
            return Err(invalid_input(format!(
                "lane {} exhausted wal capacity {}; increase --records-per-lane or read percentage",
                self.lane,
                self.wal_logicals.len()
            )));
        }
        let bytes = records.saturating_mul(self.record_bytes);
        if source.len() < bytes {
            return Err(invalid_input(format!(
                "source extent has {} bytes but {bytes} are required",
                source.len()
            )));
        }

        let wal_start = self.next_wal_record;
        if self.write_access.copies_payload() {
            unsafe {
                ptr::copy_nonoverlapping(
                    source.as_ptr(),
                    self.wal
                        .as_mut()
                        .unwrap()
                        .as_mut_ptr_at(wal_start.saturating_mul(self.record_bytes)),
                    bytes,
                );
            }
        }
        for index in 0..records {
            let wal_record = wal_start + index;
            let logical = logical_at(index);
            if !self.write_access.copies_payload() {
                self.leased_ptrs[wal_record] =
                    unsafe { source.as_ptr().add(index.saturating_mul(self.record_bytes)) }
                        as usize;
            }
            self.wal_logicals[wal_record] = logical;
            self.admit_dirty(logical, wal_record);
        }
        self.next_wal_record = self.next_wal_record.saturating_add(records);
        self.pending_extent_records = self.pending_extent_records.saturating_add(records);
        while self.pending_extent_records >= self.extent_records {
            self.finish_pending_extent();
        }
        Ok(())
    }

    fn finish_pending_extent(&mut self) {
        if self.pending_extent_records == 0 {
            return;
        }
        let wal_start = self.pending_extent_start;
        let records = self.pending_extent_records;
        if let Some(extent_ref) = self.build_extent_ref(wal_start, records) {
            self.extent_refs.admit(extent_ref);
        }
        self.wal_extents = self.wal_extents.saturating_add(1);
        self.max_extent_records = self.max_extent_records.max(records);
        self.pending_extent_start = self.next_wal_record;
        self.pending_extent_records = 0;
    }

    fn build_extent_ref(&self, wal_start: usize, records: usize) -> Option<ZcDirtyExtentRef> {
        let logical_start = self.extent_is_contiguous(wal_start, records, usize::MAX)?;
        if !self.payload_extent_is_contiguous(wal_start, records) {
            return None;
        }
        let sequence = wal_start as u64;
        let record_count = u32::try_from(records).ok()?;
        let byte_len = records.checked_mul(self.record_bytes)? as u64;
        Some(ZcDirtyExtentRef::new(
            logical_start as u64,
            record_count,
            sequence,
            self.lane as u32,
            wal_start as u64,
            self.payload_ptr_at(wal_start) as usize as u64,
            byte_len,
        ))
    }

    fn flush(&mut self) {
        self.finish_pending_extent();
    }

    fn extent_is_contiguous(
        &self,
        start: usize,
        records: usize,
        block_records: usize,
    ) -> Option<usize> {
        if records == 0 {
            return None;
        }
        let first = self.wal_logicals[start];
        if first == usize::MAX
            || first
                .checked_add(records)
                .is_none_or(|end| end > block_records)
        {
            return None;
        }
        for index in 1..records {
            if self.wal_logicals[start + index] != first + index {
                return None;
            }
        }
        Some(first)
    }

    fn admit_dirty(&mut self, logical_record: usize, wal_record: usize) {
        let offset = wal_record.saturating_mul(self.record_bytes);
        let byte_offset = if self.write_access.copies_payload() {
            offset as u64
        } else {
            self.leased_ptrs[wal_record] as u64
        };
        self.dirty.admit(
            logical_record,
            ZcDirtyRecordRef::new(
                wal_record as u64,
                self.lane as u32,
                wal_record as u64,
                byte_offset,
                self.record_bytes as u32,
            ),
        );
    }

    fn payload_ptr_at(&self, wal_record: usize) -> *const u8 {
        if self.write_access.copies_payload() {
            self.wal
                .as_ref()
                .unwrap()
                .as_ptr_at(wal_record.saturating_mul(self.record_bytes))
        } else {
            self.leased_ptrs[wal_record] as *const u8
        }
    }

    fn payload_extent_is_contiguous(&self, start: usize, records: usize) -> bool {
        if self.write_access.copies_payload() {
            return true;
        }
        if records == 0 {
            return false;
        }
        let first = self.leased_ptrs[start];
        first != 0
            && (1..records).all(|index| {
                self.leased_ptrs[start + index]
                    == first.saturating_add(index.saturating_mul(self.record_bytes))
            })
    }

    fn read_dirty_into(&self, logical_record: usize, out: &mut [u8]) -> bool {
        let Some(desc) = self.dirty.get(logical_record) else {
            return false;
        };
        unsafe {
            ptr::copy_nonoverlapping(
                self.payload_ptr_at(desc.slot as usize),
                out.as_mut_ptr(),
                self.record_bytes,
            );
        }
        true
    }

    fn read_dirty_ref(&self, logical_record: usize) -> Option<ReadRef> {
        self.dirty.get(logical_record).map(|desc| ReadRef {
            dirty: true,
            token: desc.descriptor_token(),
            sequence: desc.sequence,
        })
    }

    fn read_dirty_extent_ref(&self, logical_record: usize, records: usize) -> Option<ReadRef> {
        let extent = self
            .extent_refs
            .get_covering(logical_record as u64, u32::try_from(records).ok()?)?;
        self.dirty.get(logical_record)?;
        Some(ReadRef {
            dirty: true,
            token: extent.descriptor_token() ^ (records as u64).rotate_left(11),
            sequence: extent.sequence,
        })
    }

    fn clear_dirty_if_current(&mut self, logical_record: usize, wal_record: usize) {
        self.dirty
            .clear_if_current(logical_record, wal_record as u64);
    }
}

struct LaneState {
    combiner: Option<WalCombiner>,
    blockstore: Option<ReduceBlockStore>,
    completed_wal_extents: u64,
    reduced_extents: u64,
    contiguous_reduce_extents: u64,
    scattered_reduce_records: u64,
}

impl LaneState {
    fn new(lane: usize, config: &Config) -> io::Result<Self> {
        let blockstore = if config.mode.needs_blockstore() {
            Some(ReduceBlockStore::new(
                lane,
                config.block_records_per_lane,
                config.record_bytes,
                config.thp,
                config.hugetlb,
            )?)
        } else {
            None
        };
        let combiner = if config.mode.needs_wal() {
            Some(WalCombiner::new(
                lane,
                config.records_per_lane,
                config.block_records_per_lane,
                config.record_bytes,
                config.extent_records,
                config.write_access,
                config.thp,
                config.hugetlb,
            )?)
        } else {
            None
        };

        Ok(Self {
            combiner,
            blockstore,
            completed_wal_extents: 0,
            reduced_extents: 0,
            contiguous_reduce_extents: 0,
            scattered_reduce_records: 0,
        })
    }

    fn append_write(&mut self, logical_record: usize, source: &[u8]) -> io::Result<bool> {
        let combiner = self
            .combiner
            .as_mut()
            .ok_or_else(|| invalid_input("write attempted without a WAL combiner"))?;
        let finished = combiner.append_record(logical_record, source)?;
        if finished {
            self.completed_wal_extents = self.completed_wal_extents.saturating_add(1);
        }
        Ok(finished)
    }

    fn append_write_extent<F>(
        &mut self,
        source: &[u8],
        records: usize,
        logical_at: F,
    ) -> io::Result<()>
    where
        F: FnMut(usize) -> usize,
    {
        let combiner = self
            .combiner
            .as_mut()
            .ok_or_else(|| invalid_input("write extent attempted without a WAL combiner"))?;
        let before = combiner.wal_extents;
        combiner.append_extent(source, records, logical_at)?;
        self.completed_wal_extents = self
            .completed_wal_extents
            .saturating_add(combiner.wal_extents.saturating_sub(before));
        Ok(())
    }

    fn flush_wal(&mut self) {
        let Some(combiner) = self.combiner.as_mut() else {
            return;
        };
        let before = combiner.wal_extents;
        combiner.flush();
        self.completed_wal_extents = self
            .completed_wal_extents
            .saturating_add(combiner.wal_extents.saturating_sub(before));
    }

    fn read_into(&mut self, logical_record: usize, out: &mut [u8]) -> bool {
        if let Some(combiner) = self.combiner.as_ref() {
            if combiner.read_dirty_into(logical_record, out) {
                return true;
            }
        }
        if let Some(blockstore) = self.blockstore.as_ref() {
            blockstore.read_record_into(logical_record, out);
        } else {
            out.fill(0);
        }
        false
    }

    fn read_ref(&self, logical_record: usize) -> ReadRef {
        if let Some(combiner) = self.combiner.as_ref() {
            if let Some(read_ref) = combiner.read_dirty_ref(logical_record) {
                return read_ref;
            }
        }
        if let Some(blockstore) = self.blockstore.as_ref() {
            return ReadRef {
                dirty: false,
                token: blockstore.record_ref_token(logical_record),
                sequence: logical_record as u64,
            };
        }
        ReadRef {
            dirty: false,
            token: 0,
            sequence: u64::MAX,
        }
    }

    fn read_extent_ref(&self, logical_record: usize, records: usize) -> ReadRef {
        if let Some(combiner) = self.combiner.as_ref() {
            if let Some(read_ref) = combiner.read_dirty_extent_ref(logical_record, records) {
                return read_ref;
            }
        }
        if let Some(blockstore) = self.blockstore.as_ref() {
            return ReadRef {
                dirty: false,
                token: blockstore.record_ref_token(logical_record)
                    ^ (records as u64).rotate_left(37),
                sequence: logical_record as u64,
            };
        }
        ReadRef {
            dirty: false,
            token: (records as u64).rotate_left(37),
            sequence: u64::MAX,
        }
    }

    fn reduce_latest_completed_extent(&mut self, start: usize, records: usize) -> io::Result<u64> {
        self.reduce_extent(start, records)
    }

    fn reduce_extent(&mut self, start: usize, records: usize) -> io::Result<u64> {
        if records == 0 {
            return Ok(0);
        }
        let block_records = self
            .blockstore
            .as_ref()
            .map(|store| store.records)
            .ok_or_else(|| invalid_input("reduce attempted without a blockstore"))?;
        let record_bytes = self
            .combiner
            .as_ref()
            .map(|combiner| combiner.record_bytes)
            .ok_or_else(|| invalid_input("reduce attempted without a WAL combiner"))?;

        let first_contiguous = self.combiner.as_ref().and_then(|combiner| {
            combiner
                .extent_is_contiguous(start, records, block_records)
                .filter(|_| combiner.payload_extent_is_contiguous(start, records))
        });

        if let Some(first_record) = first_contiguous {
            let src = self.combiner.as_ref().unwrap().payload_ptr_at(start);
            self.blockstore
                .as_mut()
                .unwrap()
                .write_contiguous_from(first_record, src, records);
            let combiner = self.combiner.as_mut().unwrap();
            for index in 0..records {
                combiner.clear_dirty_if_current(first_record + index, start + index);
            }
            self.contiguous_reduce_extents = self.contiguous_reduce_extents.saturating_add(1);
        } else {
            for index in 0..records {
                let (logical_record, src) = {
                    let combiner = self.combiner.as_ref().unwrap();
                    let wal_record = start + index;
                    (
                        combiner.wal_logicals[wal_record],
                        combiner.payload_ptr_at(wal_record),
                    )
                };
                self.blockstore
                    .as_mut()
                    .unwrap()
                    .write_one_from(logical_record, src);
                self.combiner
                    .as_mut()
                    .unwrap()
                    .clear_dirty_if_current(logical_record, start + index);
            }
            self.scattered_reduce_records =
                self.scattered_reduce_records.saturating_add(records as u64);
        }

        self.reduced_extents = self.reduced_extents.saturating_add(1);
        Ok(records.saturating_mul(record_bytes) as u64)
    }

    fn max_extent_records(&self) -> usize {
        self.combiner
            .as_ref()
            .map(|combiner| combiner.max_extent_records)
            .unwrap_or(0)
    }

    fn wal_extents(&self) -> u64 {
        self.combiner
            .as_ref()
            .map(|combiner| combiner.wal_extents)
            .unwrap_or(0)
    }
}

fn main() -> io::Result<()> {
    let config = Arc::new(Config::parse()?);
    validate_config(&config)?;
    print_config(&config);

    let worker_lanes = assign_lanes(config.lanes, config.workers);
    for (worker, lanes) in worker_lanes.iter().enumerate() {
        println!(
            "zcwal-reduce-topology: worker={worker} lanes={} cpu_target={}",
            fmt_lanes(lanes),
            target_cpu_for_worker(&config, worker)
                .map(|cpu| cpu.to_string())
                .unwrap_or_else(|| "unpinned".to_string())
        );
    }
    if !config.pin {
        eprintln!(
            "PERF WARNING: zcwal-reduce-bench is running unpinned; set --pin and --cpu-list or URING_PLAY_PIN_CPUS=1/URING_PLAY_PIN_CPU_LIST before trusting topology-sensitive numbers"
        );
    }

    let barrier = Arc::new(Barrier::new(config.workers + 1));
    let mut handles = Vec::with_capacity(config.workers);
    for worker in 0..config.workers {
        let config = Arc::clone(&config);
        let barrier = Arc::clone(&barrier);
        let lanes = worker_lanes[worker].clone();
        handles.push(thread::spawn(move || {
            run_worker(worker, lanes, config, barrier)
        }));
    }

    barrier.wait();

    let mut stats = Vec::with_capacity(handles.len());
    for handle in handles {
        stats.push(
            handle
                .join()
                .map_err(|_| invalid_input("worker thread panicked"))??,
        );
    }

    print_stats(&stats);
    Ok(())
}

impl Config {
    fn parse() -> io::Result<Self> {
        let mut config = Self {
            mode: Mode::Mixed,
            pattern: Pattern::Sequential,
            read_access: ReadAccess::from_env()?,
            write_access: WriteAccess::from_env()?,
            lanes: DEFAULT_LANES,
            workers: DEFAULT_WORKERS,
            records_per_lane: DEFAULT_RECORDS_PER_LANE,
            record_bytes: DEFAULT_RECORD_BYTES,
            extent_records: DEFAULT_EXTENT_RECORDS,
            block_records_per_lane: DEFAULT_BLOCK_RECORDS_PER_LANE,
            read_pct: 50,
            read_repeats: parse_count_env(
                "URING_PLAY_ZCWAL_REDUCE_READ_REPEATS",
                DEFAULT_READ_REPEATS,
            )?,
            reduce_every_extents: 0,
            forward_window: parse_count_env(
                "URING_PLAY_ZCWAL_REDUCE_FORWARD_WINDOW",
                DEFAULT_FORWARD_WINDOW,
            )?,
            pin: env_truthy("URING_PLAY_PIN_CPUS"),
            cpu_list: parse_cpu_list_env(),
            thp: !env_falsey("URING_PLAY_ZCWAL_REDUCE_THP"),
            hugetlb: env_truthy("URING_PLAY_ZCWAL_REDUCE_HUGETLB"),
            verify: env_truthy("URING_PLAY_ZCWAL_REDUCE_VERIFY"),
            bulk_extents: !env_falsey("URING_PLAY_ZCWAL_REDUCE_BULK_EXTENTS"),
        };

        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_usage();
                    std::process::exit(0);
                }
                "--mode" => config.mode = Mode::parse(&next_arg(&mut args, "--mode")?)?,
                "--pattern" => config.pattern = Pattern::parse(&next_arg(&mut args, "--pattern")?)?,
                "--lanes" => config.lanes = parse_count(&next_arg(&mut args, "--lanes")?)?,
                "--workers" => config.workers = parse_count(&next_arg(&mut args, "--workers")?)?,
                "--records-per-lane" => {
                    config.records_per_lane =
                        parse_count(&next_arg(&mut args, "--records-per-lane")?)?
                }
                "--record-bytes" => {
                    config.record_bytes = parse_count(&next_arg(&mut args, "--record-bytes")?)?
                }
                "--extent-records" => {
                    config.extent_records = parse_count(&next_arg(&mut args, "--extent-records")?)?
                }
                "--block-records-per-lane" | "--block-records" => {
                    config.block_records_per_lane =
                        parse_count(&next_arg(&mut args, "--block-records-per-lane")?)?
                }
                "--read-pct" => {
                    config.read_pct = next_arg(&mut args, "--read-pct")?
                        .parse::<u32>()
                        .map_err(|err| invalid_input(format!("invalid --read-pct: {err}")))?
                }
                "--read-repeats" | "--read-repeat" => {
                    config.read_repeats = parse_count(&next_arg(&mut args, "--read-repeats")?)?
                }
                "--reduce-every-extents" => {
                    config.reduce_every_extents =
                        parse_count(&next_arg(&mut args, "--reduce-every-extents")?)?
                }
                "--forward-window" => {
                    config.forward_window = parse_count(&next_arg(&mut args, "--forward-window")?)?
                }
                "--read-access" => {
                    config.read_access = ReadAccess::parse(&next_arg(&mut args, "--read-access")?)?
                }
                "--read-copy" | "--materialize-reads" => config.read_access = ReadAccess::Copy,
                "--read-ref" | "--descriptor-reads" => config.read_access = ReadAccess::Ref,
                "--forward-ref" | "--send-ref" | "--send-descriptor-reads" => {
                    config.read_access = ReadAccess::ForwardRef
                }
                "--forward-extent" | "--send-extent-reads" | "--extent-descriptor-reads" => {
                    config.read_access = ReadAccess::ForwardExtent
                }
                "--write-access" => {
                    config.write_access =
                        WriteAccess::parse(&next_arg(&mut args, "--write-access")?)?
                }
                "--write-copy" | "--materialize-writes" => {
                    return Err(WriteAccess::parse("copy").unwrap_err());
                }
                "--write-lease" | "--lease-writes" | "--descriptor-writes" => {
                    config.write_access = WriteAccess::Lease
                }
                "--pin" => config.pin = true,
                "--no-pin" => config.pin = false,
                "--cpu-list" => {
                    config.cpu_list = parse_cpu_list(&next_arg(&mut args, "--cpu-list")?)?
                }
                "--thp" => config.thp = true,
                "--no-thp" => config.thp = false,
                "--hugetlb" => config.hugetlb = true,
                "--no-hugetlb" => config.hugetlb = false,
                "--verify" => config.verify = true,
                "--no-verify" => config.verify = false,
                "--bulk-extents" => config.bulk_extents = true,
                "--per-record-ingest" => config.bulk_extents = false,
                _ => {
                    return Err(invalid_input(format!(
                        "unknown argument {arg:?}; pass --help for usage"
                    )));
                }
            }
        }

        Ok(config)
    }
}

fn run_worker(
    worker: usize,
    lanes: Vec<usize>,
    config: Arc<Config>,
    barrier: Arc<Barrier>,
) -> io::Result<WorkerStats> {
    let affinity = maybe_pin_worker(&config, worker);
    let mut lane_states = Vec::with_capacity(lanes.len());
    for lane in &lanes {
        lane_states.push((*lane, LaneState::new(*lane, &config)?));
    }
    let source = make_source_pattern(worker, config.record_bytes);
    let source_extent = make_source_extent(worker, config.record_bytes, config.extent_records);
    let mut sink = vec![0u8; config.record_bytes];
    let mut forwarder = RefForwarder::new(config.forward_window);

    if config.mode == Mode::HotRead {
        for (lane, state) in lane_states.iter_mut() {
            prefill_lane_dirty_cache(*lane, state, &source_extent, &config)?;
        }
    }

    barrier.wait();

    let tid = current_tid();
    let start_switches = read_thread_context_switches(tid).unwrap_or_default();
    let start_cpu_time = thread_cpu_time().unwrap_or_default();
    let start_cpu = current_cpu();
    let started = Instant::now();

    let mut stats = WorkerStats {
        worker,
        lanes: lanes.clone(),
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
        start_cpu,
        ..WorkerStats::default()
    };

    for (lane_index, (lane, state)) in lane_states.iter_mut().enumerate() {
        match config.mode {
            Mode::Combine => {
                run_lane_combine(*lane, state, &source, &source_extent, &config, &mut stats)?
            }
            Mode::Reduce => {
                run_lane_reduce(*lane, state, &source, &source_extent, &config, &mut stats)?
            }
            Mode::HotRead => run_lane_read(
                *lane,
                lane_index,
                state,
                &config,
                &mut sink,
                &mut forwarder,
                &mut stats,
            ),
            Mode::Read => run_lane_read(
                *lane,
                lane_index,
                state,
                &config,
                &mut sink,
                &mut forwarder,
                &mut stats,
            ),
            Mode::Mixed => run_lane_mixed(
                *lane,
                lane_index,
                state,
                &source,
                &config,
                &mut sink,
                &mut forwarder,
                &mut stats,
            )?,
        }
        state.flush_wal();
        stats.wal_extents = stats.wal_extents.saturating_add(state.wal_extents());
        stats.reduced_extents = stats.reduced_extents.saturating_add(state.reduced_extents);
        stats.contiguous_reduce_extents = stats
            .contiguous_reduce_extents
            .saturating_add(state.contiguous_reduce_extents);
        stats.scattered_reduce_records = stats
            .scattered_reduce_records
            .saturating_add(state.scattered_reduce_records);
        stats.max_extent_records = stats.max_extent_records.max(state.max_extent_records());
    }
    forwarder.drain(&mut stats);

    stats.wall = started.elapsed();
    stats.cpu = thread_cpu_time()
        .unwrap_or(start_cpu_time)
        .saturating_sub(start_cpu_time);
    stats.end_cpu = current_cpu();
    let end_switches = read_thread_context_switches(tid).unwrap_or(start_switches);
    stats.voluntary_switches = end_switches
        .voluntary
        .saturating_sub(start_switches.voluntary);
    stats.involuntary_switches = end_switches
        .involuntary
        .saturating_sub(start_switches.involuntary);
    stats.migrations = end_switches
        .migrations
        .saturating_sub(start_switches.migrations);

    Ok(stats)
}

fn prefill_lane_dirty_cache(
    lane: usize,
    state: &mut LaneState,
    source_extent: &[u8],
    config: &Config,
) -> io::Result<()> {
    let mut op = 0usize;
    while op < config.block_records_per_lane {
        let records = config
            .extent_records
            .min(config.block_records_per_lane.saturating_sub(op));
        state.append_write_extent(source_extent, records, |index| {
            logical_record(config, lane, op + index)
        })?;
        op = op.saturating_add(records);
    }
    state.flush_wal();
    Ok(())
}

fn run_lane_combine(
    lane: usize,
    state: &mut LaneState,
    source: &[u8],
    source_extent: &[u8],
    config: &Config,
    stats: &mut WorkerStats,
) -> io::Result<()> {
    if config.bulk_extents {
        let mut op = 0usize;
        while op < config.records_per_lane {
            let records = config.extent_records.min(config.records_per_lane - op);
            state.append_write_extent(source_extent, records, |index| {
                logical_record(config, lane, op + index)
            })?;
            stats.ops = stats.ops.saturating_add(records as u64);
            stats.writes = stats.writes.saturating_add(records as u64);
            record_write_payload(records, config, stats);
            op = op.saturating_add(records);
        }
        return Ok(());
    }

    for op in 0..config.records_per_lane {
        let logical = logical_record(config, lane, op);
        state.append_write(logical, source)?;
        stats.ops = stats.ops.saturating_add(1);
        stats.writes = stats.writes.saturating_add(1);
        record_write_payload(1, config, stats);
    }
    Ok(())
}

fn run_lane_reduce(
    lane: usize,
    state: &mut LaneState,
    source: &[u8],
    source_extent: &[u8],
    config: &Config,
    stats: &mut WorkerStats,
) -> io::Result<()> {
    if config.bulk_extents {
        let mut op = 0usize;
        let mut extent_start = 0usize;
        while op < config.records_per_lane {
            let records = config.extent_records.min(config.records_per_lane - op);
            state.append_write_extent(source_extent, records, |index| {
                logical_record(config, lane, op + index)
            })?;
            stats.ops = stats.ops.saturating_add(records as u64);
            stats.writes = stats.writes.saturating_add(records as u64);
            record_write_payload(records, config, stats);
            stats.reduce_bytes = stats
                .reduce_bytes
                .saturating_add(state.reduce_latest_completed_extent(extent_start, records)?);
            extent_start = extent_start.saturating_add(records);
            op = op.saturating_add(records);
        }
        return Ok(());
    }

    let mut extent_start = 0usize;
    let mut extent_records = 0usize;
    for op in 0..config.records_per_lane {
        let logical = logical_record(config, lane, op);
        state.append_write(logical, source)?;
        extent_records = extent_records.saturating_add(1);
        stats.ops = stats.ops.saturating_add(1);
        stats.writes = stats.writes.saturating_add(1);
        record_write_payload(1, config, stats);
        if extent_records == config.extent_records {
            stats.reduce_bytes = stats.reduce_bytes.saturating_add(
                state.reduce_latest_completed_extent(extent_start, extent_records)?,
            );
            extent_start = extent_start.saturating_add(extent_records);
            extent_records = 0;
        }
    }
    if extent_records != 0 {
        state.flush_wal();
        stats.reduce_bytes = stats
            .reduce_bytes
            .saturating_add(state.reduce_latest_completed_extent(extent_start, extent_records)?);
    }
    Ok(())
}

fn run_lane_read(
    lane: usize,
    _lane_index: usize,
    state: &mut LaneState,
    config: &Config,
    sink: &mut [u8],
    forwarder: &mut RefForwarder,
    stats: &mut WorkerStats,
) {
    let total_ops = config.records_per_lane.saturating_mul(config.read_repeats);
    if config.read_access == ReadAccess::ForwardExtent {
        let mut op = 0usize;
        while op < total_ops {
            let records = config.extent_records.min(total_ops - op);
            let logical = logical_record(config, lane, op);
            let read_ref = state.read_extent_ref(logical, records);
            if read_ref.dirty {
                stats.dirty_hits = stats.dirty_hits.saturating_add(records as u64);
            } else {
                stats.dirty_misses = stats.dirty_misses.saturating_add(records as u64);
            }
            stats.checksum = stats
                .checksum
                .wrapping_add(read_ref.token ^ (records as u64).rotate_left(11));
            let bytes = records.saturating_mul(config.record_bytes);
            stats.read_ref_bytes = stats.read_ref_bytes.saturating_add(bytes as u64);
            stats.reads = stats.reads.saturating_add(records as u64);
            stats.ops = stats.ops.saturating_add(records as u64);
            forwarder.forward_extent(read_ref, records, bytes, stats);
            op = op.saturating_add(records);
        }
        return;
    }
    for op in 0..total_ops {
        let logical = logical_record(config, lane, op);
        record_read(state, logical, config, sink, forwarder, stats);
        stats.ops = stats.ops.saturating_add(1);
    }
}

fn run_lane_mixed(
    lane: usize,
    _lane_index: usize,
    state: &mut LaneState,
    source: &[u8],
    config: &Config,
    sink: &mut [u8],
    forwarder: &mut RefForwarder,
    stats: &mut WorkerStats,
) -> io::Result<()> {
    let mut completed_extents_since_reduce = 0usize;
    let mut extent_start = 0usize;
    let mut extent_records = 0usize;
    for op in 0..config.records_per_lane {
        let logical = logical_record(config, lane, op);
        let is_read = mixed_is_read(lane, op, config.read_pct);
        if is_read {
            record_read(state, logical, config, sink, forwarder, stats);
        } else {
            let finished = state.append_write(logical, source)?;
            extent_records = extent_records.saturating_add(1);
            stats.writes = stats.writes.saturating_add(1);
            record_write_payload(1, config, stats);
            if finished {
                completed_extents_since_reduce = completed_extents_since_reduce.saturating_add(1);
                if config.reduce_every_extents != 0
                    && completed_extents_since_reduce >= config.reduce_every_extents
                {
                    stats.reduce_bytes = stats.reduce_bytes.saturating_add(
                        state.reduce_latest_completed_extent(extent_start, extent_records)?,
                    );
                    extent_start = extent_start.saturating_add(extent_records);
                    extent_records = 0;
                    completed_extents_since_reduce = 0;
                }
            }
        }
        stats.ops = stats.ops.saturating_add(1);
    }

    if config.reduce_every_extents != 0 && extent_records != 0 {
        state.flush_wal();
        stats.reduce_bytes = stats
            .reduce_bytes
            .saturating_add(state.reduce_latest_completed_extent(extent_start, extent_records)?);
    }
    Ok(())
}

fn record_write_payload(records: usize, config: &Config, stats: &mut WorkerStats) {
    let bytes = records.saturating_mul(config.record_bytes) as u64;
    if config.write_access.copies_payload() {
        stats.wal_bytes = stats.wal_bytes.saturating_add(bytes);
    } else {
        stats.wal_ref_bytes = stats.wal_ref_bytes.saturating_add(bytes);
    }
}

fn record_read(
    state: &mut LaneState,
    logical: usize,
    config: &Config,
    sink: &mut [u8],
    forwarder: &mut RefForwarder,
    stats: &mut WorkerStats,
) {
    match config.read_access {
        ReadAccess::Copy => {
            let dirty = state.read_into(logical, sink);
            if dirty {
                stats.dirty_hits = stats.dirty_hits.saturating_add(1);
            } else {
                stats.dirty_misses = stats.dirty_misses.saturating_add(1);
            }
            stats.checksum = stats.checksum.wrapping_add(checksum_record(sink));
            stats.read_bytes = stats.read_bytes.saturating_add(config.record_bytes as u64);
        }
        ReadAccess::Ref => {
            let read_ref = state.read_ref(logical);
            if read_ref.dirty {
                stats.dirty_hits = stats.dirty_hits.saturating_add(1);
            } else {
                stats.dirty_misses = stats.dirty_misses.saturating_add(1);
            }
            stats.checksum = stats.checksum.wrapping_add(read_ref.token);
            stats.read_ref_bytes = stats
                .read_ref_bytes
                .saturating_add(config.record_bytes as u64);
        }
        ReadAccess::ForwardRef | ReadAccess::ForwardExtent => {
            let read_ref = state.read_ref(logical);
            if read_ref.dirty {
                stats.dirty_hits = stats.dirty_hits.saturating_add(1);
            } else {
                stats.dirty_misses = stats.dirty_misses.saturating_add(1);
            }
            stats.checksum = stats.checksum.wrapping_add(read_ref.token);
            stats.read_ref_bytes = stats
                .read_ref_bytes
                .saturating_add(config.record_bytes as u64);
            forwarder.forward(read_ref, config.record_bytes, stats);
        }
    }
    stats.reads = stats.reads.saturating_add(1);
}

fn print_config(config: &Config) {
    let wal_bytes_per_lane = if config.mode.needs_wal() && config.write_access.copies_payload() {
        config.records_per_lane.saturating_mul(config.record_bytes) as u64
    } else {
        0
    };
    let block_bytes_per_lane = if config.mode.needs_blockstore() {
        config
            .block_records_per_lane
            .saturating_mul(config.record_bytes) as u64
    } else {
        0
    };
    println!(
        "zcwal-reduce-config: mode={} pattern={} read_access={} write_access={} lanes={} workers={} records_per_lane={} record_bytes={} extent_records={} block_records_per_lane={} read_pct={} read_repeats={} reduce_every_extents={} forward_window={} pin={} thp={} hugetlb={} verify={} bulk_extents={} wal_arena_per_lane={} blockstore_per_lane={} timing_excludes_allocation_first_touch=true",
        config.mode,
        config.pattern,
        config.read_access,
        config.write_access,
        config.lanes,
        config.workers,
        config.records_per_lane,
        config.record_bytes,
        config.extent_records,
        config.block_records_per_lane,
        config.read_pct,
        config.read_repeats,
        config.reduce_every_extents,
        config.forward_window,
        config.pin,
        config.thp,
        config.hugetlb,
        config.verify,
        config.bulk_extents,
        fmt_bytes(wal_bytes_per_lane),
        fmt_bytes(block_bytes_per_lane)
    );
}

fn print_stats(stats: &[WorkerStats]) {
    for stat in stats {
        let secs = stat.wall.as_secs_f64().max(f64::MIN_POSITIVE);
        let cpu_secs = stat.cpu.as_secs_f64();
        let forward_records_per_event = if stat.forward_events == 0 {
            0.0
        } else {
            stat.reads as f64 / stat.forward_events as f64
        };
        println!(
            "zcwal-reduce-worker: worker={} lanes={} ops={} writes={} reads={} dirty_hits={} dirty_misses={} wal_extents={} reduced_extents={} max_extent_records={} contiguous_reduce_extents={} scattered_reduce_records={} forward_events={} forward_completions={} forward_max_inflight={} forward_events_per_sec={:.0} forward_records_per_event={:.1} seconds={secs:.6} logical_iops={:.0} wal_Gbitps={:.3} wal_ref_Gbitps={:.3} reduce_Gbitps={:.3} read_Gbitps={:.3} read_ref_Gbitps={:.3} forward_ref_Gbitps={:.3} thread_cpu_seconds={cpu_secs:.6} cpu_wall_pct={:.1} target_cpu={} affinity_applied={} start_cpu={} end_cpu={} voluntary_ctxt_switches={} involuntary_ctxt_switches={} migrations={} checksum={}",
            stat.worker,
            fmt_lanes(&stat.lanes),
            stat.ops,
            stat.writes,
            stat.reads,
            stat.dirty_hits,
            stat.dirty_misses,
            stat.wal_extents,
            stat.reduced_extents,
            stat.max_extent_records,
            stat.contiguous_reduce_extents,
            stat.scattered_reduce_records,
            stat.forward_events,
            stat.forward_completions,
            stat.forward_max_inflight,
            stat.forward_events as f64 / secs,
            forward_records_per_event,
            stat.ops as f64 / secs,
            stat.wal_bytes as f64 * 8.0 / 1_000_000_000.0 / secs,
            stat.wal_ref_bytes as f64 * 8.0 / 1_000_000_000.0 / secs,
            stat.reduce_bytes as f64 * 8.0 / 1_000_000_000.0 / secs,
            stat.read_bytes as f64 * 8.0 / 1_000_000_000.0 / secs,
            stat.read_ref_bytes as f64 * 8.0 / 1_000_000_000.0 / secs,
            stat.forward_ref_bytes as f64 * 8.0 / 1_000_000_000.0 / secs,
            cpu_secs / secs * 100.0,
            stat.target_cpu,
            stat.affinity_applied,
            stat.start_cpu,
            stat.end_cpu,
            stat.voluntary_switches,
            stat.involuntary_switches,
            stat.migrations,
            stat.checksum
        );
    }

    let wall = stats
        .iter()
        .map(|stat| stat.wall)
        .max()
        .unwrap_or_default()
        .as_secs_f64()
        .max(f64::MIN_POSITIVE);
    let total = stats
        .iter()
        .fold(WorkerStats::default(), |mut total, stat| {
            total.ops = total.ops.saturating_add(stat.ops);
            total.writes = total.writes.saturating_add(stat.writes);
            total.reads = total.reads.saturating_add(stat.reads);
            total.dirty_hits = total.dirty_hits.saturating_add(stat.dirty_hits);
            total.dirty_misses = total.dirty_misses.saturating_add(stat.dirty_misses);
            total.wal_bytes = total.wal_bytes.saturating_add(stat.wal_bytes);
            total.wal_ref_bytes = total.wal_ref_bytes.saturating_add(stat.wal_ref_bytes);
            total.reduce_bytes = total.reduce_bytes.saturating_add(stat.reduce_bytes);
            total.read_bytes = total.read_bytes.saturating_add(stat.read_bytes);
            total.read_ref_bytes = total.read_ref_bytes.saturating_add(stat.read_ref_bytes);
            total.forward_ref_bytes = total
                .forward_ref_bytes
                .saturating_add(stat.forward_ref_bytes);
            total.forward_events = total.forward_events.saturating_add(stat.forward_events);
            total.forward_completions = total
                .forward_completions
                .saturating_add(stat.forward_completions);
            total.forward_max_inflight = total.forward_max_inflight.max(stat.forward_max_inflight);
            total.wal_extents = total.wal_extents.saturating_add(stat.wal_extents);
            total.reduced_extents = total.reduced_extents.saturating_add(stat.reduced_extents);
            total.contiguous_reduce_extents = total
                .contiguous_reduce_extents
                .saturating_add(stat.contiguous_reduce_extents);
            total.scattered_reduce_records = total
                .scattered_reduce_records
                .saturating_add(stat.scattered_reduce_records);
            total.max_extent_records = total.max_extent_records.max(stat.max_extent_records);
            total.checksum = total.checksum.wrapping_add(stat.checksum);
            total.voluntary_switches = total
                .voluntary_switches
                .saturating_add(stat.voluntary_switches);
            total.involuntary_switches = total
                .involuntary_switches
                .saturating_add(stat.involuntary_switches);
            total.migrations = total.migrations.saturating_add(stat.migrations);
            total.cpu = total.cpu.saturating_add(stat.cpu);
            total
        });
    let touched_bytes = total
        .wal_bytes
        .saturating_add(total.reduce_bytes)
        .saturating_add(total.read_bytes);
    let forward_records_per_event = if total.forward_events == 0 {
        0.0
    } else {
        total.reads as f64 / total.forward_events as f64
    };
    println!(
        "zcwal-reduce-summary: workers={} ops={} writes={} reads={} dirty_hits={} dirty_misses={} wal_extents={} reduced_extents={} max_extent_records={} contiguous_reduce_extents={} scattered_reduce_records={} forward_events={} forward_completions={} forward_max_inflight={} forward_events_per_sec={:.0} forward_records_per_event={:.1} seconds={wall:.6} logical_iops={:.0} wal_Gbitps={:.3} wal_ref_Gbitps={:.3} reduce_Gbitps={:.3} read_Gbitps={:.3} read_ref_Gbitps={:.3} forward_ref_Gbitps={:.3} touched_Gbitps={:.3} thread_cpu_seconds={:.6} voluntary_ctxt_switches={} involuntary_ctxt_switches={} migrations={} checksum={}",
        stats.len(),
        total.ops,
        total.writes,
        total.reads,
        total.dirty_hits,
        total.dirty_misses,
        total.wal_extents,
        total.reduced_extents,
        total.max_extent_records,
        total.contiguous_reduce_extents,
        total.scattered_reduce_records,
        total.forward_events,
        total.forward_completions,
        total.forward_max_inflight,
        total.forward_events as f64 / wall,
        forward_records_per_event,
        total.ops as f64 / wall,
        total.wal_bytes as f64 * 8.0 / 1_000_000_000.0 / wall,
        total.wal_ref_bytes as f64 * 8.0 / 1_000_000_000.0 / wall,
        total.reduce_bytes as f64 * 8.0 / 1_000_000_000.0 / wall,
        total.read_bytes as f64 * 8.0 / 1_000_000_000.0 / wall,
        total.read_ref_bytes as f64 * 8.0 / 1_000_000_000.0 / wall,
        total.forward_ref_bytes as f64 * 8.0 / 1_000_000_000.0 / wall,
        touched_bytes as f64 * 8.0 / 1_000_000_000.0 / wall,
        total.cpu.as_secs_f64(),
        total.voluntary_switches,
        total.involuntary_switches,
        total.migrations,
        total.checksum
    );
}

fn validate_config(config: &Config) -> io::Result<()> {
    if config.lanes == 0 {
        return Err(invalid_input("--lanes must be nonzero"));
    }
    if config.workers == 0 {
        return Err(invalid_input("--workers must be nonzero"));
    }
    if config.records_per_lane == 0 {
        return Err(invalid_input("--records-per-lane must be nonzero"));
    }
    if config.record_bytes < 16 {
        return Err(invalid_input("--record-bytes must be at least 16"));
    }
    if config.extent_records == 0 {
        return Err(invalid_input("--extent-records must be nonzero"));
    }
    if config.block_records_per_lane == 0 {
        return Err(invalid_input("--block-records-per-lane must be nonzero"));
    }
    if config.mode == Mode::HotRead && config.block_records_per_lane > config.records_per_lane {
        return Err(invalid_input(
            "hot-read mode uses --records-per-lane as WAL/cache capacity; set --records-per-lane >= --block-records-per-lane",
        ));
    }
    if config.read_pct > 100 {
        return Err(invalid_input("--read-pct must be between 0 and 100"));
    }
    if config.read_repeats == 0 {
        return Err(invalid_input("--read-repeats must be nonzero"));
    }
    if config.forward_window == 0 {
        return Err(invalid_input("--forward-window must be nonzero"));
    }
    if config.read_access == ReadAccess::ForwardExtent {
        if !matches!(config.mode, Mode::Read | Mode::HotRead) {
            return Err(invalid_input(
                "read-access=forward-extent currently requires --mode read or --mode hot-read",
            ));
        }
        if config.pattern != Pattern::Sequential {
            return Err(invalid_input(
                "read-access=forward-extent currently requires --pattern seq so each descriptor represents a contiguous result-log extent",
            ));
        }
    }
    if config.pin && config.cpu_list.is_empty() {
        eprintln!(
            "PERF WARNING: --pin requested without --cpu-list/URING_PLAY_PIN_CPU_LIST; workers will pin to worker index CPUs"
        );
    }
    Ok(())
}

fn assign_lanes(lanes: usize, workers: usize) -> Vec<Vec<usize>> {
    let mut out = vec![Vec::new(); workers];
    for lane in 0..lanes {
        out[lane % workers].push(lane);
    }
    out
}

fn maybe_pin_worker(config: &Config, worker: usize) -> Affinity {
    let Some(cpu) = target_cpu_for_worker(config, worker) else {
        return Affinity {
            target_cpu: -1,
            applied: false,
        };
    };
    match set_current_thread_affinity(cpu) {
        Ok(()) => {
            println!("zcwal-reduce-affinity: worker={worker} target_cpu={cpu} status=ok");
            Affinity {
                target_cpu: cpu as i32,
                applied: true,
            }
        }
        Err(err) => {
            eprintln!(
                "PERF WARNING: zcwal-reduce-affinity worker={worker} target_cpu={cpu} status=error error={err}"
            );
            Affinity {
                target_cpu: cpu as i32,
                applied: false,
            }
        }
    }
}

fn target_cpu_for_worker(config: &Config, worker: usize) -> Option<usize> {
    if !config.pin {
        return None;
    }
    if config.cpu_list.is_empty() {
        Some(worker)
    } else {
        Some(config.cpu_list[worker % config.cpu_list.len()])
    }
}

fn set_current_thread_affinity(cpu: usize) -> io::Result<()> {
    if cpu >= libc::CPU_SETSIZE as usize {
        return Err(invalid_input(format!("cpu {cpu} exceeds CPU_SETSIZE")));
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

fn current_tid() -> i64 {
    unsafe { libc::syscall(libc::SYS_gettid) as i64 }
}

fn current_cpu() -> i32 {
    unsafe { libc::sched_getcpu() }
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

fn read_thread_context_switches(tid: i64) -> io::Result<ThreadSwitches> {
    let status = fs::read_to_string(format!("/proc/self/task/{tid}/status"))?;
    let mut switches = ThreadSwitches::default();
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

fn logical_record(config: &Config, lane: usize, op: usize) -> usize {
    match config.pattern {
        Pattern::Sequential => op % config.block_records_per_lane,
        Pattern::Random => {
            mix64(((lane as u64) << 32) ^ op as u64) as usize % config.block_records_per_lane
        }
    }
}

fn mixed_is_read(lane: usize, op: usize, read_pct: u32) -> bool {
    (mix64(((lane as u64) << 40) ^ (op as u64).wrapping_mul(0x9e37_79b9)) % 100) < read_pct as u64
}

fn mix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn checksum_record(data: &[u8]) -> u64 {
    if data.len() < 16 {
        return 0;
    }
    let first = unsafe { ptr::read_unaligned(data.as_ptr() as *const u64) };
    let last = unsafe {
        ptr::read_unaligned(data.as_ptr().add(data.len() - std::mem::size_of::<u64>()) as *const u64)
    };
    first.wrapping_add(last)
}

fn make_source_pattern(worker: usize, record_bytes: usize) -> Vec<u8> {
    let mut source = vec![0u8; record_bytes];
    for (index, byte) in source.iter_mut().enumerate() {
        *byte = mix64(((worker as u64) << 32) ^ index as u64) as u8;
    }
    source
}

fn make_source_extent(worker: usize, record_bytes: usize, extent_records: usize) -> Vec<u8> {
    let source = make_source_pattern(worker, record_bytes);
    let mut extent = vec![0u8; record_bytes.saturating_mul(extent_records)];
    for record in 0..extent_records {
        let offset = record.saturating_mul(record_bytes);
        extent[offset..offset + record_bytes].copy_from_slice(&source);
    }
    extent
}

fn parse_count(value: &str) -> io::Result<usize> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid_input("empty numeric value"));
    }
    let (digits, scale) = match value.as_bytes()[value.len() - 1] as char {
        'k' | 'K' => (&value[..value.len() - 1], 1024usize),
        'm' | 'M' => (&value[..value.len() - 1], 1024usize * 1024),
        'g' | 'G' => (&value[..value.len() - 1], 1024usize * 1024 * 1024),
        _ => (value, 1usize),
    };
    let base = digits
        .parse::<usize>()
        .map_err(|err| invalid_input(format!("invalid numeric value {value:?}: {err}")))?;
    base.checked_mul(scale)
        .ok_or_else(|| invalid_input(format!("numeric value {value:?} overflows usize")))
}

fn parse_count_env(name: &str, default: usize) -> io::Result<usize> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => parse_count(value.trim()),
        _ => Ok(default),
    }
}

fn parse_cpu_list_env() -> Vec<usize> {
    env::var("URING_PLAY_PIN_CPU_LIST")
        .ok()
        .and_then(|value| parse_cpu_list(&value).ok())
        .unwrap_or_default()
}

fn parse_cpu_list(value: &str) -> io::Result<Vec<usize>> {
    let mut out = Vec::new();
    for part in value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((first, last)) = part.split_once('-') {
            let first = first
                .parse::<usize>()
                .map_err(|err| invalid_input(format!("invalid CPU range {part:?}: {err}")))?;
            let last = last
                .parse::<usize>()
                .map_err(|err| invalid_input(format!("invalid CPU range {part:?}: {err}")))?;
            if first > last {
                return Err(invalid_input(format!(
                    "invalid descending CPU range {part:?}"
                )));
            }
            out.extend(first..=last);
        } else {
            out.push(
                part.parse::<usize>()
                    .map_err(|err| invalid_input(format!("invalid CPU {part:?}: {err}")))?,
            );
        }
    }
    Ok(out)
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> io::Result<String> {
    args.next()
        .ok_or_else(|| invalid_input(format!("{name} requires a value")))
}

fn env_truthy(name: &str) -> bool {
    env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
        .unwrap_or(false)
}

fn env_falsey(name: &str) -> bool {
    env::var(name)
        .map(|value| matches!(value.as_str(), "0" | "false" | "FALSE" | "no" | "off"))
        .unwrap_or(false)
}

fn fmt_lanes(lanes: &[usize]) -> String {
    lanes
        .iter()
        .map(|lane| lane.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn fmt_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.3}GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.3}MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.3}KiB", bytes_f / KIB)
    } else {
        format!("{bytes}B")
    }
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn print_usage() {
    println!(
        "usage: zcwal-reduce-bench [options]\n\
         \n\
         WAL-combination plus userspace reduce-blockstore microbench.\n\
         No block device is used as a mirror or stripe primitive.\n\
         \n\
         Options:\n\
          --mode combine|reduce|read|hot-read|mixed default: mixed\n\
           --pattern seq|random                  default: seq\n\
           --lanes N                             default: 8\n\
           --workers N                           default: 8\n\
           --records-per-lane N                  default: 65536\n\
           --record-bytes N                      default: 4096\n\
           --extent-records N                    default: 256\n\
          --block-records-per-lane N            default: 65536\n\
          --read-pct N                          mixed-mode read percentage, default: 50\n\
          --read-repeats N                      repeat read working set without growing cache, default: 1\n\
          --read-access copy|ref|forward-ref|forward-extent\n\
                                                copy materializes payload; ref returns slot tokens; forward-ref queues 4K descriptors; forward-extent queues one extent descriptor, default: copy\n\
           --forward-window N                    bounded downstream descriptor queue depth, default: 1024\n\
           --reduce-every-extents N              0 keeps dirty WAL until end, default: 0\n\
           --pin --cpu-list 0-7                  pin workers and first-touch arenas there\n\
           --thp|--no-thp                        madvise MADV_HUGEPAGE, default: on\n\
           --hugetlb|--no-hugetlb                request MAP_HUGETLB, default: off\n\
           --verify|--no-verify                  reserved correctness hook, default: off\n\
           --bulk-extents                        append full WAL extents with one payload copy, default: on\n\
           --per-record-ingest                   append each 4K record separately"
    );
}
