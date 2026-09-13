//! Bounded, lane-owned client custody journal. No materialized volume/base file.
//!
//! Metadata is copied; application payload is borrowed in a vectored append.
//! Direct mode refuses unaligned buffers rather than introducing a bounce
//! copy. Replay exposes file extents for sendfile or a registered-buffer disk
//! read, rather than materializing a second userspace payload.

use crate::client_wal_commitment::{CommitScope, DurableReceipt, RemoteRedundancy, Replica};
use crate::persistent_wal::BackingIoMode;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, IoSlice};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

const PAGE: usize = 4096;
const START: u64 = (PAGE * 2) as u64;
const HASH: usize = PAGE - 32;
const RECORD_START: usize = 128;
pub const MAX_BATCH_RECORDS: usize = (HASH - RECORD_START) / 16;
const SUPER: &[u8; 8] = b"ZCLOCW01";
const FRAME: &[u8; 8] = b"ZCLOCWFR";
const PADDING: &[u8; 8] = b"ZCLOCWPD";

#[repr(C, align(4096))]
struct Page([u8; PAGE]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Record {
    pub logical_offset: u64,
    pub length: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplayRecord {
    pub sequence: u64,
    pub logical_offset: u64,
    pub file_offset: u64,
    pub length: u64,
}

#[derive(Clone, Debug)]
struct Batch {
    start: u64,
    first: u64,
    records: Vec<Record>,
}

impl Batch {
    fn last(&self) -> u64 {
        self.first + self.records.len() as u64 - 1
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Checkpoint {
    generation: u64,
    head: u64,
    tail: u64,
    durable: u64,
    released: u64,
}

pub struct Journal {
    file: File,
    capacity: u64,
    scope: CommitScope,
    replica: Replica,
    direct: bool,
    checkpoint: Checkpoint,
    tail: u64,
    submitted: u64,
    batches: VecDeque<Batch>,
    poisoned: bool,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn put(page: &mut Page, offset: usize, value: u64) {
    page.0[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn get(page: &Page, offset: usize) -> u64 {
    u64::from_le_bytes(page.0[offset..offset + 8].try_into().unwrap())
}
fn seal(page: &mut Page) {
    let hash = Sha256::digest(&page.0[..HASH]);
    page.0[HASH..].copy_from_slice(&hash);
}
fn verified(page: &Page) -> bool {
    Sha256::digest(&page.0[..HASH])[..] == page.0[HASH..]
}

impl Journal {
    /// The file is exclusively locked for this lane's lifetime. Creation is
    /// non-destructive; existing files must match the size, scope and owner.
    /// tmpfs is rejected: a filesystem fsync success there is not persistence.
    pub fn open(
        path: &Path,
        capacity: u64,
        scope: CommitScope,
        replica: Replica,
        mode: BackingIoMode,
    ) -> io::Result<Self> {
        if capacity < (PAGE * 4) as u64
            || capacity % PAGE as u64 != 0
            || capacity > (i64::MAX as u64 - START) / 2
            || scope.volume == 0
            || scope.log == 0
            || scope.writer_epoch == 0
            || replica.id == 0
            || replica.incarnation == 0
            || replica.failure_domain == 0
        {
            return Err(invalid("invalid client WAL capacity or custody identity"));
        }
        let flags = libc::O_NOFOLLOW
            | if mode == BackingIoMode::Direct {
                libc::O_DIRECT
            } else {
                0
            };
        let (file, created) = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(flags)
            .open(path)
        {
            Ok(file) => (file, true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(flags)
                    .open(path)?,
                false,
            ),
            Err(e) => return Err(e),
        };
        if !file.metadata()?.is_file() {
            return Err(invalid(
                "client custody WAL must be a regular terminal file",
            ));
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::fstatfs(file.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let fs = unsafe { fs.assume_init() };
        if fs.f_type as u64 == 0x01021994 || fs.f_type as u64 == 0x858458f6 {
            return Err(invalid(
                "volatile tmpfs/ramfs cannot supply client-local durable custody",
            ));
        }
        if created {
            let rc =
                unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, (START + capacity) as i64) };
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc));
            }
        } else if file.metadata()?.len() != START + capacity {
            return Err(invalid(
                "existing client WAL size differs from the configured capacity",
            ));
        }
        let mut journal = Self {
            file,
            capacity,
            scope,
            replica,
            direct: mode == BackingIoMode::Direct,
            checkpoint: Checkpoint::default(),
            tail: 0,
            submitted: 0,
            batches: VecDeque::new(),
            poisoned: false,
        };
        if created {
            journal.persist(Checkpoint::default())?;
            // File creation must survive a power loss, not just its contents.
            File::open(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?
            .sync_all()?;
        } else {
            journal.recover()?;
        }
        Ok(journal)
    }

    pub fn scope(&self) -> CommitScope {
        self.scope
    }
    pub fn submitted_hwm(&self) -> u64 {
        self.submitted
    }
    pub fn durable_hwm(&self) -> u64 {
        self.checkpoint.durable
    }
    pub fn released_hwm(&self) -> u64 {
        self.checkpoint.released
    }
    pub fn used_bytes(&self) -> u64 {
        self.tail - self.checkpoint.head
    }
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity
    }
    pub fn file(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// Read a still-retained payload extent into the caller's final aligned
    /// read buffer. The sequence check prevents using an index after ring reuse.
    pub fn read_retained(
        &self,
        record: ReplayRecord,
        within: u64,
        out: &mut [u8],
    ) -> io::Result<()> {
        self.healthy()?;
        if record.sequence <= self.released_hwm()
            || record.sequence > self.durable_hwm()
            || within
                .checked_add(out.len() as u64)
                .is_none_or(|end| end > record.length)
            || record.file_offset < START
            || record
                .file_offset
                .checked_add(record.length)
                .is_none_or(|end| end > START + self.capacity)
            || (self.direct
                && (out.as_ptr() as usize % PAGE != 0
                    || out.len() % PAGE != 0
                    || within % PAGE as u64 != 0))
        {
            return Err(invalid("read is outside retained aligned custody"));
        }
        read_at(&self.file, out, record.file_offset + within)
    }

    fn healthy(&self) -> io::Result<()> {
        if self.poisoned {
            Err(io::Error::other(
                "client WAL I/O failed; close and recover before reuse",
            ))
        } else {
            Ok(())
        }
    }

    fn physical(&self, offset: u64) -> u64 {
        START + offset % self.capacity
    }

    /// One metadata block plus the original application buffers. No payload
    /// serialization/copy or shared lock. Callers can batch random 4K writes or
    /// pass a single large sequential extent. The returned HWM is NOT durable.
    pub fn append(&mut self, records: &[Record], payload: &[IoSlice<'_>]) -> io::Result<u64> {
        self.healthy()?;
        if records.is_empty() || records.len() > MAX_BATCH_RECORDS || payload.len() > 1023 {
            return Err(invalid("client WAL batch/IOV count is out of range"));
        }
        let mut bytes = 0u64;
        for record in records {
            if record.length == 0
                || record.length % PAGE as u64 != 0
                || record.logical_offset % PAGE as u64 != 0
                || record.logical_offset.checked_add(record.length).is_none()
            {
                return Err(invalid(
                    "client WAL records require valid 4K-aligned logical ranges",
                ));
            }
            bytes = bytes
                .checked_add(record.length)
                .ok_or_else(|| invalid("batch overflow"))?;
        }
        let payload_bytes = payload.iter().try_fold(0u64, |sum, p| {
            if self.direct && (p.as_ptr() as usize % PAGE != 0 || p.len() % PAGE != 0) {
                return Err(invalid(
                    "direct WAL requires aligned arena buffers; no bounce copy",
                ));
            }
            sum.checked_add(p.len() as u64)
                .ok_or_else(|| invalid("payload overflow"))
        })?;
        if bytes != payload_bytes {
            return Err(invalid("WAL records do not describe the borrowed payload"));
        }
        let span = bytes
            .checked_add(PAGE as u64)
            .ok_or_else(|| invalid("frame overflow"))?;
        if span > self.capacity / 2 {
            return Err(invalid("one WAL batch may use at most half the ring"));
        }
        let remaining = self.capacity - self.tail % self.capacity;
        let pad = if span > remaining { remaining } else { 0 };
        let end = self
            .tail
            .checked_add(pad)
            .and_then(|v| v.checked_add(span))
            .ok_or_else(|| invalid("client WAL cursor exhausted"))?;
        let last = self
            .submitted
            .checked_add(records.len() as u64)
            .ok_or_else(|| invalid("client WAL sequence exhausted"))?;
        if end - self.checkpoint.head > self.capacity {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "client WAL custody full; wait for remote repair/commit, never evict early-ACKed data",
            ));
        }
        // A partial disk operation must never let a subsequent call bless its
        // suffix. Capacity/shape rejections above are retryable without poison.
        self.poisoned = true;
        if pad != 0 {
            let mut page = Page([0; PAGE]);
            page.0[..8].copy_from_slice(PADDING);
            put(&mut page, 8, self.tail);
            put(&mut page, 16, pad);
            seal(&mut page);
            write_at(&self.file, &page.0, self.physical(self.tail))?;
        }
        let start = self.tail + pad;
        let mut page = Page([0; PAGE]);
        page.0[..8].copy_from_slice(FRAME);
        put(&mut page, 8, start);
        put(&mut page, 16, span);
        put(&mut page, 24, self.submitted + 1);
        put(&mut page, 32, records.len() as u64);
        for (index, record) in records.iter().enumerate() {
            put(&mut page, RECORD_START + 16 * index, record.logical_offset);
            put(&mut page, RECORD_START + 16 * index + 8, record.length);
        }
        seal(&mut page);
        let mut iov = Vec::with_capacity(payload.len() + 1);
        iov.push(IoSlice::new(&page.0));
        iov.extend(payload.iter().map(|p| IoSlice::new(p)));
        writev_at(&self.file, &mut iov, self.physical(start))?;
        self.batches.push_back(Batch {
            start,
            first: self.submitted + 1,
            records: records.to_vec(),
        });
        self.tail = end;
        self.submitted = last;
        self.poisoned = false;
        Ok(last)
    }

    pub fn commit(&mut self) -> io::Result<DurableReceipt> {
        self.healthy()?;
        self.poisoned = true;
        // Publish a tail only after all its payload/metadata is on media.
        self.file.sync_data()?;
        self.persist(Checkpoint {
            tail: self.tail,
            durable: self.submitted,
            ..self.checkpoint
        })?;
        self.poisoned = false;
        Ok(DurableReceipt {
            scope: self.scope,
            replica: self.replica.id,
            incarnation: self.replica.incarnation,
            through: self.checkpoint.durable,
        })
    }

    /// Only the completion tracker can authorize custody release. An early
    /// ACK integer is deliberately not accepted by this interface.
    /// Persist the release before overwriting even one byte of retained data.
    /// Partial batches remain intact until all of their records are released.
    pub fn release_remote_prefix(&mut self, proof: &RemoteRedundancy<'_>) -> io::Result<()> {
        if proof.scope() != self.scope {
            return Err(invalid("foreign remote custody release proof"));
        }
        self.release_verified_frontier(proof.through().min(self.checkpoint.durable))
    }

    fn release_verified_frontier(&mut self, through: u64) -> io::Result<()> {
        self.healthy()?;
        if through < self.checkpoint.released || through > self.checkpoint.durable {
            return Err(invalid("invalid remote-redundant release frontier"));
        }
        if through == self.checkpoint.released {
            return Ok(());
        }
        let count = self
            .batches
            .iter()
            .take_while(|b| b.last() <= through)
            .count();
        let head = self
            .batches
            .get(count)
            .map_or(self.checkpoint.tail, |b| b.start.min(self.checkpoint.tail));
        self.poisoned = true;
        self.persist(Checkpoint {
            head,
            released: through,
            ..self.checkpoint
        })?;
        self.batches.drain(..count);
        self.poisoned = false;
        Ok(())
    }

    /// Borrowing the journal prevents ring reuse while these file extents are
    /// being replayed. NIC completions must precede releasing that ownership.
    pub fn replay(
        &self,
        from: u64,
        through: u64,
    ) -> io::Result<impl Iterator<Item = ReplayRecord> + '_> {
        self.healthy()?;
        if from == 0
            || from <= self.checkpoint.released
            || through > self.checkpoint.durable
            || from > through.saturating_add(1)
        {
            return Err(invalid("replay range is outside retained durable custody"));
        }
        Ok(self
            .batches
            .iter()
            .flat_map(move |batch| {
                let mut offset = self.physical(batch.start) + PAGE as u64;
                batch
                    .records
                    .iter()
                    .enumerate()
                    .map(move |(index, record)| {
                        let result = ReplayRecord {
                            sequence: batch.first + index as u64,
                            logical_offset: record.logical_offset,
                            file_offset: offset,
                            length: record.length,
                        };
                        offset += record.length;
                        result
                    })
            })
            .filter(move |r| r.sequence >= from && r.sequence <= through))
    }

    fn persist(&mut self, mut checkpoint: Checkpoint) -> io::Result<()> {
        checkpoint.generation = self
            .checkpoint
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("client WAL checkpoint generation exhausted"))?;
        let mut page = Page([0; PAGE]);
        page.0[..8].copy_from_slice(SUPER);
        for (index, value) in [
            checkpoint.generation,
            self.capacity,
            self.scope.volume,
            self.scope.log,
            self.scope.writer_epoch,
            self.scope.lane as u64,
            self.replica.id,
            self.replica.incarnation,
            self.replica.failure_domain,
            checkpoint.head,
            checkpoint.tail,
            checkpoint.durable,
            checkpoint.released,
        ]
        .iter()
        .enumerate()
        {
            put(&mut page, 8 + index * 8, *value);
        }
        seal(&mut page);
        write_at(&self.file, &page.0, checkpoint.generation % 2 * PAGE as u64)?;
        self.file.sync_data()?;
        self.checkpoint = checkpoint;
        Ok(())
    }

    fn recover(&mut self) -> io::Result<()> {
        let mut candidates = Vec::new();
        for slot in 0..2 {
            let mut page = Page([0; PAGE]);
            read_at(&self.file, &mut page.0, slot * PAGE as u64)?;
            if page.0[..8] != SUPER[..] || !verified(&page) {
                continue;
            }
            for (offset, expected) in [
                (16, self.capacity),
                (24, self.scope.volume),
                (32, self.scope.log),
                (40, self.scope.writer_epoch),
                (48, self.scope.lane as u64),
                (56, self.replica.id),
                (64, self.replica.incarnation),
                (72, self.replica.failure_domain),
            ] {
                if get(&page, offset) != expected {
                    return Err(invalid("client WAL belongs to a different scope/owner"));
                }
            }
            candidates.push(Checkpoint {
                generation: get(&page, 8),
                head: get(&page, 80),
                tail: get(&page, 88),
                durable: get(&page, 96),
                released: get(&page, 104),
            });
        }
        let cp = candidates
            .into_iter()
            .max_by_key(|c| c.generation)
            .ok_or_else(|| invalid("client WAL has no valid committed checkpoint"))?;
        if cp.head > cp.tail
            || cp.tail - cp.head > self.capacity
            || cp.released > cp.durable
            || cp.head % PAGE as u64 != 0
            || cp.tail % PAGE as u64 != 0
        {
            return Err(invalid("client WAL checkpoint violates ring/commit bounds"));
        }
        let mut cursor = cp.head;
        let mut previous = None;
        while cursor < cp.tail {
            let mut page = Page([0; PAGE]);
            read_at(&self.file, &mut page.0, self.physical(cursor))?;
            let span = get(&page, 16);
            if !verified(&page)
                || get(&page, 8) != cursor
                || span < PAGE as u64
                || span % PAGE as u64 != 0
                || span > cp.tail - cursor
                || span > self.capacity - cursor % self.capacity
            {
                return Err(invalid(
                    "retained committed WAL frame is damaged; refusing partial recovery",
                ));
            }
            if page.0[..8] == PADDING[..] {
                if span != self.capacity - cursor % self.capacity {
                    return Err(invalid("invalid WAL wrap padding"));
                }
                cursor += span;
                continue;
            }
            let first = get(&page, 24);
            let count = get(&page, 32);
            if page.0[..8] != FRAME[..]
                || count == 0
                || count > MAX_BATCH_RECORDS as u64
                || first == 0
                || first
                    .checked_add(count - 1)
                    .is_none_or(|last| last > cp.durable)
                || previous.is_some_and(|last: u64| first != last + 1)
                || (previous.is_none() && first > cp.released + 1)
            {
                return Err(invalid(
                    "retained WAL sequence has a hole or wrong frame type",
                ));
            }
            let mut records = Vec::with_capacity(count as usize);
            let mut bytes = PAGE as u64;
            for index in 0..count as usize {
                let record = Record {
                    logical_offset: get(&page, RECORD_START + index * 16),
                    length: get(&page, RECORD_START + index * 16 + 8),
                };
                if record.length == 0
                    || record.length % PAGE as u64 != 0
                    || record.logical_offset % PAGE as u64 != 0
                    || record.logical_offset.checked_add(record.length).is_none()
                {
                    return Err(invalid("retained WAL contains an invalid logical range"));
                }
                bytes = bytes
                    .checked_add(record.length)
                    .ok_or_else(|| invalid("WAL record overflow"))?;
                records.push(record);
            }
            if bytes != span {
                return Err(invalid("WAL payload/frame size mismatch"));
            }
            let batch = Batch {
                start: cursor,
                first,
                records,
            };
            previous = Some(batch.last());
            self.batches.push_back(batch);
            cursor += span;
        }
        if previous.unwrap_or(cp.released) != cp.durable {
            return Err(invalid("retained WAL does not reach its durable HWM"));
        }
        self.checkpoint = cp;
        self.tail = cp.tail;
        self.submitted = cp.durable;
        Ok(())
    }
}

fn write_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<()> {
    writev_at(file, &mut [IoSlice::new(bytes)], offset)
}

fn writev_at(file: &File, mut buffers: &mut [IoSlice<'_>], mut offset: u64) -> io::Result<()> {
    while !buffers.is_empty() {
        let rc = unsafe {
            libc::pwritev(
                file.as_raw_fd(),
                buffers.as_ptr().cast(),
                buffers.len() as i32,
                offset as i64,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if rc == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        offset += rc as u64;
        IoSlice::advance_slices(&mut buffers, rc as usize);
    }
    Ok(())
}

fn read_at(file: &File, mut bytes: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        match file.read_at(bytes, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                offset += n as u64;
                bytes = &mut bytes[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestPath(PathBuf);
    impl TestPath {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::var_os("ZC_CLIENT_WAL_TEST_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/var/tmp"));
            Self(root.join(format!(
                "zc-client-wal-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }
        fn open(&self, capacity: u64, mode: BackingIoMode) -> Journal {
            Journal::open(
                &self.0,
                capacity,
                CommitScope {
                    volume: 1,
                    log: 2,
                    writer_epoch: 3,
                    lane: 0,
                },
                Replica {
                    id: 1,
                    incarnation: 1,
                    failure_domain: 1,
                },
                mode,
            )
            .unwrap()
        }
    }
    impl Drop for TestPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn append(journal: &mut Journal, page: &Page, offset: u64) -> u64 {
        journal
            .append(
                &[Record {
                    logical_offset: offset,
                    length: PAGE as u64,
                }],
                &[IoSlice::new(&page.0)],
            )
            .unwrap()
    }

    #[test]
    fn direct_borrowed_arena_commit_reopen_and_replay() {
        let path = TestPath::new();
        let mut journal = path.open(16 * PAGE as u64, BackingIoMode::Direct);
        let page = Page([0x6a; PAGE]);
        assert_eq!(append(&mut journal, &page, 17 * PAGE as u64), 1);
        assert_eq!(journal.durable_hwm(), 0);
        assert_eq!(journal.commit().unwrap().through, 1);
        drop(journal);
        let journal = path.open(16 * PAGE as u64, BackingIoMode::Direct);
        let records = journal.replay(1, 1).unwrap().collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].logical_offset, 17 * PAGE as u64);
        let mut actual = Page([0; PAGE]);
        read_at(&journal.file, &mut actual.0, records[0].file_offset).unwrap();
        assert_eq!(actual.0, page.0);
    }

    #[test]
    fn uncommitted_tail_never_becomes_recovered_commitment() {
        let path = TestPath::new();
        let mut journal = path.open(16 * PAGE as u64, BackingIoMode::Buffered);
        let page = Page([7; PAGE]);
        append(&mut journal, &page, 0);
        journal.commit().unwrap();
        append(&mut journal, &page, PAGE as u64);
        journal.file.sync_data().unwrap(); // Payload on media without a published commit.
        drop(journal);
        let mut journal = path.open(16 * PAGE as u64, BackingIoMode::Buffered);
        assert_eq!(journal.submitted_hwm(), 1);
        assert_eq!(journal.replay(1, 1).unwrap().count(), 1);
        assert!(journal.replay(1, 2).is_err());
        assert_eq!(append(&mut journal, &page, 2 * PAGE as u64), 2);
    }

    #[test]
    fn custody_pressure_is_backpressure_and_release_allows_wrap() {
        let path = TestPath::new();
        let capacity = 6 * PAGE as u64;
        let page = Page([0x3c; PAGE]);
        let mut journal = path.open(capacity, BackingIoMode::Buffered);
        for _ in 0..3 {
            append(&mut journal, &page, 0);
        }
        journal.commit().unwrap();
        assert_eq!(journal.used_bytes(), capacity);
        let err = journal
            .append(
                &[Record {
                    logical_offset: 0,
                    length: PAGE as u64,
                }],
                &[IoSlice::new(&page.0)],
            )
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(journal.replay(1, 3).unwrap().count(), 3);
        journal.release_verified_frontier(1).unwrap();
        assert_eq!(append(&mut journal, &page, PAGE as u64), 4);
        journal.commit().unwrap();
        drop(journal);
        let journal = path.open(capacity, BackingIoMode::Buffered);
        assert_eq!(
            journal
                .replay(2, 4)
                .unwrap()
                .map(|r| r.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
    }

    #[test]
    fn random_batch_has_no_payload_copy_and_partial_release_keeps_batch() {
        let path = TestPath::new();
        let mut journal = path.open(16 * PAGE as u64, BackingIoMode::Direct);
        let pages = [Page([1; PAGE]), Page([2; PAGE])];
        let records = [
            Record {
                logical_offset: 32 * PAGE as u64,
                length: PAGE as u64,
            },
            Record {
                logical_offset: 0,
                length: PAGE as u64,
            },
        ];
        journal
            .append(
                &records,
                &[IoSlice::new(&pages[0].0), IoSlice::new(&pages[1].0)],
            )
            .unwrap();
        journal.commit().unwrap();
        journal.release_verified_frontier(1).unwrap();
        assert_eq!(journal.used_bytes(), 3 * PAGE as u64);
        drop(journal);
        let journal = path.open(16 * PAGE as u64, BackingIoMode::Direct);
        let replay = journal.replay(2, 2).unwrap().next().unwrap();
        let mut actual = Page([0; PAGE]);
        read_at(&journal.file, &mut actual.0, replay.file_offset).unwrap();
        assert_eq!(actual.0, pages[1].0);
    }

    #[test]
    fn damaged_committed_metadata_and_foreign_owner_fail_closed() {
        let path = TestPath::new();
        let mut journal = path.open(16 * PAGE as u64, BackingIoMode::Buffered);
        append(&mut journal, &Page([8; PAGE]), 0);
        journal.commit().unwrap();
        let scope = journal.scope;
        let mut owner = journal.replica;
        drop(journal);
        owner.incarnation += 1;
        assert!(
            Journal::open(
                &path.0,
                16 * PAGE as u64,
                scope,
                owner,
                BackingIoMode::Buffered
            )
            .is_err()
        );
        let file = OpenOptions::new().write(true).open(&path.0).unwrap();
        file.write_at(&[0; 8], START).unwrap();
        file.sync_data().unwrap();
        owner.incarnation -= 1;
        assert!(
            Journal::open(
                &path.0,
                16 * PAGE as u64,
                scope,
                owner,
                BackingIoMode::Buffered
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_two_writers_and_does_not_hide_unaligned_direct_copies() {
        let path = TestPath::new();
        let mut journal = path.open(16 * PAGE as u64, BackingIoMode::Direct);
        assert!(
            Journal::open(
                &path.0,
                16 * PAGE as u64,
                journal.scope,
                journal.replica,
                BackingIoMode::Direct
            )
            .is_err()
        );
        let storage = [Page([0; PAGE]), Page([0; PAGE])];
        let bytes =
            unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>().add(1), PAGE) };
        assert!(
            journal
                .append(
                    &[Record {
                        logical_offset: 0,
                        length: PAGE as u64
                    }],
                    &[IoSlice::new(bytes)]
                )
                .is_err()
        );
        assert_eq!(journal.submitted_hwm(), 0);
    }
}
