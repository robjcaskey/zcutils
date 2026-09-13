//! Persistent, lane-owned terminal and serial forwarding adapter for live
//! client custody. Placement remains in userspace. The terminal actor owns
//! ordering; a separate forwarder can publish a tail receipt while local fsync
//! is stalled. Only small descriptors and immutable arena leases cross queues.
//!
//! This TCP adapter requires an explicitly trusted private network. Scope and
//! incarnation validation are fencing, NOT peer authentication. Do not expose
//! it on an untrusted/public network; authenticated transport is separate work.

use crate::client_wal_commitment::{CommitScope, Replica};
use crate::persistent_wal::{
    BackingIoMode, FileProvisioning, IntegrityMode, PersistentWalOpenOptions, PersistentWalRuntime,
};
use crate::raid_mirror_serial::Arena;
use crate::*;
use serde::{Deserialize, Serialize};
use std::mem;

fn env_u64_or(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub(super) const MAX_PAYLOAD: usize = 64 * 1024;
pub(super) const HEADER_BYTES: usize = 128;
pub(super) const HELLO: u64 = 1;
pub(super) const WRITE: u64 = 2;
pub(super) const READ: u64 = 3;
pub(super) const SYNC: u64 = 4;
pub(super) const FENCE: u64 = 5;
pub(super) const COPY: u64 = 6;
const COPY_RANGE: u64 = 7;
const COPY_DONE: u64 = 8;
pub(super) const ACK: u64 = 9;
pub(super) const PATCH: u64 = 10;
pub(super) const PATCH_PAGES: u64 = 11;
pub(super) const PATCH_DONE: u64 = 12;
pub(super) const ACTIVATE: u64 = 13;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Peer {
    pub address: SocketAddr,
    pub owner: Replica,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub scope: CommitScope,
    pub owner: Replica,
    pub listen: SocketAddr,
    pub directory: PathBuf,
    pub logical_bytes: u64,
    pub journal_bytes: u64,
    pub downstream: Option<Peer>,
    #[serde(default)]
    pub allow_plaintext_private_network: bool,
    /// A replacement is never admitted as an empty, supposedly current replica.
    #[serde(default)]
    pub requires_copy: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Header {
    pub scope: CommitScope,
    pub op: u64,
    pub generation: u64,
    pub sequence: u64,
    pub offset: u64,
    pub length: u64,
    pub replica: u64,
    pub incarnation: u64,
    pub status: u64,
}

impl Header {
    pub fn request(scope: CommitScope, op: u64, generation: u64, sequence: u64) -> Self {
        Self {
            scope,
            op,
            generation,
            sequence,
            offset: 0,
            length: 0,
            replica: 0,
            incarnation: 0,
            status: 0,
        }
    }
    pub fn encode(self) -> [u8; HEADER_BYTES] {
        let mut bytes = [0; HEADER_BYTES];
        bytes[..8].copy_from_slice(b"ZCCUST01");
        for (index, value) in [
            self.scope.volume,
            self.scope.log,
            self.scope.writer_epoch,
            self.scope.lane as u64,
            self.op,
            self.generation,
            self.sequence,
            self.offset,
            self.length,
            self.replica,
            self.incarnation,
            self.status,
        ]
        .iter()
        .enumerate()
        {
            bytes[8 + 8 * index..16 + 8 * index].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }
    pub fn decode(bytes: &[u8; HEADER_BYTES], scope: CommitScope) -> io::Result<Self> {
        let get = |n: usize| u64::from_le_bytes(bytes[8 + n * 8..16 + n * 8].try_into().unwrap());
        if &bytes[..8] != b"ZCCUST01"
            || get(0) != scope.volume
            || get(1) != scope.log
            || get(2) != scope.writer_epoch
            || get(3) != u64::from(scope.lane)
            || bytes[104..].iter().any(|&v| v != 0)
        {
            return Err(io::Error::other(
                "foreign custody scope or unsupported wire version",
            ));
        }
        let result = Self {
            scope,
            op: get(4),
            generation: get(5),
            sequence: get(6),
            offset: get(7),
            length: get(8),
            replica: get(9),
            incarnation: get(10),
            status: get(11),
        };
        if !(HELLO..=ACTIVATE).contains(&result.op)
            || result.generation == 0
            || result.length > MAX_PAYLOAD as u64
        {
            return Err(io::Error::other("invalid custody frame bounds"));
        }
        Ok(result)
    }
    pub fn read(stream: &mut TcpStream, scope: CommitScope) -> io::Result<Self> {
        let mut bytes = [0; HEADER_BYTES];
        stream.read_exact(&mut bytes)?;
        Self::decode(&bytes, scope)
    }
    pub fn write(self, stream: &mut TcpStream, payload: &[u8]) -> io::Result<()> {
        let header = self.encode();
        let mut iov = [IoSlice::new(&header), IoSlice::new(payload)];
        tcp_write_all_vectored(stream, &mut iov, "live custody borrowed arena")
    }
    pub fn validate_ack(self, owner: Replica, generation: u64) -> io::Result<Self> {
        if self.op != ACK
            || self.replica != owner.id
            || self.incarnation != owner.incarnation
            || self.generation != generation
            || self.status != 0
        {
            return Err(io::Error::other(format!(
                "stale or rejected custody receipt: {self:?}"
            )));
        }
        Ok(self)
    }
}

pub(super) fn connect(address: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

pub(super) fn hello(
    stream: &mut TcpStream,
    scope: CommitScope,
    peer: &Peer,
    generation: u64,
) -> io::Result<Header> {
    Header::request(scope, HELLO, generation, 0).write(stream, &[])?;
    Header::read(stream, scope)?.validate_ack(peer.owner, generation)
}

pub(super) fn hello_read(
    stream: &mut TcpStream,
    scope: CommitScope,
    peer: &Peer,
    generation: u64,
) -> io::Result<Header> {
    Header {
        offset: 1,
        ..Header::request(scope, HELLO, generation, 0)
    }
    .write(stream, &[])?;
    Header::read(stream, scope)?.validate_ack(peer.owner, generation)
}

pub(super) fn durable_directory(path: &Path) -> io::Result<()> {
    if !path.exists() {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("custody directory needs a parent"))?;
        durable_directory(parent)?;
        match fs::create_dir(path) {
            Ok(()) => fs::File::open(parent)?.sync_all()?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    fs::File::open(path)?.sync_all()
}

/// Cold metadata, not an extra checkpoint per I/O. The terminal WAL's native
/// sequence is translated after a base copy; regular writes are exactly one
/// native frame each. The generation fences old sockets, including after reboot.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    scope: CommitScope,
    owner: Replica,
    generation: u64,
    logical_base: u64,
    native_base: u64,
    copying: bool,
    staged: bool,
}

struct Terminal {
    config: Config,
    wal: PersistentWalRuntime,
    identity: Identity,
    delay_ms: u64,
    delay_after: u64,
    failed: bool,
    copy_cursor: u64,
    patch: Option<(u64, u64)>,
    durable_native: u64,
}

impl Terminal {
    fn open(config: Config) -> io::Result<Self> {
        if !config.allow_plaintext_private_network
            || config.logical_bytes == 0
            || config.logical_bytes % 4096 != 0
            || config.scope.lane != 0
        {
            return Err(io::Error::other(
                "live custody requires explicit trusted TCP, 4K geometry and a single lane",
            ));
        }
        durable_directory(&config.directory)?;
        let wal = PersistentWalRuntime::open_with_options(
            config.directory.join("terminal.wal"),
            config.directory.join("terminal.base"),
            config.logical_bytes,
            config.journal_bytes,
            IntegrityMode::Crc32c,
            PersistentWalOpenOptions {
                file_provisioning: FileProvisioning::Preallocate,
                io_mode: BackingIoMode::Direct,
            },
        )?;
        wal.validate_persistent_backings()?;
        let identity_path = config.directory.join("identity.json");
        let identity = match fs::read(&identity_path) {
            Ok(bytes) => serde_json::from_slice::<Identity>(&bytes).map_err(io::Error::other)?,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && wal.stats().appended_sequence == 0 =>
            {
                Identity {
                    scope: config.scope,
                    owner: config.owner,
                    generation: 1,
                    logical_base: 0,
                    native_base: 0,
                    copying: config.requires_copy,
                    staged: config.requires_copy,
                }
            }
            Err(error) => return Err(error),
        };
        if identity.scope != config.scope
            || identity.owner != config.owner
            || identity.native_base > wal.stats().durable_sequence
        {
            return Err(io::Error::other(
                "terminal identity/sequence does not match its persisted WAL",
            ));
        }
        let durable_native = wal.stats().durable_sequence;
        let mut result = Self {
            config,
            wal,
            identity,
            delay_ms: env_u64_or("ZC_CUSTODY_TEST_COMMIT_DELAY_MS", 0),
            delay_after: env_u64_or("ZC_CUSTODY_TEST_COMMIT_DELAY_AFTER", 0),
            failed: false,
            copy_cursor: 0,
            patch: None,
            durable_native,
        };
        result.persist_identity()?;
        Ok(result)
    }
    fn persist_identity(&mut self) -> io::Result<()> {
        self.failed = true;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = self.config.directory.join("identity.next");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(&self.identity).map_err(io::Error::other)?)?;
        file.sync_all()?;
        fs::rename(tmp, self.config.directory.join("identity.json"))?;
        fs::File::open(&self.config.directory)?.sync_all()?;
        self.failed = false;
        Ok(())
    }
    fn hwm(&self) -> u64 {
        self.identity.logical_base + self.durable_native - self.identity.native_base
    }
    fn append(&mut self, offset: u64, payload: &[u8]) -> io::Result<()> {
        self.failed = true;
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            match self.wal.append_contiguous(offset, payload) {
                Ok(_) => break,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < until =>
                {
                    self.wal.sync()?;
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => return Err(error),
            }
        }
        self.durable_native = self.wal.sync()?;
        self.failed = false;
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        self.failed = true;
        self.durable_native = self.wal.sync()?;
        self.failed = false;
        Ok(())
    }
    fn execute(
        &mut self,
        request: Header,
        mut arena: Arc<Arena>,
        pages: &[u64],
    ) -> io::Result<Reply> {
        if self.failed {
            return Err(io::Error::other(
                "terminal fenced after persistent I/O failure",
            ));
        }
        if request.generation < self.identity.generation
            || (request.generation != self.identity.generation
                && !matches!(request.op, FENCE | COPY))
        {
            return Err(io::Error::other(
                "old or unadmitted writer generation was fenced",
            ));
        }
        if self.identity.copying
            && !matches!(
                request.op,
                HELLO | COPY | COPY_RANGE | COPY_DONE | PATCH_PAGES | PATCH_DONE
            )
        {
            return Err(io::Error::other(
                "incomplete replacement is not an active replica",
            ));
        }
        if self.identity.staged && matches!(request.op, WRITE | READ | SYNC | FENCE) {
            return Err(io::Error::other(
                "staged replacement has not been activated",
            ));
        }
        let mut bytes = 0;
        match request.op {
            HELLO => {}
            FENCE => {
                self.sync()?;
                self.identity.generation = request.generation;
                self.persist_identity()?;
            }
            COPY => {
                if !self.identity.copying || self.wal.stats().appended_sequence != 0 {
                    return Err(io::Error::other(
                        "copy target must be a fresh staged replica",
                    ));
                }
                self.identity.generation = request.generation;
                self.persist_identity()?;
            }
            PATCH => {
                if !self.identity.staged
                    || self.identity.copying
                    || request.offset != self.hwm()
                    || request.sequence < request.offset
                {
                    return Err(io::Error::other(
                        "state catchup requires a completed staged base and a matching prefix",
                    ));
                }
                self.identity.copying = true;
                self.patch = Some((request.offset, request.sequence));
                self.persist_identity()?;
            }
            PATCH_PAGES => {
                if self
                    .patch
                    .is_none_or(|(_, through)| through != request.sequence)
                    || pages.is_empty()
                    || pages.len() * 4096 != request.length as usize
                    || pages
                        .iter()
                        .any(|page| *page >= self.config.logical_bytes / 4096)
                {
                    return Err(io::Error::other("invalid staged catchup page batch"));
                }
                self.failed = true;
                self.wal
                    .append_pages(pages, &arena.bytes()[..request.length as usize])?;
                self.failed = false;
            }
            PATCH_DONE => {
                if self
                    .patch
                    .is_none_or(|(_, through)| through != request.sequence)
                {
                    return Err(io::Error::other(
                        "state catchup lacks its fenced target prefix",
                    ));
                }
                self.sync()?;
                self.identity.logical_base = request.sequence;
                self.identity.native_base = self.durable_native;
                self.identity.copying = false;
                self.patch = None;
                self.persist_identity()?;
            }
            ACTIVATE => {
                if !self.identity.staged || self.identity.copying || request.sequence != self.hwm()
                {
                    return Err(io::Error::other(
                        "activation requires a fully rebuilt durable prefix",
                    ));
                }
                self.identity.staged = false;
                self.persist_identity()?;
            }
            WRITE | COPY_RANGE => {
                validate_range(request, self.config.logical_bytes)?;
                if request.op == COPY_RANGE && !self.identity.copying {
                    return Err(io::Error::other("copy write outside staged reconstruction"));
                }
                if request.op == COPY_RANGE && request.offset != self.copy_cursor {
                    return Err(io::Error::other("base copy has a gap or overlapping range"));
                }
                if request.op == COPY_RANGE || request.sequence > self.hwm() {
                    if request.op == WRITE && request.sequence != self.hwm() + 1 {
                        return Err(io::Error::other("custody write would create a prefix gap"));
                    }
                    if request.op == WRITE
                        && request.sequence > self.delay_after
                        && self.delay_ms != 0
                    {
                        thread::sleep(Duration::from_millis(self.delay_ms));
                    }
                    self.append(request.offset, &arena.bytes()[..request.length as usize])?;
                    if request.op == COPY_RANGE {
                        self.copy_cursor += request.length;
                    }
                }
            }
            COPY_DONE => {
                if !self.identity.copying || self.copy_cursor != self.config.logical_bytes {
                    return Err(io::Error::other("copy already ended"));
                }
                self.sync()?;
                self.identity.logical_base = request.sequence;
                self.identity.native_base = self.durable_native;
                self.identity.copying = false;
                self.persist_identity()?;
            }
            SYNC => {
                self.sync()?;
            }
            READ => {
                validate_range(request, self.config.logical_bytes)?;
                bytes = request.length as usize;
                self.wal.read_at(
                    request.offset,
                    &mut Arc::get_mut(&mut arena)
                        .ok_or_else(|| io::Error::other("shared mutable read arena"))?
                        .bytes_mut()[..bytes],
                )?;
            }
            _ => return Err(io::Error::other("unexpected custody opcode")),
        }
        Ok(Reply {
            header: Header {
                op: ACK,
                sequence: self.hwm(),
                replica: self.config.owner.id,
                incarnation: self.config.owner.incarnation,
                length: bytes as u64,
                ..request
            },
            payload: if bytes == 0 { None } else { Some(arena) },
        })
    }
}

fn validate_range(request: Header, logical_bytes: u64) -> io::Result<()> {
    if request.length == 0
        || request.length > MAX_PAYLOAD as u64
        || request.length % 4096 != 0
        || request.offset % 4096 != 0
        || request
            .offset
            .checked_add(request.length)
            .is_none_or(|end| end > logical_bytes)
    {
        return Err(io::Error::other(
            "custody range must fit the volume and be 4K aligned",
        ));
    }
    Ok(())
}

struct Reply {
    header: Header,
    payload: Option<Arc<Arena>>,
}
struct Job {
    header: Header,
    arena: Arc<Arena>,
    replies: mpsc::SyncSender<Reply>,
    pages: Vec<u64>,
}

fn call_actor(tx: &mpsc::SyncSender<Job>, header: Header, arena: Arc<Arena>) -> io::Result<Reply> {
    let (replies, rx) = mpsc::sync_channel(1);
    tx.send(Job {
        header,
        arena,
        replies,
        pages: Vec::new(),
    })
    .map_err(io::Error::other)?;
    rx.recv_timeout(Duration::from_secs(30))
        .map_err(io::Error::other)
}

fn patch_from_client(
    input: &mut TcpStream,
    config: &Config,
    tx: &mpsc::SyncSender<Job>,
    header: Header,
) -> io::Result<()> {
    let mut arena = Arc::new(Arena::new(MAX_PAYLOAD)?);
    let ack = call_actor(tx, header, arena.clone())?;
    ack.header.write(input, &[])?;
    if ack.header.status != 0 {
        return Ok(());
    }
    loop {
        let request = Header::read(input, config.scope)?;
        if request.generation != header.generation || request.sequence != header.sequence {
            return Err(io::Error::other("staged patch changed its fenced prefix"));
        }
        if request.op == PATCH_DONE {
            let done = call_actor(tx, request, arena)?;
            done.header.write(input, &[])?;
            return Ok(());
        }
        if request.op != PATCH_PAGES || request.length == 0 || request.length % 4096 != 0 {
            return Err(io::Error::other("invalid staged patch framing"));
        }
        let count = request.length as usize / 4096;
        let mut wire = vec![0u8; count * 8];
        input.read_exact(&mut wire)?;
        let pages = wire
            .chunks_exact(8)
            .map(|v| u64::from_le_bytes(v.try_into().unwrap()))
            .collect();
        input.read_exact(
            &mut Arc::get_mut(&mut arena)
                .ok_or_else(|| io::Error::other("patch arena still leased"))?
                .bytes_mut()[..request.length as usize],
        )?;
        let (replies, rx) = mpsc::sync_channel(1);
        tx.send(Job {
            header: request,
            arena: arena.clone(),
            replies,
            pages,
        })
        .map_err(io::Error::other)?;
        let ack = rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(io::Error::other)?;
        ack.header.write(input, &[])?;
        if ack.header.status != 0 {
            return Ok(());
        }
    }
}

/// Admission/fencing only: these locks are never taken for a data request or
/// receipt. Retire disconnected/partitioned old sessions so repeated failover
/// cannot consume the bounded connection/arena budget with zombie sockets.
#[derive(Default)]
struct SessionRegistry {
    state: Mutex<(u64, u64, Vec<(u64, u64, TcpStream)>)>,
}
struct SessionGuard {
    registry: Arc<SessionRegistry>,
    id: u64,
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.registry
            .state
            .lock()
            .unwrap()
            .2
            .retain(|(id, _, _)| *id != self.id);
    }
}
impl SessionRegistry {
    fn fence(&self, generation: u64) {
        let mut state = self.state.lock().unwrap();
        state.0 = state.0.max(generation);
        let mut retired = 0;
        state.2.retain(|(_, epoch, stream)| {
            if *epoch < generation {
                let _ = stream.shutdown(std::net::Shutdown::Both);
                retired += 1;
                false
            } else {
                true
            }
        });
        println!(
            "custody-peer-fenced: generation={generation} retired_connections={retired} persisted_before_socket_retirement=true"
        );
    }
    fn register(self: &Arc<Self>, stream: &TcpStream, generation: u64) -> io::Result<SessionGuard> {
        let mut state = self.state.lock().unwrap();
        if generation < state.0 {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return Err(io::Error::other(
                "connection admission raced a persisted fence",
            ));
        }
        let id = state
            .1
            .checked_add(1)
            .ok_or_else(|| io::Error::other("session identifier exhausted"))?;
        state.1 = id;
        state.2.push((id, generation, stream.try_clone()?));
        Ok(SessionGuard {
            registry: self.clone(),
            id,
        })
    }
}

fn copy_from_tail(
    config: &Config,
    tx: &mpsc::SyncSender<Job>,
    header: Header,
) -> io::Result<Reply> {
    let peer = config
        .downstream
        .as_ref()
        .ok_or_else(|| io::Error::other("copy has no surviving source"))?;
    let mut source = connect(peer.address, Duration::from_secs(5))?;
    let source_hwm = hello_read(&mut source, config.scope, peer, header.generation)?.sequence;
    if source_hwm < header.sequence {
        return Err(io::Error::other(
            "base source is behind requested copy prefix",
        ));
    }
    let mut arena = Arc::new(Arena::new(MAX_PAYLOAD)?);
    let started = call_actor(tx, header, arena.clone())?;
    if started.header.status != 0 {
        return Ok(started);
    }
    println!(
        "custody-base-copy-start: replica={} retained_from={} source_hwm={source_hwm}",
        config.owner.id,
        header.sequence + 1
    );
    for offset in (0..config.logical_bytes).step_by(MAX_PAYLOAD) {
        let length = (config.logical_bytes - offset).min(MAX_PAYLOAD as u64);
        let request = Header {
            op: READ,
            offset,
            length,
            ..header
        };
        request.write(&mut source, &[])?;
        let ack =
            Header::read(&mut source, config.scope)?.validate_ack(peer.owner, header.generation)?;
        if ack.length != length || ack.offset != offset {
            return Err(io::Error::other("copy source shape mismatch"));
        }
        source.read_exact(
            &mut Arc::get_mut(&mut arena)
                .ok_or_else(|| io::Error::other("copy arena leased"))?
                .bytes_mut()[..length as usize],
        )?;
        let written = call_actor(
            tx,
            Header {
                op: COPY_RANGE,
                ..request
            },
            arena.clone(),
        )?;
        if written.header.status != 0 {
            return Ok(written);
        }
        let delay = env_u64_or("ZC_CUSTODY_TEST_COPY_DELAY_MS", 0);
        if delay != 0 {
            thread::sleep(Duration::from_millis(delay));
        }
    }
    let done = call_actor(
        tx,
        Header {
            op: COPY_DONE,
            ..header
        },
        arena,
    )?;
    println!(
        "custody-base-copy-complete: replica={} base_bytes={} staged=true",
        config.owner.id, config.logical_bytes
    );
    Ok(done)
}

fn serve_connection(
    mut input: TcpStream,
    config: Config,
    tx: mpsc::SyncSender<Job>,
    registry: Arc<SessionRegistry>,
) -> io::Result<()> {
    input.set_nodelay(true)?;
    input.set_write_timeout(Some(Duration::from_secs(5)))?;
    let initial = Header::read(&mut input, config.scope)?;
    if !matches!(initial.op, HELLO | FENCE | COPY | PATCH | ACTIVATE) {
        return Err(io::Error::other("custody connection lacks admission"));
    }
    if initial.op == PATCH {
        return patch_from_client(&mut input, &config, &tx, initial);
    }
    // Control admission is ordered by the same lane actor as all data, so a
    // fence cannot race a queued stale write into the surviving image.
    let admitted = if initial.op == COPY {
        copy_from_tail(&config, &tx, initial)?
    } else {
        call_actor(&tx, initial, Arc::new(Arena::new(MAX_PAYLOAD)?))?
    };
    admitted.header.write(&mut input, &[])?;
    if initial.op == COPY || admitted.header.status != 0 {
        return Ok(());
    }
    if initial.op == FENCE {
        registry.fence(initial.generation);
    }
    let _session = registry.register(&input, initial.generation)?;
    if initial.op == HELLO && initial.offset == 1 {
        // Read-only attachment: hand the one final buffer to the terminal
        // actor, then reclaim it after TX. No per-read allocation/copy, and no
        // pool of write-forwarding leases on a read-only connection.
        let mut arena = Arc::new(Arena::new(MAX_PAYLOAD)?);
        let (read_replies, read_results) = mpsc::sync_channel(1);
        loop {
            let request = match Header::read(&mut input, config.scope) {
                Ok(header) => header,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(error) => return Err(error),
            };
            if request.op != READ || request.generation != initial.generation {
                return Err(io::Error::other(
                    "read-only custody attachment cannot write or change generation",
                ));
            }
            validate_range(request, config.logical_bytes)?;
            tx.send(Job {
                header: request,
                arena,
                replies: read_replies.clone(),
                pages: Vec::new(),
            })
            .map_err(io::Error::other)?;
            let reply = read_results
                .recv_timeout(Duration::from_secs(30))
                .map_err(io::Error::other)?;
            if reply.header.status != 0 {
                reply.header.write(&mut input, &[])?;
                return Ok(());
            }
            arena = reply
                .payload
                .ok_or_else(|| io::Error::other("read result lacks final arena"))?;
            reply
                .header
                .write(&mut input, &arena.bytes()[..reply.header.length as usize])?;
        }
    }
    let (reply_tx, reply_rx) = mpsc::sync_channel::<Reply>(128);
    let mut output = input.try_clone()?;
    let output_worker = thread::spawn(move || -> io::Result<()> {
        while let Ok(reply) = reply_rx.recv() {
            reply.header.write(
                &mut output,
                reply
                    .payload
                    .as_ref()
                    .map_or(&[], |a| &a.bytes()[..reply.header.length as usize]),
            )?;
        }
        Ok(())
    });
    let (forward_tx, forward_rx) = mpsc::sync_channel::<(Header, Arc<Arena>)>(64);
    let forward_replies = reply_tx.clone();
    let scope = config.scope;
    let downstream = config.downstream.clone();
    let forward_worker = thread::spawn(move || -> io::Result<()> {
        let mut stream = None;
        while let Ok((header, arena)) = forward_rx.recv() {
            let Some(peer) = downstream.as_ref() else {
                continue;
            };
            if stream.is_none() {
                let mut connected = connect(peer.address, Duration::from_secs(5))?;
                hello(&mut connected, scope, peer, header.generation)?;
                stream = Some(connected);
            }
            let stream = stream.as_mut().unwrap();
            header.write(
                stream,
                if header.op == WRITE {
                    &arena.bytes()[..header.length as usize]
                } else {
                    &[]
                },
            )?;
            let ack = Header::read(stream, scope)?.validate_ack(peer.owner, header.generation)?;
            if ack.length != 0 {
                return Err(io::Error::other("data on a durability result lane"));
            }
            drop(arena);
            // This path never joins/waits for the local terminal writer.
            forward_replies
                .send(Reply {
                    header: ack,
                    payload: None,
                })
                .map_err(io::Error::other)?;
        }
        Ok(())
    });
    let result = (|| {
        let mut pool = (0..128)
            .map(|_| Arena::new(MAX_PAYLOAD).map(Arc::new))
            .collect::<io::Result<Vec<_>>>()?;
        let mut slot = 0;
        loop {
            let header = match Header::read(&mut input, config.scope) {
                Ok(h) => h,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error),
            };
            if header.generation != initial.generation || !matches!(header.op, WRITE | READ | SYNC)
            {
                return Err(io::Error::other(
                    "session generation or opcode changed without admission",
                ));
            }
            if matches!(header.op, READ | WRITE) {
                validate_range(header, config.logical_bytes)?;
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            while Arc::strong_count(&pool[slot]) != 1 {
                if Instant::now() >= deadline {
                    return Err(io::Error::other("bounded custody arena pool exhausted"));
                }
                thread::sleep(Duration::from_millis(1));
            }
            if header.op == WRITE {
                input.read_exact(
                    &mut Arc::get_mut(&mut pool[slot]).unwrap().bytes_mut()
                        [..header.length as usize],
                )?;
            }
            if header.op == READ {
                // Transfer exclusive ownership: the actor fills the final RX
                // buffer and the result writer keeps it until socket completion.
                let arena = mem::replace(&mut pool[slot], Arc::new(Arena::new(MAX_PAYLOAD)?));
                tx.send(Job {
                    header,
                    arena,
                    replies: reply_tx.clone(),
                    pages: Vec::new(),
                })
                .map_err(io::Error::other)?;
            } else {
                if config.downstream.is_some() {
                    forward_tx
                        .send((header, pool[slot].clone()))
                        .map_err(io::Error::other)?;
                }
                tx.send(Job {
                    header,
                    arena: pool[slot].clone(),
                    replies: reply_tx.clone(),
                    pages: Vec::new(),
                })
                .map_err(io::Error::other)?;
            }
            slot = (slot + 1) % pool.len();
        }
        Ok(())
    })();
    drop(forward_tx);
    drop(reply_tx);
    let forwarded = forward_worker
        .join()
        .map_err(|_| io::Error::other("custody forward worker panicked"))?;
    let output = output_worker
        .join()
        .map_err(|_| io::Error::other("custody result worker panicked"))?;
    result.and(forwarded).and(output)
}

pub(super) fn serve(config: Config) -> io::Result<()> {
    let mut terminal = Terminal::open(config.clone())?;
    let listener = TcpListener::bind(config.listen)?;
    let (tx, rx) = mpsc::sync_channel::<Job>(128);
    let actor = thread::spawn(move || -> io::Result<()> {
        maybe_pin_current_thread("custody-terminal", 0);
        while let Ok(job) = rx.recv() {
            let header = job.header;
            let reply = match terminal.execute(header, job.arena, &job.pages) {
                Ok(reply) => reply,
                Err(error) => {
                    eprintln!(
                        "custody-peer-rejected: replica={} op={} sequence={} error={error}",
                        terminal.config.owner.id, header.op, header.sequence
                    );
                    Reply {
                        header: Header {
                            op: ACK,
                            status: 1,
                            length: 0,
                            replica: terminal.config.owner.id,
                            incarnation: terminal.config.owner.incarnation,
                            ..header
                        },
                        payload: None,
                    }
                }
            };
            let _ = job.replies.send(reply);
        }
        Ok(())
    });
    println!(
        "custody-peer-ready: listen={} replica={} incarnation={} forwarding={} lane=0 terminal_worker=0 placement_owner=userspace bounded_queues=true payload_rebuffer_copies=0",
        config.listen,
        config.owner.id,
        config.owner.incarnation,
        config.downstream.is_some()
    );
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry = Arc::new(SessionRegistry::default());
    for incoming in listener.incoming() {
        let stream = incoming?;
        // Admission-only cap, never a shared per-I/O counter.
        if connections.fetch_add(1, Ordering::AcqRel) >= 8 {
            connections.fetch_sub(1, Ordering::AcqRel);
            drop(stream);
            continue;
        }
        let connections = connections.clone();
        let config = config.clone();
        let tx = tx.clone();
        let registry = registry.clone();
        thread::spawn(move || {
            if let Err(error) = serve_connection(stream, config, tx, registry) {
                eprintln!("custody-peer-session-ended: {error}");
            }
            connections.fetch_sub(1, Ordering::AcqRel);
        });
    }
    drop(tx);
    actor
        .join()
        .map_err(|_| io::Error::other("custody terminal panicked"))?
}

pub(super) fn inspect(config: Config) -> io::Result<()> {
    use sha2::{Digest, Sha256};
    let terminal = Terminal::open(config)?;
    let mut arena = Arena::new(MAX_PAYLOAD)?;
    let mut hash = Sha256::new();
    for offset in (0..terminal.config.logical_bytes).step_by(MAX_PAYLOAD) {
        let bytes = (terminal.config.logical_bytes - offset).min(MAX_PAYLOAD as u64) as usize;
        terminal
            .wal
            .read_at(offset, &mut arena.bytes_mut()[..bytes])?;
        hash.update(&arena.bytes()[..bytes]);
    }
    println!(
        "custody-image: replica={} incarnation={} hwm={} sha256={:x} bytes={} generation={}",
        terminal.config.owner.id,
        terminal.config.owner.incarnation,
        terminal.hwm(),
        hash.finalize(),
        terminal.config.logical_bytes,
        terminal.identity.generation
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let base = env::var_os("ZC_CLIENT_WAL_TEST_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(env::temp_dir);
            let path = base.join(format!(
                "zc-live-custody-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn config(&self, staged: bool) -> Config {
            Config {
                scope: CommitScope {
                    volume: 17,
                    log: 34,
                    writer_epoch: 1,
                    lane: 0,
                },
                owner: Replica {
                    id: 3,
                    incarnation: 1,
                    failure_domain: 3,
                },
                listen: "127.0.0.1:0".parse().unwrap(),
                directory: self.0.clone(),
                logical_bytes: 8192,
                journal_bytes: 1024 * 1024,
                downstream: None,
                allow_plaintext_private_network: true,
                requires_copy: staged,
            }
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn arena(value: u8) -> Arc<Arena> {
        let mut arena = Arena::new(MAX_PAYLOAD).unwrap();
        arena.bytes_mut().fill(value);
        Arc::new(arena)
    }
    fn request(config: &Config, op: u64, generation: u64, sequence: u64) -> Header {
        Header {
            length: if matches!(op, READ | WRITE | COPY_RANGE | PATCH_PAGES) {
                4096
            } else {
                0
            },
            ..Header::request(config.scope, op, generation, sequence)
        }
    }
    fn base_copy(terminal: &mut Terminal, config: &Config, at: u64) {
        terminal
            .execute(request(config, COPY, 2, at), arena(0), &[])
            .unwrap();
        terminal
            .execute(request(config, COPY_RANGE, 2, at), arena(0x11), &[])
            .unwrap();
        assert!(
            terminal
                .execute(request(config, COPY_DONE, 2, at), arena(0), &[])
                .is_err()
        );
        terminal
            .execute(
                Header {
                    offset: 4096,
                    ..request(config, COPY_RANGE, 2, at)
                },
                arena(0x22),
                &[],
            )
            .unwrap();
        terminal
            .execute(request(config, COPY_DONE, 2, at), arena(0), &[])
            .unwrap();
    }

    #[test]
    fn persisted_generation_fences_old_middle_and_duplicate_writes_cannot_roll_back() {
        let path = TestDir::new();
        let config = path.config(false);
        let mut terminal = Terminal::open(config.clone()).unwrap();
        assert_eq!(
            terminal
                .execute(request(&config, WRITE, 1, 1), arena(0x11), &[])
                .unwrap()
                .header
                .sequence,
            1
        );
        terminal
            .execute(request(&config, FENCE, 2, 0), arena(0), &[])
            .unwrap();
        assert!(
            terminal
                .execute(request(&config, WRITE, 1, 2), arena(0x99), &[])
                .is_err()
        );
        assert!(
            terminal
                .execute(request(&config, WRITE, 2, 3), arena(0x99), &[])
                .is_err()
        );
        terminal
            .execute(request(&config, WRITE, 2, 2), arena(0x22), &[])
            .unwrap();
        terminal
            .execute(request(&config, WRITE, 2, 1), arena(0x11), &[])
            .unwrap();
        drop(terminal);
        let mut terminal = Terminal::open(config.clone()).unwrap();
        assert_eq!(terminal.hwm(), 2);
        assert!(
            terminal
                .execute(request(&config, FENCE, 1, 0), arena(0), &[])
                .is_err()
        );
        assert!(
            terminal
                .execute(request(&config, WRITE, 1, 3), arena(0x99), &[])
                .is_err()
        );
        let read = terminal
            .execute(request(&config, READ, 2, 0), arena(0), &[])
            .unwrap();
        assert!(
            read.payload.unwrap().bytes()[..4096]
                .iter()
                .all(|b| *b == 0x22)
        );
        drop(terminal);
        let mut wrong = config;
        wrong.owner.incarnation += 1;
        assert!(Terminal::open(wrong).is_err());
    }

    #[test]
    fn staged_copy_and_compacted_tail_require_complete_durable_activation() {
        let path = TestDir::new();
        let config = path.config(true);
        let mut terminal = Terminal::open(config.clone()).unwrap();
        assert!(
            terminal
                .execute(request(&config, WRITE, 1, 1), arena(1), &[])
                .is_err()
        );
        assert!(
            terminal
                .execute(request(&config, READ, 1, 0), arena(0), &[])
                .is_err()
        );
        base_copy(&mut terminal, &config, 8);
        assert_eq!(terminal.hwm(), 8);
        assert!(
            terminal
                .execute(request(&config, WRITE, 2, 9), arena(1), &[])
                .is_err()
        );
        terminal
            .execute(
                Header {
                    offset: 8,
                    ..request(&config, PATCH, 2, 100)
                },
                arena(0),
                &[],
            )
            .unwrap();
        // Ninety-two overwrites of page zero collapse to its final version.
        terminal
            .execute(request(&config, PATCH_PAGES, 2, 100), arena(0x33), &[0])
            .unwrap();
        assert!(
            terminal
                .execute(request(&config, ACTIVATE, 2, 100), arena(0), &[])
                .is_err()
        );
        terminal
            .execute(request(&config, PATCH_DONE, 2, 100), arena(0), &[])
            .unwrap();
        assert!(
            terminal
                .execute(request(&config, ACTIVATE, 2, 99), arena(0), &[])
                .is_err()
        );
        terminal
            .execute(request(&config, ACTIVATE, 2, 100), arena(0), &[])
            .unwrap();
        assert!(
            terminal
                .execute(
                    Header {
                        offset: 100,
                        ..request(&config, PATCH, 2, 101)
                    },
                    arena(0),
                    &[]
                )
                .is_err()
        );
        drop(terminal);
        let mut terminal = Terminal::open(config.clone()).unwrap();
        assert_eq!(terminal.hwm(), 100);
        for (offset, expected) in [(0, 0x33), (4096, 0x22)] {
            let read = terminal
                .execute(
                    Header {
                        offset,
                        ..request(&config, READ, 2, 0)
                    },
                    arena(0),
                    &[],
                )
                .unwrap();
            assert!(
                read.payload.unwrap().bytes()[..4096]
                    .iter()
                    .all(|b| *b == expected)
            );
        }
        assert_eq!(
            terminal
                .execute(request(&config, WRITE, 2, 101), arena(0x44), &[])
                .unwrap()
                .header
                .sequence,
            101
        );
    }

    #[test]
    fn restart_during_state_patch_stays_fenced_and_uncountable() {
        let path = TestDir::new();
        let config = path.config(true);
        let mut terminal = Terminal::open(config.clone()).unwrap();
        base_copy(&mut terminal, &config, 8);
        terminal
            .execute(
                Header {
                    offset: 8,
                    ..request(&config, PATCH, 2, 12)
                },
                arena(0),
                &[],
            )
            .unwrap();
        terminal
            .execute(request(&config, PATCH_PAGES, 2, 12), arena(0x33), &[0])
            .unwrap();
        drop(terminal);
        let mut terminal = Terminal::open(config.clone()).unwrap();
        assert!(
            terminal
                .execute(request(&config, ACTIVATE, 2, 12), arena(0), &[])
                .is_err()
        );
        assert!(
            terminal
                .execute(request(&config, PATCH_DONE, 2, 12), arena(0), &[])
                .is_err()
        );
        assert!(
            terminal
                .execute(request(&config, WRITE, 2, 13), arena(0), &[])
                .is_err()
        );
    }

    #[test]
    fn failed_persistent_fence_cannot_be_followed_by_an_acknowledged_write() {
        let path = TestDir::new();
        let config = path.config(false);
        let mut terminal = Terminal::open(config.clone()).unwrap();
        fs::create_dir(path.0.join("identity.next")).unwrap();
        assert!(
            terminal
                .execute(request(&config, FENCE, 2, 0), arena(0), &[])
                .is_err()
        );
        assert!(terminal.failed);
        assert!(
            terminal
                .execute(request(&config, WRITE, 2, 1), arena(0x44), &[])
                .is_err()
        );
        assert_eq!(terminal.wal.stats().appended_sequence, 0);
    }

    #[test]
    fn wire_bounds_scope_and_incarnation_fail_closed() {
        let path = TestDir::new();
        let config = path.config(false);
        let header = request(&config, WRITE, 1, 1);
        assert_eq!(
            Header::decode(&header.encode(), config.scope)
                .unwrap()
                .length,
            4096
        );
        let mut foreign = config.scope;
        foreign.writer_epoch += 1;
        assert!(Header::decode(&header.encode(), foreign).is_err());
        assert!(
            Header::decode(
                &Header {
                    length: MAX_PAYLOAD as u64 + 4096,
                    ..header
                }
                .encode(),
                config.scope
            )
            .is_err()
        );
        let mut reserved = header.encode();
        reserved[127] = 1;
        assert!(Header::decode(&reserved, config.scope).is_err());
        let ack = Header {
            op: ACK,
            replica: config.owner.id,
            incarnation: config.owner.incarnation + 1,
            ..header
        };
        assert!(ack.validate_ack(config.owner, 1).is_err());
        let mut private_required = config;
        private_required.allow_plaintext_private_network = false;
        assert!(Terminal::open(private_required).is_err());
    }

    #[test]
    fn cold_fencing_retires_partitioned_old_sessions_without_a_hotpath_registry_lock() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut old = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        old.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let (server, _) = listener.accept().unwrap();
        let registry = Arc::new(SessionRegistry::default());
        let guard = registry.register(&server, 1).unwrap();
        registry.fence(2);
        assert_eq!(old.read(&mut [0; 1]).unwrap(), 0);
        assert!(registry.register(&server, 1).is_err());
        drop(guard);
        assert!(registry.state.lock().unwrap().2.is_empty());
    }
}
