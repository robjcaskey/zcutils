use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const BLOCK: usize = 4096;
const SUPER_SLOTS: usize = 2;
const DATA_START: u64 = (BLOCK * SUPER_SLOTS) as u64;
const SUPER_MAGIC: &[u8; 8] = b"ZCPWALS1";
const FRAME_MAGIC: &[u8; 8] = b"ZCPWALF1";
const VERSION: u32 = 1;
const FRAME_FIXED: usize = 64;
const MAX_RECORDS: usize = (BLOCK - FRAME_FIXED) / 8;
const FLAG_PAYLOAD_CRC32C: u32 = 1;
const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
const FS_IOC_FIEMAP: libc::c_ulong = 0xc020_660b;
const FIEMAP_FLAG_SYNC: u32 = 1;
const FIEMAP_EXTENT_LAST: u32 = 1;
const FIEMAP_EXTENT_UNKNOWN: u32 = 2;
const FIEMAP_EXTENT_DELALLOC: u32 = 4;
const FIEMAP_BATCH: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileProvisioning {
    /// Reserve all required regular-file blocks with `fallocate` and verify
    /// their logical coverage with FIEMAP.
    Preallocate,
    /// Do not alter allocation; reject sparse, delayed-allocation, or unknown
    /// extents after checking FIEMAP.
    RequireAllocated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackingIoMode {
    /// Normal buffered I/O through the filesystem page cache.
    Buffered,
    /// Open both terminal backings with `O_DIRECT`. External buffers must be
    /// 4096-byte aligned; the WAL never hides a bounce copy on this path.
    Direct,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersistentWalOpenOptions {
    pub file_provisioning: FileProvisioning,
    pub io_mode: BackingIoMode,
}

impl Default for PersistentWalOpenOptions {
    fn default() -> Self {
        Self {
            file_provisioning: FileProvisioning::Preallocate,
            io_mode: BackingIoMode::Buffered,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackingKind {
    RegularFile,
    BlockDevice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocationEvidence {
    NotApplicable,
    Fiemap,
    AllocatedBlockCount,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackingInfo {
    pub kind: BackingKind,
    pub available_bytes: u64,
    pub required_bytes: u64,
    pub allocated_extents: u32,
    pub allocation_evidence: AllocationEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrityMode {
    Frame,
    Crc32c,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersistentWalStats {
    pub integrity: IntegrityMode,
    pub io_mode: BackingIoMode,
    pub generation: u64,
    pub appended_sequence: u64,
    pub durable_sequence: u64,
    pub reduced_sequence: u64,
    pub journal_used_bytes: u64,
    pub journal_capacity_bytes: u64,
    pub pending_frames: usize,
    pub journal_backing: BackingInfo,
    pub base_backing: BackingInfo,
}

#[derive(Clone, Debug)]
struct FrameRef {
    sequence: u64,
    frame_start: u64,
    frame_end: u64,
    payload_start: u64,
    logical_pages: Box<[u64]>,
}

#[derive(Debug)]
struct WalState {
    generation: u64,
    super_generation: u64,
    active_super_slot: usize,
    tail: u64,
    durable_tail: u64,
    durable_sequence: u64,
    reduced_tail: u64,
    reduced_sequence: u64,
    next_sequence: u64,
    frames: VecDeque<FrameRef>,
    retained_frames: VecDeque<FrameRef>,
    retention_pins: BTreeMap<u64, usize>,
}

pub struct PersistentWal {
    journal: File,
    base: File,
    logical_bytes: u64,
    journal_bytes: u64,
    integrity: IntegrityMode,
    io_mode: BackingIoMode,
    journal_backing: BackingInfo,
    base_backing: BackingInfo,
    latest_payload: Box<[AtomicU64]>,
    state: Mutex<WalState>,
}

enum ReduceCommand {
    Kick,
    Stop,
}

pub struct PersistentWalRuntime {
    wal: Arc<PersistentWal>,
    reduce_tx: SyncSender<ReduceCommand>,
    reduce_worker: Mutex<Option<JoinHandle<io::Result<()>>>>,
}

pub struct PersistentWalRetention {
    wal: Arc<PersistentWal>,
    start_sequence: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PersistentWalReplayStats {
    pub records_replayed: u64,
    pub bytes_replayed: u64,
}

impl PersistentWalRuntime {
    pub fn open(
        journal_path: impl AsRef<Path>,
        base_path: impl AsRef<Path>,
        logical_bytes: u64,
        journal_bytes: u64,
    ) -> io::Result<Self> {
        Self::open_with_integrity(
            journal_path,
            base_path,
            logical_bytes,
            journal_bytes,
            IntegrityMode::Crc32c,
        )
    }

    pub fn open_with_integrity(
        journal_path: impl AsRef<Path>,
        base_path: impl AsRef<Path>,
        logical_bytes: u64,
        journal_bytes: u64,
        integrity: IntegrityMode,
    ) -> io::Result<Self> {
        Self::open_with_options(
            journal_path,
            base_path,
            logical_bytes,
            journal_bytes,
            integrity,
            PersistentWalOpenOptions::default(),
        )
    }

    pub fn open_with_options(
        journal_path: impl AsRef<Path>,
        base_path: impl AsRef<Path>,
        logical_bytes: u64,
        journal_bytes: u64,
        integrity: IntegrityMode,
        options: PersistentWalOpenOptions,
    ) -> io::Result<Self> {
        let wal = Arc::new(PersistentWal::open_with_options(
            journal_path.as_ref(),
            base_path.as_ref(),
            logical_bytes,
            journal_bytes,
            integrity,
            options,
        )?);
        let (reduce_tx, reduce_rx) = mpsc::sync_channel(1);
        let worker_wal = Arc::clone(&wal);
        let reduce_worker = thread::Builder::new()
            .name("zcpwal-reduce".to_string())
            .spawn(move || {
                while let Ok(command) = reduce_rx.recv() {
                    match command {
                        ReduceCommand::Stop => return Ok(()),
                        ReduceCommand::Kick => {
                            while worker_wal.reduce(256)? != 0 {}
                            let _ = worker_wal.reset_if_drained()?;
                        }
                    }
                }
                Ok(())
            })?;
        Ok(Self {
            wal,
            reduce_tx,
            reduce_worker: Mutex::new(Some(reduce_worker)),
        })
    }

    pub fn append_contiguous(&self, offset: u64, payload: &[u8]) -> io::Result<u64> {
        self.wal.append_contiguous(offset, payload)
    }

    pub fn read_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.wal.read_at(offset, out)
    }

    pub fn sync(&self) -> io::Result<u64> {
        let hwm = self.wal.sync()?;
        match self.reduce_tx.try_send(ReduceCommand::Kick) {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(hwm),
            Err(TrySendError::Disconnected(_)) => Err(io::Error::other(
                "persistent WAL reducer stopped unexpectedly",
            )),
        }
    }

    pub fn stats(&self) -> PersistentWalStats {
        self.wal.stats()
    }

    pub fn pin_retained_tail(&self) -> PersistentWalRetention {
        PersistentWal::pin_retained_tail(&self.wal)
    }

    pub fn pin_retained_from(&self, start_sequence: u64) -> io::Result<PersistentWalRetention> {
        PersistentWal::pin_retained_from(&self.wal, start_sequence)
    }
}

impl Drop for PersistentWalRuntime {
    fn drop(&mut self) {
        let _ = self.reduce_tx.send(ReduceCommand::Stop);
        if let Some(worker) = self
            .reduce_worker
            .lock()
            .expect("persistent WAL reducer mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
    }
}

impl PersistentWal {
    pub fn pin_retained_tail(wal: &Arc<Self>) -> PersistentWalRetention {
        let mut state = wal.state.lock().expect("persistent WAL mutex poisoned");
        let start_sequence = state.next_sequence;
        *state.retention_pins.entry(start_sequence).or_insert(0) += 1;
        drop(state);
        PersistentWalRetention {
            wal: Arc::clone(wal),
            start_sequence,
        }
    }

    pub fn pin_retained_from(
        wal: &Arc<Self>,
        start_sequence: u64,
    ) -> io::Result<PersistentWalRetention> {
        let mut state = wal.state.lock().expect("persistent WAL mutex poisoned");
        if start_sequence == 0 || start_sequence > state.next_sequence {
            return Err(invalid("retained WAL pin sequence is outside the journal"));
        }
        let oldest_available = state
            .retained_frames
            .front()
            .or_else(|| state.frames.front())
            .map_or(state.next_sequence, |frame| frame.sequence);
        if start_sequence < oldest_available {
            return Err(invalid("requested retained WAL suffix has been reclaimed"));
        }
        *state.retention_pins.entry(start_sequence).or_insert(0) += 1;
        drop(state);
        Ok(PersistentWalRetention {
            wal: Arc::clone(wal),
            start_sequence,
        })
    }

    pub fn open(
        journal_path: impl AsRef<Path>,
        base_path: impl AsRef<Path>,
        logical_bytes: u64,
        journal_bytes: u64,
    ) -> io::Result<Self> {
        Self::open_with_integrity(
            journal_path,
            base_path,
            logical_bytes,
            journal_bytes,
            IntegrityMode::Crc32c,
        )
    }

    pub fn open_with_integrity(
        journal_path: impl AsRef<Path>,
        base_path: impl AsRef<Path>,
        logical_bytes: u64,
        journal_bytes: u64,
        integrity: IntegrityMode,
    ) -> io::Result<Self> {
        Self::open_with_options(
            journal_path,
            base_path,
            logical_bytes,
            journal_bytes,
            integrity,
            PersistentWalOpenOptions::default(),
        )
    }

    pub fn open_with_options(
        journal_path: impl AsRef<Path>,
        base_path: impl AsRef<Path>,
        logical_bytes: u64,
        journal_bytes: u64,
        integrity: IntegrityMode,
        options: PersistentWalOpenOptions,
    ) -> io::Result<Self> {
        if logical_bytes == 0 || logical_bytes % BLOCK as u64 != 0 {
            return Err(invalid("logical size must be a non-zero multiple of 4096"));
        }
        if journal_bytes < DATA_START + (BLOCK * 2) as u64 || journal_bytes % BLOCK as u64 != 0 {
            return Err(invalid(
                "journal size must be 4096-aligned and hold superblocks plus one frame",
            ));
        }
        let (journal, journal_backing) = open_backing(
            journal_path.as_ref(),
            journal_bytes,
            "journal",
            options.file_provisioning,
            options.io_mode,
        )?;
        let (base, base_backing) = open_backing(
            base_path.as_ref(),
            logical_bytes,
            "base",
            options.file_provisioning,
            options.io_mode,
        )?;
        reject_same_backing(&journal, &base)?;
        let pages = usize::try_from(logical_bytes / BLOCK as u64)
            .map_err(|_| invalid("logical page count exceeds usize"))?;
        let mut wal = Self {
            journal,
            base,
            logical_bytes,
            journal_bytes,
            integrity,
            io_mode: options.io_mode,
            journal_backing,
            base_backing,
            latest_payload: (0..pages)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            state: Mutex::new(WalState {
                generation: 1,
                super_generation: 0,
                active_super_slot: 0,
                tail: DATA_START,
                durable_tail: DATA_START,
                durable_sequence: 0,
                reduced_tail: DATA_START,
                reduced_sequence: 0,
                next_sequence: 1,
                frames: VecDeque::new(),
                retained_frames: VecDeque::new(),
                retention_pins: BTreeMap::new(),
            }),
        };
        wal.recover_or_initialize()?;
        Ok(wal)
    }

    pub fn append_contiguous(&self, logical_offset: u64, payload: &[u8]) -> io::Result<u64> {
        if payload.is_empty() || payload.len() % BLOCK != 0 || logical_offset % BLOCK as u64 != 0 {
            return Err(invalid("WAL append must contain aligned 4096-byte records"));
        }
        let records = payload.len() / BLOCK;
        if records > MAX_RECORDS {
            return Err(invalid("WAL append exceeds one extent header"));
        }
        let end = logical_offset
            .checked_add(payload.len() as u64)
            .ok_or_else(|| invalid("logical append range overflow"))?;
        if end > self.logical_bytes {
            return Err(invalid("logical append exceeds final image"));
        }
        let pages = (0..records)
            .map(|index| logical_offset / BLOCK as u64 + index as u64)
            .collect::<Vec<_>>();
        self.append_pages(&pages, payload)
    }

    pub fn append_pages(&self, logical_pages: &[u64], payload: &[u8]) -> io::Result<u64> {
        if logical_pages.is_empty()
            || logical_pages.len() > MAX_RECORDS
            || payload.len() != logical_pages.len() * BLOCK
        {
            return Err(invalid("WAL extent page table and payload shape disagree"));
        }
        if logical_pages
            .iter()
            .any(|page| *page >= self.latest_payload.len() as u64)
        {
            return Err(invalid("WAL extent references a logical page out of range"));
        }
        self.validate_external_buffer(payload.as_ptr(), payload.len(), "append payload")?;

        let payload_crc = match self.integrity {
            IntegrityMode::Frame => 0,
            IntegrityMode::Crc32c => crc32c_slices(payload.chunks(BLOCK)),
        };
        let mut state = self.state.lock().expect("persistent WAL mutex poisoned");
        let frame_start = state.tail;
        let payload_start = frame_start + BLOCK as u64;
        let frame_end = payload_start
            .checked_add(payload.len() as u64)
            .ok_or_else(|| invalid("WAL frame length overflow"))?;
        if frame_end > self.journal_bytes {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "persistent WAL is full; checkpoint must catch up before reset",
            ));
        }
        let sequence = state.next_sequence;
        let mut header = AlignedBlock::zeroed();
        encode_frame_header(
            &mut header.0,
            state.generation,
            sequence,
            frame_end - frame_start,
            logical_pages,
            payload_crc,
            self.integrity,
        );
        pwritev_all(&self.journal, frame_start, &header.0, payload)?;

        for (index, logical_page) in logical_pages.iter().copied().enumerate() {
            self.latest_payload[logical_page as usize].store(
                payload_start + (index * BLOCK) as u64 + 1,
                Ordering::Release,
            );
        }
        state.frames.push_back(FrameRef {
            sequence,
            frame_start,
            frame_end,
            payload_start,
            logical_pages: logical_pages.to_vec().into_boxed_slice(),
        });
        state.tail = frame_end;
        state.next_sequence = sequence.saturating_add(1);
        Ok(sequence)
    }

    pub fn sync(&self) -> io::Result<u64> {
        self.sync_with_payload_durable_hook(|| {})
    }

    /// Test/coordination hook at the only crash boundary between the payload
    /// drain and publication of the new durable tail. Production callers use
    /// [`Self::sync`], which supplies an empty hook.
    pub fn sync_with_payload_durable_hook<F>(&self, hook: F) -> io::Result<u64>
    where
        F: FnOnce(),
    {
        let mut state = self.state.lock().expect("persistent WAL mutex poisoned");
        // Phase 1 makes every frame through `tail` durable while the published
        // durable tail still points at the preceding prefix. Phase 2 below
        // writes and drains the alternate superblock. A crash can therefore
        // expose either the old prefix or a prefix whose payload is complete;
        // it cannot expose a commit record ahead of its payload.
        self.journal.sync_data()?;
        hook();
        state.durable_tail = state.tail;
        state.durable_sequence = state.next_sequence.saturating_sub(1);
        self.persist_super_locked(&mut state)?;
        Ok(state.durable_sequence)
    }

    pub fn read_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        if out.is_empty() || out.len() % BLOCK != 0 || offset % BLOCK as u64 != 0 {
            return Err(invalid("persistent WAL reads must be 4096 aligned"));
        }
        let end = offset
            .checked_add(out.len() as u64)
            .ok_or_else(|| invalid("read range overflow"))?;
        if end > self.logical_bytes {
            return Err(invalid("read exceeds final image"));
        }
        self.validate_external_buffer(out.as_ptr(), out.len(), "read destination")?;
        for (index, page_out) in out.chunks_mut(BLOCK).enumerate() {
            let logical_page = offset / BLOCK as u64 + index as u64;
            let encoded = self.latest_payload[logical_page as usize].load(Ordering::Acquire);
            if encoded == 0 {
                read_exact_at(&self.base, page_out, logical_page * BLOCK as u64)?;
            } else {
                read_exact_at(&self.journal, page_out, encoded - 1)?;
            }
        }
        Ok(())
    }

    pub fn reduce(&self, max_frames: usize) -> io::Result<usize> {
        if max_frames == 0 {
            return Ok(0);
        }
        let frames = {
            let state = self.state.lock().expect("persistent WAL mutex poisoned");
            state
                .frames
                .iter()
                .take_while(|frame| frame.frame_end <= state.durable_tail)
                .take(max_frames)
                .cloned()
                .collect::<Vec<_>>()
        };
        if frames.is_empty() {
            return Ok(0);
        }

        let mut page = AlignedBlock::zeroed();
        for frame in &frames {
            for (index, logical_page) in frame.logical_pages.iter().copied().enumerate() {
                let payload_offset = frame.payload_start + (index * BLOCK) as u64;
                read_exact_at(&self.journal, &mut page.0, payload_offset)?;
                write_all_at(&self.base, &page.0, logical_page * BLOCK as u64)?;
            }
        }
        self.base.sync_data()?;

        let mut state = self.state.lock().expect("persistent WAL mutex poisoned");
        let mut reduced = 0usize;
        for frame in frames {
            let Some(front) = state.frames.front() else {
                break;
            };
            if front.sequence != frame.sequence || front.frame_start != frame.frame_start {
                break;
            }
            for (index, logical_page) in frame.logical_pages.iter().copied().enumerate() {
                let encoded = frame.payload_start + (index * BLOCK) as u64 + 1;
                let _ = self.latest_payload[logical_page as usize].compare_exchange(
                    encoded,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            if state
                .retention_pins
                .first_key_value()
                .is_some_and(|(start, _)| frame.sequence >= *start)
            {
                state.retained_frames.push_back(frame.clone());
            }
            state.frames.pop_front();
            state.reduced_tail = frame.frame_end;
            state.reduced_sequence = frame.sequence;
            reduced += 1;
        }
        self.persist_super_locked(&mut state)?;
        Ok(reduced)
    }

    pub fn reset_if_drained(&self) -> io::Result<bool> {
        let mut state = self.state.lock().expect("persistent WAL mutex poisoned");
        if !state.frames.is_empty()
            || !state.retained_frames.is_empty()
            || !state.retention_pins.is_empty()
            || state.reduced_tail != state.tail
            || state.durable_tail != state.tail
        {
            return Ok(false);
        }
        state.generation = state.generation.saturating_add(1);
        state.tail = DATA_START;
        state.durable_tail = DATA_START;
        state.reduced_tail = DATA_START;
        state.durable_sequence = state.next_sequence.saturating_sub(1);
        state.reduced_sequence = state.durable_sequence;
        self.persist_super_locked(&mut state)?;
        Ok(true)
    }

    pub fn stats(&self) -> PersistentWalStats {
        let state = self.state.lock().expect("persistent WAL mutex poisoned");
        PersistentWalStats {
            integrity: self.integrity,
            io_mode: self.io_mode,
            generation: state.generation,
            appended_sequence: state.next_sequence.saturating_sub(1),
            durable_sequence: state.durable_sequence,
            reduced_sequence: state.reduced_sequence,
            journal_used_bytes: state.tail.saturating_sub(DATA_START),
            journal_capacity_bytes: self.journal_bytes.saturating_sub(DATA_START),
            pending_frames: state.frames.len(),
            journal_backing: self.journal_backing,
            base_backing: self.base_backing,
        }
    }

    fn recover_or_initialize(&mut self) -> io::Result<()> {
        let supers = [self.read_super(0)?, self.read_super(1)?];
        let selected = supers
            .into_iter()
            .enumerate()
            .filter_map(|(slot, value)| value.map(|value| (slot, value)))
            .max_by_key(|(_, value)| value.super_generation);
        let Some((slot, superblock)) = selected else {
            let mut state = self.state.lock().expect("persistent WAL mutex poisoned");
            self.persist_super_locked(&mut state)?;
            return Ok(());
        };
        if superblock.logical_bytes != self.logical_bytes
            || superblock.journal_bytes != self.journal_bytes
            || superblock.integrity != self.integrity
            || superblock.durable_tail < DATA_START
            || superblock.durable_tail > self.journal_bytes
            || superblock.reduced_tail < DATA_START
            || superblock.reduced_tail > superblock.durable_tail
        {
            return Err(invalid(
                "persistent WAL superblock geometry is incompatible",
            ));
        }

        let mut frames = VecDeque::new();
        let mut cursor = DATA_START;
        let mut max_sequence = 0u64;
        while cursor < superblock.durable_tail {
            let frame = self.read_frame(cursor, superblock.generation, superblock.durable_tail)?;
            max_sequence = max_sequence.max(frame.sequence);
            if frame.frame_end > superblock.reduced_tail {
                for (index, logical_page) in frame.logical_pages.iter().copied().enumerate() {
                    self.latest_payload[logical_page as usize].store(
                        frame.payload_start + (index * BLOCK) as u64 + 1,
                        Ordering::Relaxed,
                    );
                }
                frames.push_back(frame.clone());
            }
            cursor = frame.frame_end;
        }
        if cursor != superblock.durable_tail {
            return Err(invalid("persistent WAL durable tail splits a frame"));
        }
        *self.state.lock().expect("persistent WAL mutex poisoned") = WalState {
            generation: superblock.generation,
            super_generation: superblock.super_generation,
            active_super_slot: slot,
            tail: superblock.durable_tail,
            durable_tail: superblock.durable_tail,
            durable_sequence: superblock.durable_sequence,
            reduced_tail: superblock.reduced_tail,
            reduced_sequence: superblock.reduced_sequence,
            next_sequence: max_sequence
                .max(superblock.durable_sequence)
                .saturating_add(1),
            frames,
            retained_frames: VecDeque::new(),
            retention_pins: BTreeMap::new(),
        };
        Ok(())
    }

    fn read_frame(&self, start: u64, generation: u64, durable_tail: u64) -> io::Result<FrameRef> {
        let mut header = AlignedBlock::zeroed();
        read_exact_at(&self.journal, &mut header.0, start)?;
        let decoded = decode_frame_header(&header.0)?;
        if decoded.generation != generation
            || decoded.frame_bytes < (BLOCK * 2) as u64
            || decoded.frame_bytes % BLOCK as u64 != 0
        {
            return Err(invalid("invalid persistent WAL frame generation or length"));
        }
        let frame_end = start
            .checked_add(decoded.frame_bytes)
            .ok_or_else(|| invalid("persistent WAL frame end overflow"))?;
        if frame_end > durable_tail {
            return Err(invalid("persistent WAL frame crosses durable tail"));
        }
        let payload_start = start + BLOCK as u64;
        let mut payload = AlignedBlocks::zeroed(decoded.logical_pages.len());
        read_exact_at(&self.journal, payload.as_bytes_mut(), payload_start)?;
        if decoded.integrity == IntegrityMode::Crc32c
            && crc32c(payload.as_bytes()) != decoded.payload_crc
        {
            return Err(invalid("persistent WAL payload checksum mismatch"));
        }
        Ok(FrameRef {
            sequence: decoded.sequence,
            frame_start: start,
            frame_end,
            payload_start,
            logical_pages: decoded.logical_pages.into_boxed_slice(),
        })
    }

    fn read_super(&self, slot: usize) -> io::Result<Option<Superblock>> {
        let mut bytes = AlignedBlock::zeroed();
        read_exact_at(&self.journal, &mut bytes.0, (slot * BLOCK) as u64)?;
        decode_super(&bytes.0)
    }

    fn persist_super_locked(&self, state: &mut WalState) -> io::Result<()> {
        state.super_generation = state.super_generation.saturating_add(1);
        let slot = (state.active_super_slot + 1) % SUPER_SLOTS;
        // A retention pin is also a crash-recovery boundary. Even when the
        // reducer has applied these frames to `base`, the durable checkpoint
        // cannot move beyond the oldest pinned frame or recovery would no
        // longer be able to reconstruct that migration suffix.
        let (published_reduced_tail, published_reduced_sequence) = state
            .retained_frames
            .front()
            .map_or((state.reduced_tail, state.reduced_sequence), |frame| {
                (frame.frame_start, frame.sequence.saturating_sub(1))
            });
        let bytes = encode_super(Superblock {
            super_generation: state.super_generation,
            generation: state.generation,
            logical_bytes: self.logical_bytes,
            journal_bytes: self.journal_bytes,
            durable_tail: state.durable_tail,
            durable_sequence: state.durable_sequence,
            reduced_tail: published_reduced_tail,
            reduced_sequence: published_reduced_sequence,
            integrity: self.integrity,
        });
        write_all_at(&self.journal, &bytes.0, (slot * BLOCK) as u64)?;
        self.journal.sync_data()?;
        state.active_super_slot = slot;
        Ok(())
    }

    fn validate_external_buffer(
        &self,
        pointer: *const u8,
        length: usize,
        role: &str,
    ) -> io::Result<()> {
        if self.io_mode == BackingIoMode::Direct
            && ((pointer as usize) % BLOCK != 0 || length % BLOCK != 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "persistent WAL direct-I/O {role} must be 4096-byte aligned (address={pointer:p}, length={length}); no hidden bounce copy is permitted"
                ),
            ));
        }
        Ok(())
    }
}

impl PersistentWalRetention {
    pub fn start_sequence(&self) -> u64 {
        self.start_sequence
    }

    pub fn durable_hwm(&self) -> u64 {
        self.wal
            .state
            .lock()
            .expect("persistent WAL mutex poisoned")
            .durable_sequence
    }

    /// Replay the retained ordered suffix directly into a staged terminal.
    /// The caller fences new admissions, syncs the WAL, obtains `through`, and
    /// invokes this method before publishing the destination route.
    pub fn replay_into(
        &self,
        through: u64,
        destination: &File,
    ) -> io::Result<PersistentWalReplayStats> {
        self.replay_range_into(self.start_sequence, through, destination, 0)
    }

    /// Replay a retained subrange, coalescing repeated writes so only the
    /// latest payload for each logical page in that interval is materialized.
    /// This can run opportunistically before cutover; the fenced replay then
    /// needs only the suffix after the caller's last completed HWM.
    pub fn replay_range_into(
        &self,
        from_sequence: u64,
        through: u64,
        destination: &File,
        max_bytes_per_second: u64,
    ) -> io::Result<PersistentWalReplayStats> {
        if from_sequence < self.start_sequence {
            return Err(invalid("retained replay starts before its pin"));
        }
        let latest_pages = {
            let state = self
                .wal
                .state
                .lock()
                .expect("persistent WAL mutex poisoned");
            if through > state.durable_sequence {
                return Err(invalid("retained replay HWM is not durable"));
            }
            let mut latest = BTreeMap::new();
            for frame in state
                .retained_frames
                .iter()
                .chain(state.frames.iter())
                .filter(|frame| frame.sequence >= from_sequence && frame.sequence <= through)
            {
                for (index, logical_page) in frame.logical_pages.iter().copied().enumerate() {
                    latest.insert(logical_page, frame.payload_start + (index * BLOCK) as u64);
                }
            }
            latest
        };
        let mut page = AlignedBlock::zeroed();
        let mut records = 0u64;
        let mut bytes = 0u64;
        let started = Instant::now();
        for (logical_page, payload_offset) in latest_pages {
            read_exact_at(&self.wal.journal, &mut page.0, payload_offset)?;
            write_all_at(destination, &page.0, logical_page * BLOCK as u64)?;
            records += 1;
            bytes += BLOCK as u64;
            pace_retained_replay(started, bytes, max_bytes_per_second);
        }
        Ok(PersistentWalReplayStats {
            records_replayed: records,
            bytes_replayed: bytes,
        })
    }
}

fn pace_retained_replay(started: Instant, bytes: u64, max_bytes_per_second: u64) {
    if max_bytes_per_second == 0 {
        return;
    }
    let target = Duration::from_secs_f64(bytes as f64 / max_bytes_per_second as f64);
    if let Some(delay) = target.checked_sub(started.elapsed()) {
        thread::sleep(delay);
    }
}

impl Drop for PersistentWalRetention {
    fn drop(&mut self) {
        let mut state = self
            .wal
            .state
            .lock()
            .expect("persistent WAL mutex poisoned");
        if let Some(count) = state.retention_pins.get_mut(&self.start_sequence) {
            *count -= 1;
            if *count == 0 {
                state.retention_pins.remove(&self.start_sequence);
            }
        }
        let floor = state
            .retention_pins
            .first_key_value()
            .map(|(start, _)| *start);
        while state
            .retained_frames
            .front()
            .is_some_and(|frame| floor.is_none_or(|floor| frame.sequence < floor))
        {
            state.retained_frames.pop_front();
        }
    }
}

#[repr(C, align(4096))]
struct AlignedBlock([u8; BLOCK]);

impl AlignedBlock {
    fn zeroed() -> Self {
        Self([0; BLOCK])
    }
}

struct AlignedBlocks(Vec<AlignedBlock>);

impl AlignedBlocks {
    fn zeroed(blocks: usize) -> Self {
        Self((0..blocks).map(|_| AlignedBlock::zeroed()).collect())
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast::<u8>(), self.0.len() * BLOCK) }
    }

    fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(self.0.as_mut_ptr().cast::<u8>(), self.0.len() * BLOCK)
        }
    }
}

#[derive(Clone, Copy)]
struct Superblock {
    super_generation: u64,
    generation: u64,
    logical_bytes: u64,
    journal_bytes: u64,
    durable_tail: u64,
    durable_sequence: u64,
    reduced_tail: u64,
    reduced_sequence: u64,
    integrity: IntegrityMode,
}

struct DecodedFrame {
    generation: u64,
    sequence: u64,
    frame_bytes: u64,
    payload_crc: u32,
    integrity: IntegrityMode,
    logical_pages: Vec<u64>,
}

fn encode_super(value: Superblock) -> AlignedBlock {
    let mut aligned = AlignedBlock::zeroed();
    let out = &mut aligned.0;
    out[0..8].copy_from_slice(SUPER_MAGIC);
    put_u32(out, 8, VERSION);
    put_u64(out, 16, value.super_generation);
    put_u64(out, 24, value.generation);
    put_u64(out, 32, value.logical_bytes);
    put_u64(out, 40, value.journal_bytes);
    put_u64(out, 48, value.durable_tail);
    put_u64(out, 56, value.durable_sequence);
    put_u64(out, 64, value.reduced_tail);
    put_u64(out, 72, value.reduced_sequence);
    put_u32(
        out,
        80,
        u32::from(value.integrity == IntegrityMode::Crc32c) * FLAG_PAYLOAD_CRC32C,
    );
    let checksum = crc32c(&out[..BLOCK - 4]);
    put_u32(out, BLOCK - 4, checksum);
    aligned
}

fn decode_super(input: &[u8; BLOCK]) -> io::Result<Option<Superblock>> {
    if input.iter().all(|byte| *byte == 0) {
        return Ok(None);
    }
    if &input[0..8] != SUPER_MAGIC || get_u32(input, 8) != VERSION {
        return Ok(None);
    }
    if crc32c(&input[..BLOCK - 4]) != get_u32(input, BLOCK - 4) {
        return Ok(None);
    }
    Ok(Some(Superblock {
        super_generation: get_u64(input, 16),
        generation: get_u64(input, 24),
        logical_bytes: get_u64(input, 32),
        journal_bytes: get_u64(input, 40),
        durable_tail: get_u64(input, 48),
        durable_sequence: get_u64(input, 56),
        reduced_tail: get_u64(input, 64),
        reduced_sequence: get_u64(input, 72),
        integrity: decode_integrity(get_u32(input, 80))?,
    }))
}

fn encode_frame_header(
    out: &mut [u8; BLOCK],
    generation: u64,
    sequence: u64,
    frame_bytes: u64,
    logical_pages: &[u64],
    payload_crc: u32,
    integrity: IntegrityMode,
) {
    out[0..8].copy_from_slice(FRAME_MAGIC);
    put_u32(out, 8, VERSION);
    put_u32(out, 12, logical_pages.len() as u32);
    put_u64(out, 16, generation);
    put_u64(out, 24, sequence);
    put_u64(out, 32, frame_bytes);
    put_u32(out, 40, payload_crc);
    put_u32(
        out,
        44,
        u32::from(integrity == IntegrityMode::Crc32c) * FLAG_PAYLOAD_CRC32C,
    );
    for (index, page) in logical_pages.iter().copied().enumerate() {
        put_u64(out, FRAME_FIXED + index * 8, page);
    }
    let checksum = crc32c(&out[..BLOCK - 4]);
    put_u32(out, BLOCK - 4, checksum);
}

fn decode_frame_header(input: &[u8; BLOCK]) -> io::Result<DecodedFrame> {
    if &input[0..8] != FRAME_MAGIC || get_u32(input, 8) != VERSION {
        return Err(invalid(
            "persistent WAL frame header magic/version mismatch",
        ));
    }
    if crc32c(&input[..BLOCK - 4]) != get_u32(input, BLOCK - 4) {
        return Err(invalid("persistent WAL frame header checksum mismatch"));
    }
    let records = get_u32(input, 12) as usize;
    if records == 0 || records > MAX_RECORDS {
        return Err(invalid("persistent WAL frame record count is invalid"));
    }
    let logical_pages = (0..records)
        .map(|index| get_u64(input, FRAME_FIXED + index * 8))
        .collect::<Vec<_>>();
    Ok(DecodedFrame {
        generation: get_u64(input, 16),
        sequence: get_u64(input, 24),
        frame_bytes: get_u64(input, 32),
        payload_crc: get_u32(input, 40),
        integrity: decode_integrity(get_u32(input, 44))?,
        logical_pages,
    })
}

fn decode_integrity(flags: u32) -> io::Result<IntegrityMode> {
    if flags & !FLAG_PAYLOAD_CRC32C != 0 {
        return Err(invalid("persistent WAL contains unknown integrity flags"));
    }
    Ok(if flags & FLAG_PAYLOAD_CRC32C != 0 {
        IntegrityMode::Crc32c
    } else {
        IntegrityMode::Frame
    })
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FiemapHeader {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FiemapExtent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct FiemapRequest {
    header: FiemapHeader,
    extents: [FiemapExtent; FIEMAP_BATCH],
}

fn open_backing(
    path: &Path,
    required_bytes: u64,
    role: &str,
    file_provisioning: FileProvisioning,
    io_mode: BackingIoMode,
) -> io::Result<(File, BackingInfo)> {
    let mut open_options = OpenOptions::new();
    open_options.read(true).write(true);
    if io_mode == BackingIoMode::Direct {
        open_options.custom_flags(libc::O_DIRECT);
    }
    let file = match open_options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            open_options.create(true).open(path)?
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    let file_type = metadata.mode() & libc::S_IFMT;
    match file_type {
        libc::S_IFREG => {
            if metadata.len() < required_bytes {
                file.set_len(required_bytes).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "cannot size persistent WAL {role} regular file {} to {required_bytes} bytes: {error}",
                            path.display()
                        ),
                    )
                })?;
            }
            if file_provisioning == FileProvisioning::Preallocate {
                let length = libc::off_t::try_from(required_bytes)
                    .map_err(|_| invalid("persistent WAL allocation length exceeds off_t"))?;
                let result = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, length) };
                if result != 0 {
                    let error = io::Error::last_os_error();
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "cannot preallocate persistent WAL {role} file {} through {required_bytes} bytes: {error}",
                            path.display()
                        ),
                    ));
                }
            }
            let (allocated_extents, allocation_evidence) =
                verify_regular_file_allocation(&file, required_bytes).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "persistent WAL {role} file {} is not fully allocated: {error}",
                            path.display()
                        ),
                    )
                })?;
            let available_bytes = file.metadata()?.len();
            Ok((
                file,
                BackingInfo {
                    kind: BackingKind::RegularFile,
                    available_bytes,
                    required_bytes,
                    allocated_extents,
                    allocation_evidence,
                },
            ))
        }
        libc::S_IFBLK => {
            let mut available_bytes = 0u64;
            let result = unsafe {
                libc::ioctl(
                    file.as_raw_fd(),
                    BLKGETSIZE64,
                    (&mut available_bytes as *mut u64).cast::<libc::c_void>(),
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            if available_bytes < required_bytes {
                return Err(invalid(format!(
                    "persistent WAL {role} block device {} has {available_bytes} bytes, needs {required_bytes}",
                    path.display()
                )));
            }
            Ok((
                file,
                BackingInfo {
                    kind: BackingKind::BlockDevice,
                    available_bytes,
                    required_bytes,
                    allocated_extents: 0,
                    allocation_evidence: AllocationEvidence::NotApplicable,
                },
            ))
        }
        _ => Err(invalid(format!(
            "persistent WAL {role} backing {} must be a regular file or terminal block device",
            path.display()
        ))),
    }
}

fn reject_same_backing(journal: &File, base: &File) -> io::Result<()> {
    let journal_meta = journal.metadata()?;
    let base_meta = base.metadata()?;
    let journal_type = journal_meta.mode() & libc::S_IFMT;
    let base_type = base_meta.mode() & libc::S_IFMT;
    let aliases = if journal_type == libc::S_IFBLK && base_type == libc::S_IFBLK {
        journal_meta.rdev() == base_meta.rdev()
    } else {
        journal_meta.dev() == base_meta.dev() && journal_meta.ino() == base_meta.ino()
    };
    if aliases {
        return Err(invalid(
            "persistent WAL journal and base must use distinct backing stores",
        ));
    }
    Ok(())
}

fn verify_fiemap(file: &File, required_bytes: u64) -> io::Result<u32> {
    let mut cursor = 0u64;
    let mut total_extents = 0u32;
    while cursor < required_bytes {
        let mut request: FiemapRequest = unsafe { mem::zeroed() };
        request.header.start = cursor;
        request.header.length = required_bytes - cursor;
        request.header.flags = FIEMAP_FLAG_SYNC;
        request.header.extent_count = FIEMAP_BATCH as u32;
        let result = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                FS_IOC_FIEMAP,
                (&mut request as *mut FiemapRequest).cast::<libc::c_void>(),
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        let mapped = request.header.mapped_extents as usize;
        if mapped == 0 {
            return Err(invalid(format!("sparse hole begins at byte {cursor}")));
        }
        if mapped > FIEMAP_BATCH {
            return Err(invalid("FIEMAP returned more extents than requested"));
        }
        for extent in &request.extents[..mapped] {
            if extent.logical != cursor {
                return Err(invalid(format!(
                    "sparse hole covers bytes {cursor}..{}",
                    extent.logical
                )));
            }
            if extent.length == 0
                || extent.flags & (FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DELALLOC) != 0
            {
                return Err(invalid(format!(
                    "unusable FIEMAP extent at byte {} length={} flags={:#x}",
                    extent.logical, extent.length, extent.flags
                )));
            }
            cursor = extent
                .logical
                .checked_add(extent.length)
                .ok_or_else(|| invalid("FIEMAP extent end overflow"))?
                .min(required_bytes);
            total_extents = total_extents
                .checked_add(1)
                .ok_or_else(|| invalid("FIEMAP extent count overflow"))?;
            if cursor == required_bytes || extent.flags & FIEMAP_EXTENT_LAST != 0 {
                break;
            }
        }
        if cursor < required_bytes && request.extents[mapped - 1].flags & FIEMAP_EXTENT_LAST != 0 {
            return Err(invalid(format!("sparse hole begins at byte {cursor}")));
        }
    }
    Ok(total_extents)
}

fn verify_regular_file_allocation(
    file: &File,
    required_bytes: u64,
) -> io::Result<(u32, AllocationEvidence)> {
    match verify_fiemap(file, required_bytes) {
        Ok(extents) => Ok((extents, AllocationEvidence::Fiemap)),
        Err(error)
            if error.raw_os_error().is_some_and(|code| {
                code == libc::EOPNOTSUPP || code == libc::ENOTTY || code == libc::ENOSYS
            }) =>
        {
            let metadata = file.metadata()?;
            let allocated_bytes = metadata.blocks().saturating_mul(512);
            if allocated_bytes < required_bytes {
                return Err(invalid(format!(
                    "filesystem lacks FIEMAP and allocated block count {allocated_bytes} is below required size {required_bytes}"
                )));
            }
            Ok((0, AllocationEvidence::AllocatedBlockCount))
        }
        Err(error) => Err(error),
    }
}

fn pwritev_all(file: &File, offset: u64, header: &[u8], payload: &[u8]) -> io::Result<()> {
    let iovecs = [
        libc::iovec {
            iov_base: header.as_ptr().cast_mut().cast(),
            iov_len: header.len(),
        },
        libc::iovec {
            iov_base: payload.as_ptr().cast_mut().cast(),
            iov_len: payload.len(),
        },
    ];
    let written = unsafe {
        libc::pwritev(
            std::os::fd::AsRawFd::as_raw_fd(file),
            iovecs.as_ptr(),
            iovecs.len() as i32,
            offset as libc::off_t,
        )
    };
    if written < 0 {
        return Err(io::Error::last_os_error());
    }
    let expected = header.len() + payload.len();
    if written as usize != expected {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("short persistent WAL pwritev: {written} of {expected}"),
        ));
    }
    Ok(())
}

fn read_exact_at(file: &File, mut out: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !out.is_empty() {
        let read = file.read_at(out, offset)?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short pread"));
        }
        offset += read as u64;
        out = &mut out[read..];
    }
    Ok(())
}

fn write_all_at(file: &File, mut input: &[u8], mut offset: u64) -> io::Result<()> {
    while !input.is_empty() {
        let written = file.write_at(input, offset)?;
        if written == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short pwrite"));
        }
        offset += written as u64;
        input = &input[written..];
    }
    Ok(())
}

fn crc32c(input: &[u8]) -> u32 {
    crc32c_slices(std::iter::once(input))
}

fn crc32c_slices<'a>(slices: impl IntoIterator<Item = &'a [u8]>) -> u32 {
    let slices = slices.into_iter();
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("sse4.2") {
        return unsafe { crc32c_x86(slices) };
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("crc") {
        return unsafe { crc32c_aarch64(slices) };
    }
    crc32c_software(slices)
}

fn crc32c_software<'a>(slices: impl IntoIterator<Item = &'a [u8]>) -> u32 {
    let mut crc = !0u32;
    for input in slices {
        for byte in input {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0x82f63b78u32 & (0u32.wrapping_sub(crc & 1)));
            }
        }
    }
    !crc
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_x86<'a>(slices: impl IntoIterator<Item = &'a [u8]>) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let mut crc = !0u64;
    for mut input in slices {
        while input.len() >= 8 {
            let word = u64::from_le_bytes(input[..8].try_into().expect("eight byte CRC word"));
            crc = _mm_crc32_u64(crc, word);
            input = &input[8..];
        }
        for byte in input {
            crc = u64::from(_mm_crc32_u8(crc as u32, *byte));
        }
    }
    !(crc as u32)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
unsafe fn crc32c_aarch64<'a>(slices: impl IntoIterator<Item = &'a [u8]>) -> u32 {
    use std::arch::aarch64::{__crc32cb, __crc32cd};
    let mut crc = !0u32;
    for mut input in slices {
        while input.len() >= 8 {
            let word = u64::from_le_bytes(input[..8].try_into().expect("eight byte CRC word"));
            crc = __crc32cd(crc, word);
            input = &input[8..];
        }
        for byte in input {
            crc = __crc32cb(crc, *byte);
        }
    }
    !crc
}

fn put_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u32(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(input[offset..offset + 4].try_into().expect("u32 field"))
}

fn get_u64(input: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(input[offset..offset + 8].try_into().expect("u64 field"))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn paths(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("zc-pwal-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        (root.join("journal"), root.join("base"))
    }

    #[test]
    fn append_sync_recover_read_reduce_and_reset() {
        let (journal, base) = paths("lifecycle");
        let first = vec![0x11; BLOCK * 2];
        let second = vec![0x22; BLOCK];
        {
            let wal = PersistentWal::open(&journal, &base, (BLOCK * 8) as u64, (BLOCK * 32) as u64)
                .unwrap();
            wal.append_contiguous(BLOCK as u64, &first).unwrap();
            wal.append_contiguous((BLOCK * 2) as u64, &second).unwrap();
            wal.sync().unwrap();
            let mut out = vec![0u8; BLOCK * 2];
            wal.read_at(BLOCK as u64, &mut out).unwrap();
            assert_eq!(&out[..BLOCK], &first[..BLOCK]);
            assert_eq!(&out[BLOCK..], &second);
        }
        {
            let wal = PersistentWal::open(&journal, &base, (BLOCK * 8) as u64, (BLOCK * 32) as u64)
                .unwrap();
            let mut out = vec![0u8; BLOCK * 2];
            wal.read_at(BLOCK as u64, &mut out).unwrap();
            assert_eq!(out[0], 0x11);
            assert_eq!(out[BLOCK], 0x22);
            assert_eq!(wal.reduce(32).unwrap(), 2);
            assert!(wal.reset_if_drained().unwrap());
            assert_eq!(wal.stats().journal_used_bytes, 0);
        }
        let _ = fs::remove_dir_all(journal.parent().unwrap());
    }

    #[test]
    fn direct_io_lifecycle_uses_aligned_caller_buffers_without_bounce_copy() {
        let (journal, base) = paths("direct-lifecycle");
        let options = PersistentWalOpenOptions {
            io_mode: BackingIoMode::Direct,
            ..PersistentWalOpenOptions::default()
        };
        {
            let wal = PersistentWal::open_with_options(
                &journal,
                &base,
                (BLOCK * 8) as u64,
                (BLOCK * 32) as u64,
                IntegrityMode::Crc32c,
                options,
            )
            .unwrap();
            let mut payload = AlignedBlocks::zeroed(2);
            payload.as_bytes_mut()[..BLOCK].fill(0x31);
            payload.as_bytes_mut()[BLOCK..].fill(0x42);
            wal.append_contiguous(BLOCK as u64, payload.as_bytes())
                .unwrap();
            wal.sync().unwrap();
            assert_eq!(wal.stats().io_mode, BackingIoMode::Direct);

            let mut out = AlignedBlocks::zeroed(2);
            wal.read_at(BLOCK as u64, out.as_bytes_mut()).unwrap();
            assert_eq!(out.as_bytes()[0], 0x31);
            assert_eq!(out.as_bytes()[BLOCK], 0x42);

            let unaligned = vec![0x55; BLOCK];
            let error = wal.append_contiguous(0, &unaligned).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("no hidden bounce copy"));
        }
        {
            let wal = PersistentWal::open_with_options(
                &journal,
                &base,
                (BLOCK * 8) as u64,
                (BLOCK * 32) as u64,
                IntegrityMode::Crc32c,
                options,
            )
            .unwrap();
            let mut out = AlignedBlocks::zeroed(2);
            wal.read_at(BLOCK as u64, out.as_bytes_mut()).unwrap();
            assert_eq!(out.as_bytes()[0], 0x31);
            assert_eq!(out.as_bytes()[BLOCK], 0x42);
            assert_eq!(wal.reduce(32).unwrap(), 1);
        }
        let _ = fs::remove_dir_all(journal.parent().unwrap());
    }

    #[test]
    fn unsynced_tail_is_not_recovered() {
        let (journal, base) = paths("unsynced");
        {
            let wal = PersistentWal::open(&journal, &base, (BLOCK * 4) as u64, (BLOCK * 16) as u64)
                .unwrap();
            wal.append_contiguous(0, &vec![0x5a; BLOCK]).unwrap();
        }
        let wal =
            PersistentWal::open(&journal, &base, (BLOCK * 4) as u64, (BLOCK * 16) as u64).unwrap();
        let mut out = vec![0xff; BLOCK];
        wal.read_at(0, &mut out).unwrap();
        assert_eq!(out, vec![0; BLOCK]);
        let _ = fs::remove_dir_all(journal.parent().unwrap());
    }

    #[test]
    fn crc_mode_rejects_durable_payload_corruption() {
        let (journal, base) = paths("crc-corruption");
        {
            let wal = PersistentWal::open(&journal, &base, (BLOCK * 4) as u64, (BLOCK * 16) as u64)
                .unwrap();
            wal.append_contiguous(0, &vec![0x3c; BLOCK]).unwrap();
            wal.sync().unwrap();
        }
        let journal_file = OpenOptions::new().write(true).open(&journal).unwrap();
        write_all_at(&journal_file, &[0xa5], DATA_START + BLOCK as u64).unwrap();
        journal_file.sync_data().unwrap();
        let error = PersistentWal::open(&journal, &base, (BLOCK * 4) as u64, (BLOCK * 16) as u64)
            .err()
            .expect("corrupt payload must fail recovery");
        assert!(error.to_string().contains("payload checksum mismatch"));
        let _ = fs::remove_dir_all(journal.parent().unwrap());
    }

    #[test]
    fn frame_mode_deliberately_relies_on_admitted_topology_for_payload_integrity() {
        let (journal, base) = paths("frame-corruption");
        {
            let wal = PersistentWal::open_with_integrity(
                &journal,
                &base,
                (BLOCK * 4) as u64,
                (BLOCK * 16) as u64,
                IntegrityMode::Frame,
            )
            .unwrap();
            wal.append_contiguous(0, &vec![0x3c; BLOCK]).unwrap();
            wal.sync().unwrap();
        }
        let journal_file = OpenOptions::new().write(true).open(&journal).unwrap();
        write_all_at(&journal_file, &[0xa5], DATA_START + BLOCK as u64).unwrap();
        journal_file.sync_data().unwrap();
        let wal = PersistentWal::open_with_integrity(
            &journal,
            &base,
            (BLOCK * 4) as u64,
            (BLOCK * 16) as u64,
            IntegrityMode::Frame,
        )
        .unwrap();
        let mut out = vec![0; BLOCK];
        wal.read_at(0, &mut out).unwrap();
        assert_eq!(out[0], 0xa5);
        let _ = fs::remove_dir_all(journal.parent().unwrap());
    }

    #[test]
    fn integrity_mode_is_persistent_geometry() {
        let (journal, base) = paths("mode-mismatch");
        {
            let wal = PersistentWal::open_with_integrity(
                &journal,
                &base,
                (BLOCK * 4) as u64,
                (BLOCK * 16) as u64,
                IntegrityMode::Frame,
            )
            .unwrap();
            wal.append_contiguous(0, &vec![0x44; BLOCK]).unwrap();
            wal.sync().unwrap();
        }
        let error = PersistentWal::open(&journal, &base, (BLOCK * 4) as u64, (BLOCK * 16) as u64)
            .err()
            .expect("integrity mode mismatch must fail");
        assert!(
            error
                .to_string()
                .contains("superblock geometry is incompatible")
        );
        let _ = fs::remove_dir_all(journal.parent().unwrap());
    }

    #[test]
    fn reducer_never_applies_an_unsynced_frame() {
        let (journal, base) = paths("reduce-unsynced");
        let wal =
            PersistentWal::open(&journal, &base, (BLOCK * 4) as u64, (BLOCK * 16) as u64).unwrap();
        wal.append_contiguous(0, &vec![0x77; BLOCK]).unwrap();
        assert_eq!(wal.reduce(16).unwrap(), 0);
        let mut base_page = vec![0xff; BLOCK];
        read_exact_at(&wal.base, &mut base_page, 0).unwrap();
        assert_eq!(base_page, vec![0; BLOCK]);
        let _ = fs::remove_dir_all(journal.parent().unwrap());
    }

    #[test]
    fn retained_tail_survives_reduction_and_replays_to_staged_terminal() {
        let (journal, base) = paths("retained-tail");
        let destination = journal.parent().unwrap().join("destination.img");
        let wal = Arc::new(
            PersistentWal::open(&journal, &base, (BLOCK * 4) as u64, (BLOCK * 16) as u64).unwrap(),
        );
        let pin = PersistentWal::pin_retained_tail(&wal);
        assert_eq!(pin.start_sequence(), 1);
        let payload = vec![0x6d; BLOCK];
        wal.append_contiguous(BLOCK as u64, &payload).unwrap();
        let latest_payload = vec![0x7e; BLOCK];
        wal.append_contiguous(BLOCK as u64, &latest_payload)
            .unwrap();
        let durable = wal.sync().unwrap();
        assert_eq!(wal.reduce(16).unwrap(), 2);
        assert!(!wal.reset_if_drained().unwrap());
        drop(pin);
        drop(wal);

        // The published reduced HWM remained behind the retained suffix, so a
        // restart can reconstruct and re-pin it from migration metadata.
        let wal = Arc::new(
            PersistentWal::open(&journal, &base, (BLOCK * 4) as u64, (BLOCK * 16) as u64).unwrap(),
        );
        assert_eq!(wal.stats().reduced_sequence, 0);
        assert_eq!(wal.stats().pending_frames, 2);
        let pin = PersistentWal::pin_retained_from(&wal, 1).unwrap();

        let destination_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&destination)
            .unwrap();
        destination_file.set_len((BLOCK * 4) as u64).unwrap();
        let replay = pin.replay_into(durable, &destination_file).unwrap();
        assert_eq!(replay.records_replayed, 1);
        assert_eq!(replay.bytes_replayed, BLOCK as u64);
        let mut page = vec![0; BLOCK];
        read_exact_at(&destination_file, &mut page, BLOCK as u64).unwrap();
        assert_eq!(page, latest_payload);
        assert_eq!(wal.reduce(16).unwrap(), 2);
        drop(pin);
        assert!(wal.reset_if_drained().unwrap());
        let _ = fs::remove_dir_all(journal.parent().unwrap());
    }

    #[test]
    fn sparse_presized_files_are_rejected_or_preallocated_by_policy() {
        let (journal, base) = paths("sparse-policy");
        File::create(&journal)
            .unwrap()
            .set_len((BLOCK * 16) as u64)
            .unwrap();
        File::create(&base)
            .unwrap()
            .set_len((BLOCK * 4) as u64)
            .unwrap();
        let strict = PersistentWalOpenOptions {
            file_provisioning: FileProvisioning::RequireAllocated,
            ..PersistentWalOpenOptions::default()
        };
        let error = PersistentWal::open_with_options(
            &journal,
            &base,
            (BLOCK * 4) as u64,
            (BLOCK * 16) as u64,
            IntegrityMode::Crc32c,
            strict,
        )
        .err()
        .expect("sparse presized files must fail strict admission");
        assert!(error.to_string().contains("not fully allocated"));

        let wal =
            PersistentWal::open(&journal, &base, (BLOCK * 4) as u64, (BLOCK * 16) as u64).unwrap();
        assert_eq!(wal.stats().journal_backing.kind, BackingKind::RegularFile);
        drop(wal);
        PersistentWal::open_with_options(
            &journal,
            &base,
            (BLOCK * 4) as u64,
            (BLOCK * 16) as u64,
            IntegrityMode::Crc32c,
            strict,
        )
        .unwrap();
        let _ = fs::remove_dir_all(journal.parent().unwrap());
    }

    #[test]
    fn journal_and_base_cannot_alias_the_same_file() {
        let (journal, _) = paths("same-backing");
        let error =
            PersistentWal::open(&journal, &journal, (BLOCK * 4) as u64, (BLOCK * 16) as u64)
                .err()
                .expect("same backing must be rejected");
        assert!(error.to_string().contains("distinct backing stores"));
        let _ = fs::remove_dir_all(journal.parent().unwrap());
    }
}
