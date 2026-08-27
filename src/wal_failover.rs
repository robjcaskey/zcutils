//! Userspace custody-transfer stage for a mirrored WAL volume.
//!
//! This sits after the `/dev/zcnblk0` client edge and before terminal WAL
//! leaves.  The kernel client never chooses a replica.  Before promotion,
//! writes and syncs are sent to both regional leaves and reads use the active
//! leaf.  Promotion is admitted only at a fully mirrored sync high-water mark;
//! it then excludes the old primary from all subsequent traffic.

use crate::{
    ZCNBLK_FAN_WAL_HEADER_LEN, ZCNBLK_FAN_WAL_OP_EOF, ZCNBLK_FAN_WAL_OP_HELLO,
    ZCNBLK_FAN_WAL_OP_HELLO_ACK, ZCNBLK_FAN_WAL_OP_READ_DESC, ZCNBLK_FAN_WAL_OP_REQUEST_BATCH,
    ZCNBLK_FAN_WAL_OP_RESULT, ZCNBLK_FAN_WAL_OP_RESULT_BATCH, ZCNBLK_FAN_WAL_OP_SYNC,
    ZCNBLK_FAN_WAL_OP_WRITE_BATCH, ZCNBLK_FAN_WAL_OP_WRITE_DESC,
    ZCNBLK_FAN_WAL_OP_WRITE_EXTENT_BATCH, ZcOfiMessageStream, ZcnblkFanWalFrame,
    pin_current_thread_for_lane,
};
use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const PRIMARY_MASK: u8 = 1;
const SECONDARY_MASK: u8 = 2;
const BOTH_MASK: u8 = PRIMARY_MASK | SECONDARY_MASK;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WalTransportKind {
    Tcp,
    OfiRdm,
}

impl WalTransportKind {
    fn parse(value: &str, variable: &str) -> io::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "tcp" | "tcp-mux" | "tcpmux" => Ok(Self::Tcp),
            "ofi" | "ofi-rdm" | "rdm" | "efa" => Ok(Self::OfiRdm),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{variable} must be tcp or ofi, got {other:?}"),
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::OfiRdm => "ofi-rdm",
        }
    }
}

#[derive(Clone, Debug)]
struct WalTransport {
    kind: WalTransportKind,
    provider: String,
    endpoint: String,
    domains: Vec<String>,
}

impl WalTransport {
    fn from_env(prefix: &str, default: WalTransportKind) -> io::Result<Self> {
        let kind_name = format!("{prefix}_TRANSPORT");
        let kind = env::var(&kind_name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| WalTransportKind::parse(&value, &kind_name))
            .transpose()?
            .unwrap_or(default);
        let provider =
            env::var(format!("{prefix}_OFI_PROVIDER")).unwrap_or_else(|_| "efa".to_string());
        let endpoint =
            env::var(format!("{prefix}_OFI_ENDPOINT")).unwrap_or_else(|_| "rdm".to_string());
        let domains = env::var(format!("{prefix}_OFI_DOMAINS"))
            .ok()
            .into_iter()
            .flat_map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        if kind == WalTransportKind::OfiRdm && provider.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{prefix}_OFI_PROVIDER must not be empty"),
            ));
        }
        if kind == WalTransportKind::OfiRdm && endpoint != "rdm" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{prefix}_OFI_ENDPOINT must be rdm for the WAL mirror"),
            ));
        }
        Ok(Self {
            kind,
            provider,
            endpoint,
            domains,
        })
    }

    fn lane_domain(&self, lane: u32) -> io::Result<Option<&str>> {
        if self.domains.is_empty() {
            return Ok(None);
        }
        if self.domains.len() == 1 {
            return Ok(Some(self.domains[0].as_str()));
        }
        let lane = usize::try_from(lane)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "lane overflow"))?;
        self.domains
            .get(lane)
            .map(String::as_str)
            .map(Some)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "OFI domain list has {} entries but lane {lane} was requested",
                        self.domains.len()
                    ),
                )
            })
    }
}

enum WalStream {
    Tcp(TcpStream),
    Ofi(ZcOfiMessageStream),
}

impl WalStream {
    fn tcp(&self) -> Option<&TcpStream> {
        match self {
            Self::Tcp(stream) => Some(stream),
            Self::Ofi(_) => None,
        }
    }

    fn configure_low_latency(&self) -> io::Result<()> {
        if let Self::Tcp(stream) = self {
            stream.set_nodelay(true)?;
        }
        Ok(())
    }
}

impl Read for WalStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(out),
            Self::Ofi(stream) => stream.read(out),
        }
    }
}

impl Write for WalStream {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(input),
            Self::Ofi(stream) => stream.write(input),
        }
    }

    fn write_vectored(&mut self, inputs: &[std::io::IoSlice<'_>]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write_vectored(inputs),
            Self::Ofi(stream) => stream.write_vectored(inputs),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            Self::Ofi(stream) => stream.flush(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplicationMode {
    Synchronous,
    Asynchronous,
}

impl ReplicationMode {
    fn from_env() -> io::Result<Self> {
        match env::var("ZCNBLK_WAL_FAILOVER_MODE") {
            Ok(value) => match value.as_str() {
                "async" | "asynchronous" => Ok(Self::Asynchronous),
                "sync" | "synchronous" => Ok(Self::Synchronous),
                other => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid ZCNBLK_WAL_FAILOVER_MODE {other:?}; use sync or async"),
                )),
            },
            Err(env::VarError::NotPresent) => Ok(Self::Synchronous),
            Err(error) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                error.to_string(),
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Synchronous => "sync",
            Self::Asynchronous => "async",
        }
    }
}

#[derive(Debug)]
struct FailoverState {
    /// Serializes every admitted WAL operation against a custody change.
    custody_fence: RwLock<()>,
    active: AtomicU8,
    write_mask: AtomicU8,
    placement_epoch: AtomicU64,
    write_generation: AtomicU64,
    synced_generation: AtomicU64,
    sync_epoch: AtomicU64,
    mode: ReplicationMode,
    secondary_write_generation: AtomicU64,
    secondary_synced_generation: AtomicU64,
    accepted_loss_generation: AtomicU64,
    replication_paused: AtomicBool,
    replication_failed: AtomicBool,
    replication_progress: (Mutex<u64>, Condvar),
    /// Optional source-region ingress address whose established sessions must
    /// be synchronously kicked by an explicit declared-loss promotion.
    fence_source_ip: Option<IpAddr>,
    next_session_id: AtomicU64,
    sessions: Mutex<Vec<RegisteredSocket>>,
}

#[derive(Debug)]
struct RegisteredSocket {
    id: u64,
    peer_ip: IpAddr,
    stream: TcpStream,
}

struct SessionRegistration {
    id: u64,
    state: Arc<FailoverState>,
}

impl Drop for SessionRegistration {
    fn drop(&mut self) {
        if let Ok(mut sessions) = self.state.sessions.lock() {
            sessions.retain(|session| session.id != self.id);
        }
    }
}

impl FailoverState {
    fn new() -> Self {
        Self::with_mode(ReplicationMode::Synchronous)
    }

    fn with_mode(mode: ReplicationMode) -> Self {
        Self::with_mode_and_fence(mode, None)
    }

    fn with_mode_and_fence(mode: ReplicationMode, fence_source_ip: Option<IpAddr>) -> Self {
        Self {
            custody_fence: RwLock::new(()),
            active: AtomicU8::new(PRIMARY_MASK),
            write_mask: AtomicU8::new(BOTH_MASK),
            placement_epoch: AtomicU64::new(1),
            write_generation: AtomicU64::new(0),
            synced_generation: AtomicU64::new(0),
            sync_epoch: AtomicU64::new(0),
            mode,
            secondary_write_generation: AtomicU64::new(0),
            secondary_synced_generation: AtomicU64::new(0),
            accepted_loss_generation: AtomicU64::new(u64::MAX),
            replication_paused: AtomicBool::new(false),
            replication_failed: AtomicBool::new(false),
            replication_progress: (Mutex::new(0), Condvar::new()),
            fence_source_ip,
            next_session_id: AtomicU64::new(1),
            sessions: Mutex::new(Vec::new()),
        }
    }

    fn register_session(self: &Arc<Self>, stream: &TcpStream) -> io::Result<SessionRegistration> {
        let peer_ip = stream.peer_addr()?.ip();
        if self.fence_source_ip == Some(peer_ip)
            && self.accepted_loss_generation.load(Ordering::Acquire) != u64::MAX
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "source peer {peer_ip} is fenced after declared-loss promotion; repair and rejoin are required"
                ),
            ));
        }
        let id = self.next_session_id.fetch_add(1, Ordering::AcqRel);
        self.sessions
            .lock()
            .map_err(|_| io::Error::other("session registry poisoned"))?
            .push(RegisteredSocket {
                id,
                peer_ip,
                stream: stream.try_clone()?,
            });
        Ok(SessionRegistration {
            id,
            state: Arc::clone(self),
        })
    }

    fn fence_source_sessions(&self) -> usize {
        let Some(source_ip) = self.fence_source_ip else {
            return 0;
        };
        let Ok(sessions) = self.sessions.lock() else {
            return 0;
        };
        let mut fenced = 0;
        for session in sessions
            .iter()
            .filter(|session| session.peer_ip == source_ip)
        {
            let _ = session.stream.shutdown(Shutdown::Both);
            fenced += 1;
        }
        fenced
    }

    fn active_label(&self) -> &'static str {
        match self.active.load(Ordering::Acquire) {
            PRIMARY_MASK => "primary",
            SECONDARY_MASK => "secondary",
            _ => "invalid",
        }
    }

    fn status(&self) -> String {
        format!(
            "active={} placement_epoch={} replication_mode={} write_generation={} synced_generation={} secondary_write_generation={} secondary_synced_generation={} sync_epoch={} write_mask=0x{:x} replication_paused={} replication_failed={} accepted_loss_generation={}",
            self.active_label(),
            self.placement_epoch.load(Ordering::Acquire),
            self.mode.label(),
            self.write_generation.load(Ordering::Acquire),
            self.synced_generation.load(Ordering::Acquire),
            self.secondary_write_generation.load(Ordering::Acquire),
            self.secondary_synced_generation.load(Ordering::Acquire),
            self.sync_epoch.load(Ordering::Acquire),
            self.write_mask.load(Ordering::Acquire),
            self.replication_paused.load(Ordering::Acquire),
            self.replication_failed.load(Ordering::Acquire),
            self.accepted_loss_generation.load(Ordering::Acquire),
        )
    }

    fn promote_secondary(&self) -> io::Result<String> {
        let _fence = self
            .custody_fence
            .write()
            .map_err(|_| io::Error::other("custody fence poisoned"))?;
        if self.active.load(Ordering::Acquire) == SECONDARY_MASK {
            return Ok(format!("OK {} already_active=true", self.status()));
        }
        let writes = self.write_generation.load(Ordering::Acquire);
        let synced = self.synced_generation.load(Ordering::Acquire);
        if writes != synced {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "promotion requires a fully mirrored sync fence: write_generation={writes} synced_generation={synced}"
                ),
            ));
        }
        if self.mode == ReplicationMode::Asynchronous {
            if self.replication_failed.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "asynchronous replication leg failed before clean promotion",
                ));
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            let (progress_lock, progress_changed) = &self.replication_progress;
            let mut observed = progress_lock
                .lock()
                .map_err(|_| io::Error::other("replication progress lock poisoned"))?;
            while self.secondary_synced_generation.load(Ordering::Acquire) < synced {
                let now = Instant::now();
                if now >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "clean promotion timed out waiting for asynchronous replica: required={synced} replicated={}",
                            self.secondary_synced_generation.load(Ordering::Acquire)
                        ),
                    ));
                }
                let (next, _) = progress_changed
                    .wait_timeout(observed, deadline - now)
                    .map_err(|_| io::Error::other("replication progress lock poisoned"))?;
                observed = next;
            }
        }

        // Publish the new reader before excluding the old writer.  The write
        // lock means no request can observe the intermediate state.
        self.active.store(SECONDARY_MASK, Ordering::Release);
        self.write_mask.store(SECONDARY_MASK, Ordering::Release);
        self.placement_epoch.fetch_add(1, Ordering::AcqRel);
        let status = self.status();
        eprintln!(
            "zcnblk-wal-failover-custody-transfer: from=primary to=secondary {status} fence={} loss=none",
            if self.mode == ReplicationMode::Asynchronous {
                "async-replica-caught-up-to-source-sync-hwm"
            } else {
                "fully-mirrored-sync-hwm"
            }
        );
        Ok(format!("OK {status} already_active=false"))
    }

    fn promote_secondary_accept_loss(&self, generation: u64, reason: &str) -> io::Result<String> {
        let _fence = self
            .custody_fence
            .write()
            .map_err(|_| io::Error::other("custody fence poisoned"))?;
        if self.mode != ReplicationMode::Asynchronous {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "declared-loss promotion requires asynchronous replication mode",
            ));
        }
        if reason.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "declared-loss promotion requires a reason",
            ));
        }
        let replicated = self.secondary_synced_generation.load(Ordering::Acquire);
        if generation != replicated {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "accepted generation must equal the remote durable sync HWM: requested={generation} replicated={replicated}"
                ),
            ));
        }
        let source = self.synced_generation.load(Ordering::Acquire);
        if generation > source {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "accepted generation is ahead of the last source sync HWM",
            ));
        }
        self.accepted_loss_generation
            .store(generation, Ordering::Release);
        self.replication_paused.store(false, Ordering::Release);
        self.active.store(SECONDARY_MASK, Ordering::Release);
        self.write_mask.store(SECONDARY_MASK, Ordering::Release);
        self.placement_epoch.fetch_add(1, Ordering::AcqRel);
        self.replication_progress.1.notify_all();
        // A declared-loss cut is a new history, not transparent path
        // migration.  Kick every source-region session before returning the
        // committed decision, and reject that peer until it is repaired and
        // explicitly rejoined.  This cannot rely on TCP detecting a vanished
        // region: a dead virtual or physical fabric may produce no FIN/RST.
        let fenced_sessions = self.fence_source_sessions();
        let first_missing = (generation < source).then_some(generation + 1);
        let last_missing = (generation < source).then_some(source);
        let status = self.status();
        eprintln!(
            "zcnblk-wal-failover-custody-transfer: from=primary to=secondary {status} fence=declared-loss accepted_generation={generation} source_sync_generation={source} first_missing={first_missing:?} last_missing={last_missing:?} fenced_source_sessions={fenced_sessions} reason={reason:?}"
        );
        Ok(format!(
            "OK {status} already_active=false declared_loss=true source_sync_generation={source} first_missing={first_missing:?} last_missing={last_missing:?} fenced_source_sessions={fenced_sessions}"
        ))
    }
}

struct ReplicationRequest {
    frame: ZcnblkFanWalFrame,
    payload: Vec<u8>,
    generation: u64,
    write: bool,
}

#[derive(Clone)]
pub(crate) struct Endpoint {
    pub(crate) host: String,
    pub(crate) base_port: u16,
}

impl Endpoint {
    pub(crate) fn parse(value: &str) -> io::Result<Self> {
        let socket = resolve_one(value)?;
        Ok(Self {
            host: socket.ip().to_string(),
            base_port: socket.port(),
        })
    }

    pub(crate) fn lane_addr(&self, lane: u32) -> io::Result<SocketAddr> {
        let offset = u16::try_from(lane)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "WAL lane exceeds u16"))?;
        let port = self
            .base_port
            .checked_add(offset)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WAL lane port overflow"))?;
        resolve_one(&format!("{}:{port}", self.host))
    }
}

fn resolve_one(value: &str) -> io::Result<SocketAddr> {
    value
        .to_socket_addrs()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid socket address {value:?}: {error}"),
            )
        })?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, value.to_string()))
}

pub(crate) fn read_frame<R: Read + ?Sized>(
    stream: &mut R,
) -> io::Result<(ZcnblkFanWalFrame, Vec<u8>)> {
    let frame = read_frame_header(stream)?;
    let mut payload = vec![0u8; wire_payload_len(frame)];
    stream.read_exact(&mut payload)?;
    Ok((frame, payload))
}

/// Read only the fixed WAL header. Bulk migration uses this to validate a
/// source read result before splicing its payload directly into a destination
/// write socket, without materializing the payload in userspace.
pub(crate) fn read_frame_header<R: Read + ?Sized>(stream: &mut R) -> io::Result<ZcnblkFanWalFrame> {
    let mut header = [0u8; ZCNBLK_FAN_WAL_HEADER_LEN];
    stream.read_exact(&mut header)?;
    ZcnblkFanWalFrame::decode(&header)
}

pub(crate) fn write_frame<W: Write + ?Sized>(
    stream: &mut W,
    frame: ZcnblkFanWalFrame,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() != wire_payload_len(frame) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WAL frame payload length mismatch",
        ));
    }
    stream.write_all(&frame.encode())?;
    stream.write_all(payload)
}

fn wire_payload_len(frame: ZcnblkFanWalFrame) -> usize {
    match frame.op {
        ZCNBLK_FAN_WAL_OP_WRITE_DESC
        | ZCNBLK_FAN_WAL_OP_RESULT
        | ZCNBLK_FAN_WAL_OP_WRITE_BATCH
        | ZCNBLK_FAN_WAL_OP_RESULT_BATCH
        | ZCNBLK_FAN_WAL_OP_REQUEST_BATCH
        | ZCNBLK_FAN_WAL_OP_WRITE_EXTENT_BATCH => frame.payload_len as usize,
        _ => 0,
    }
}

fn request_batch_contains_write(frame: ZcnblkFanWalFrame, payload: &[u8]) -> io::Result<bool> {
    let records = frame.segment_count as usize;
    let descriptor_bytes = records
        .checked_mul(ZCNBLK_FAN_WAL_HEADER_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "descriptor overflow"))?;
    if payload.len() < descriptor_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request batch is shorter than its descriptor table",
        ));
    }
    for descriptor in payload[..descriptor_bytes].chunks_exact(ZCNBLK_FAN_WAL_HEADER_LEN) {
        let header: &[u8; ZCNBLK_FAN_WAL_HEADER_LEN] = descriptor
            .try_into()
            .expect("chunks_exact yields a complete WAL header");
        if ZcnblkFanWalFrame::decode(header)?.op == ZCNBLK_FAN_WAL_OP_WRITE_DESC {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn is_write(frame: ZcnblkFanWalFrame, payload: &[u8]) -> io::Result<bool> {
    match frame.op {
        ZCNBLK_FAN_WAL_OP_WRITE_DESC
        | ZCNBLK_FAN_WAL_OP_WRITE_BATCH
        | ZCNBLK_FAN_WAL_OP_WRITE_EXTENT_BATCH => Ok(true),
        ZCNBLK_FAN_WAL_OP_REQUEST_BATCH => request_batch_contains_write(frame, payload),
        _ => Ok(false),
    }
}

fn send_request_and_read_result<S: Read + Write + ?Sized>(
    stream: &mut S,
    frame: ZcnblkFanWalFrame,
    payload: &[u8],
) -> io::Result<(ZcnblkFanWalFrame, Vec<u8>)> {
    write_frame(stream, frame, payload)?;
    read_frame(stream)
}

fn send_mirrored_request_and_read_results<P, S>(
    primary: &mut P,
    secondary: &mut S,
    frame: ZcnblkFanWalFrame,
    payload: &[u8],
) -> io::Result<((ZcnblkFanWalFrame, Vec<u8>), (ZcnblkFanWalFrame, Vec<u8>))>
where
    P: Read + Write + ?Sized,
    S: Read + Write + ?Sized,
{
    // Put both mirror legs in flight before waiting for either result.  The
    // upstream ACK is still withheld until both replies validate, so this
    // removes serialized network latency without weakening mirror semantics.
    write_frame(primary, frame, payload)?;
    write_frame(secondary, frame, payload)?;
    let primary_result = read_frame(primary)?;
    let secondary_result = read_frame(secondary)?;
    Ok((primary_result, secondary_result))
}

pub(crate) fn connect_leaf(endpoint: &Endpoint, lane: u32) -> io::Result<TcpStream> {
    let address = endpoint.lane_addr(lane)?;
    let retry_ms = env::var("ZCNBLK_WAL_FAILOVER_CONNECT_RETRY_MS")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(20_000);
    let started = Instant::now();
    let deadline = Duration::from_millis(retry_ms);
    let mut attempts = 0u64;
    loop {
        attempts += 1;
        match TcpStream::connect(address) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                if attempts != 1 {
                    eprintln!(
                        "zcnblk-wal-failover-leaf-connect: address={address} lane={lane} attempts={attempts} elapsed_ms={} status=connected",
                        started.elapsed().as_millis(),
                    );
                }
                return Ok(stream);
            }
            Err(error) if started.elapsed() < deadline => {
                if attempts == 1 || attempts.is_multiple_of(50) {
                    eprintln!(
                        "zcnblk-wal-failover-leaf-connect: address={address} lane={lane} attempts={attempts} elapsed_ms={} status=waiting error={error}",
                        started.elapsed().as_millis(),
                    );
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

fn connect_transport_leaf(
    endpoint: &Endpoint,
    lane: u32,
    transport: &WalTransport,
) -> io::Result<WalStream> {
    if transport.kind == WalTransportKind::Tcp {
        return connect_leaf(endpoint, lane).map(WalStream::Tcp);
    }
    let address = endpoint.lane_addr(lane)?;
    let started = Instant::now();
    let retry_ms = env::var("ZCNBLK_WAL_FAILOVER_CONNECT_RETRY_MS")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(20_000);
    loop {
        match ZcOfiMessageStream::connect_on_domain(
            &transport.provider,
            &transport.endpoint,
            &address.ip().to_string(),
            address.port(),
            false,
            false,
            transport.lane_domain(lane)?,
        ) {
            Ok(stream) => return Ok(WalStream::Ofi(stream)),
            Err(error) if started.elapsed() < Duration::from_millis(retry_ms) => {
                eprintln!(
                    "zcnblk-wal-failover-leaf-connect: transport=ofi-rdm provider={} endpoint={} address={address} lane={lane} elapsed_ms={} status=waiting error={error}",
                    transport.provider,
                    transport.endpoint,
                    started.elapsed().as_millis(),
                );
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn validate_mirror_results(
    primary: &(ZcnblkFanWalFrame, Vec<u8>),
    secondary: &(ZcnblkFanWalFrame, Vec<u8>),
) -> io::Result<()> {
    let a = primary.0;
    let b = secondary.0;
    // Preferred worker/queue/CPU/NUMA fields describe the terminal leaf that
    // produced the result.  They are expected to differ across regions and
    // are not part of replica-content equality.
    let contract_equal = mirror_frame_contract_equal(a, b);
    let payload_equal = mirror_payloads_equal(a, &primary.1, &secondary.1)?;
    if !contract_equal || !payload_equal {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "regional WAL replicas diverged: primary_op={} secondary_op={} primary_status={} secondary_status={} primary_sequence={} secondary_sequence={} primary_sync_epoch={} secondary_sync_epoch={} primary_placement_epoch={} secondary_placement_epoch={} primary_payload={} secondary_payload={}",
                primary.0.op,
                secondary.0.op,
                primary.0.status,
                secondary.0.status,
                primary.0.sequence,
                secondary.0.sequence,
                primary.0.sync_epoch,
                secondary.0.sync_epoch,
                primary.0.placement_epoch,
                secondary.0.placement_epoch,
                primary.1.len(),
                secondary.1.len(),
            ),
        ));
    }
    Ok(())
}

fn mirror_frame_contract_equal(a: ZcnblkFanWalFrame, b: ZcnblkFanWalFrame) -> bool {
    a.op == b.op
        && a.flags == b.flags
        && a.status == b.status
        && a.lane_id == b.lane_id
        && a.lane_count == b.lane_count
        && a.branch_id == b.branch_id
        && a.branch_count == b.branch_count
        && a.segment_index == b.segment_index
        && a.segment_count == b.segment_count
        && a.payload_len == b.payload_len
        && a.sequence == b.sequence
        && a.request_id == b.request_id
        && a.sync_epoch == b.sync_epoch
        && a.placement_epoch == b.placement_epoch
        && a.logical_offset == b.logical_offset
        && a.leaf_offset == b.leaf_offset
        && a.logical_len == b.logical_len
        && a.zcnblk_op == b.zcnblk_op
        && a.zcnblk_flags == b.zcnblk_flags
        && a.zcnblk_shard == b.zcnblk_shard
}

fn mirror_payloads_equal(
    frame: ZcnblkFanWalFrame,
    primary: &[u8],
    secondary: &[u8],
) -> io::Result<bool> {
    if frame.op != ZCNBLK_FAN_WAL_OP_RESULT_BATCH {
        return Ok(primary == secondary);
    }
    let descriptor_bytes = (frame.segment_count as usize)
        .checked_mul(ZCNBLK_FAN_WAL_HEADER_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "result descriptor overflow"))?;
    if primary.len() < descriptor_bytes || secondary.len() < descriptor_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "result batch is shorter than its descriptor table",
        ));
    }
    if primary.len() != secondary.len()
        || primary[descriptor_bytes..] != secondary[descriptor_bytes..]
    {
        return Ok(false);
    }
    for (left, right) in primary[..descriptor_bytes]
        .chunks_exact(ZCNBLK_FAN_WAL_HEADER_LEN)
        .zip(secondary[..descriptor_bytes].chunks_exact(ZCNBLK_FAN_WAL_HEADER_LEN))
    {
        let left: &[u8; ZCNBLK_FAN_WAL_HEADER_LEN] = left.try_into().expect("exact descriptor");
        let right: &[u8; ZCNBLK_FAN_WAL_HEADER_LEN] = right.try_into().expect("exact descriptor");
        if !mirror_frame_contract_equal(
            ZcnblkFanWalFrame::decode(left)?,
            ZcnblkFanWalFrame::decode(right)?,
        ) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn async_replication_worker(
    mut secondary: WalStream,
    requests: mpsc::Receiver<ReplicationRequest>,
    state: Arc<FailoverState>,
    lane: u32,
) {
    let delay = env::var("ZCNBLK_WAL_ASYNC_REPLICATION_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_default();
    while let Ok(request) = requests.recv() {
        while state.replication_paused.load(Ordering::Acquire)
            && state.accepted_loss_generation.load(Ordering::Acquire) == u64::MAX
        {
            let guard = match state.replication_progress.0.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            let _ = state
                .replication_progress
                .1
                .wait_timeout(guard, Duration::from_millis(100));
        }
        let accepted = state.accepted_loss_generation.load(Ordering::Acquire);
        if accepted != u64::MAX && request.generation > accepted {
            eprintln!(
                "zcnblk-wal-failover-async-drop: lane={lane} generation={} accepted_loss_generation={accepted} op={} reason=declared-loss-tail-excluded",
                request.generation, request.frame.op,
            );
            continue;
        }
        if !delay.is_zero() {
            thread::sleep(delay);
        }
        if let Err(error) =
            send_request_and_read_result(&mut secondary, request.frame, &request.payload)
        {
            state.replication_failed.store(true, Ordering::Release);
            state.replication_progress.1.notify_all();
            eprintln!(
                "zcnblk-wal-failover-async-replication-error: lane={lane} generation={} op={} error={error}",
                request.generation, request.frame.op,
            );
            return;
        }
        if request.write {
            state
                .secondary_write_generation
                .fetch_max(request.generation, Ordering::AcqRel);
        }
        if request.frame.op == ZCNBLK_FAN_WAL_OP_SYNC {
            state
                .secondary_synced_generation
                .fetch_max(request.generation, Ordering::AcqRel);
        }
        if let Ok(mut progress) = state.replication_progress.0.lock() {
            *progress = progress.saturating_add(1);
            state.replication_progress.1.notify_all();
        }
    }
}

fn proxy_session_async(
    mut upstream: WalStream,
    primary_endpoint: Endpoint,
    secondary_endpoint: Endpoint,
    leaf_transport: WalTransport,
    state: Arc<FailoverState>,
) -> io::Result<()> {
    let (hello, hello_payload) = read_frame(&mut upstream)?;
    if hello.op != ZCNBLK_FAN_WAL_OP_HELLO || !hello_payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WAL async failover ingress requires HELLO as its first frame",
        ));
    }
    let lane = hello.lane_id;
    let mut primary = connect_transport_leaf(&primary_endpoint, lane, &leaf_transport)?;
    let mut replica_stream = connect_transport_leaf(&secondary_endpoint, lane, &leaf_transport)?;
    // Reserve the post-promotion session before any HELLO waits. Terminal
    // leaves accept their declared connection topology before dispatching
    // workers, and the standby keeps replication-stream ownership separate
    // from promoted foreground I/O.
    let mut standby_secondary = connect_transport_leaf(&secondary_endpoint, lane, &leaf_transport)?;
    let primary_hello = send_request_and_read_result(&mut primary, hello, &[])?;
    let secondary_hello = send_request_and_read_result(&mut replica_stream, hello, &[])?;
    let standby_hello = send_request_and_read_result(&mut standby_secondary, hello, &[])?;
    if primary_hello.0.op != ZCNBLK_FAN_WAL_OP_HELLO_ACK
        || secondary_hello.0.op != ZCNBLK_FAN_WAL_OP_HELLO_ACK
        || standby_hello.0.op != ZCNBLK_FAN_WAL_OP_HELLO_ACK
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "regional WAL leaf omitted HELLO_ACK",
        ));
    }
    write_frame(&mut upstream, primary_hello.0, &primary_hello.1)?;
    let queue_depth = env::var("ZCNBLK_WAL_ASYNC_QUEUE_DEPTH")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(1024)
        .max(1);
    let (replica_tx, replica_rx) = mpsc::sync_channel(queue_depth);
    let replica_state = Arc::clone(&state);
    thread::Builder::new()
        .name(format!("zcwal-async-replica-{lane}"))
        .spawn(move || async_replication_worker(replica_stream, replica_rx, replica_state, lane))?;
    let mut promoted_secondary = Some(standby_secondary);
    eprintln!(
        "zcnblk-wal-failover-session: lane={lane} primary={} secondary={} placement_owner=userspace write_policy=regional-sync-plus-cross-region-async read_policy={} async_queue_depth={queue_depth} {}",
        primary_endpoint.lane_addr(lane)?,
        secondary_endpoint.lane_addr(lane)?,
        state.active_label(),
        state.status(),
    );

    loop {
        let (frame, payload) = match read_frame(&mut upstream) {
            Ok(request) => request,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        let _fence = state
            .custody_fence
            .read()
            .map_err(|_| io::Error::other("custody fence poisoned"))?;
        let active = state.active.load(Ordering::Acquire);
        if frame.op == ZCNBLK_FAN_WAL_OP_EOF {
            if active == PRIMARY_MASK {
                write_frame(&mut primary, frame, &payload)?;
            }
            if let Some(stream) = promoted_secondary.as_mut() {
                write_frame(stream, frame, &payload)?;
            }
            drop(replica_tx);
            return Ok(());
        }

        let write = is_write(frame, &payload)?;
        let generation = if write {
            state.write_generation.fetch_add(1, Ordering::AcqRel) + 1
        } else {
            state.write_generation.load(Ordering::Acquire)
        };
        let result = match active {
            PRIMARY_MASK => {
                let result = send_request_and_read_result(&mut primary, frame, &payload)?;
                if write || frame.op == ZCNBLK_FAN_WAL_OP_SYNC {
                    replica_tx
                        .send(ReplicationRequest {
                            frame,
                            payload,
                            generation,
                            write,
                        })
                        .map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "asynchronous replication worker exited",
                            )
                        })?;
                }
                if frame.op == ZCNBLK_FAN_WAL_OP_SYNC {
                    state.synced_generation.store(generation, Ordering::Release);
                    state.sync_epoch.store(frame.sync_epoch, Ordering::Release);
                }
                result
            }
            SECONDARY_MASK => {
                let result = send_request_and_read_result(
                    promoted_secondary.as_mut().expect("opened secondary"),
                    frame,
                    &payload,
                )?;
                if write {
                    state
                        .secondary_write_generation
                        .store(generation, Ordering::Release);
                }
                if frame.op == ZCNBLK_FAN_WAL_OP_SYNC {
                    state.synced_generation.store(generation, Ordering::Release);
                    state
                        .secondary_synced_generation
                        .store(generation, Ordering::Release);
                    state.sync_epoch.store(frame.sync_epoch, Ordering::Release);
                }
                result
            }
            _ => return Err(io::Error::other("invalid active regional mask")),
        };
        write_frame(&mut upstream, result.0, &result.1)?;
    }
}

fn proxy_session(
    mut upstream: WalStream,
    primary_endpoint: Endpoint,
    secondary_endpoint: Endpoint,
    leaf_transport: WalTransport,
    state: Arc<FailoverState>,
) -> io::Result<()> {
    upstream.configure_low_latency()?;
    let _registration = upstream
        .tcp()
        .map(|stream| state.register_session(stream))
        .transpose()?;
    if state.mode == ReplicationMode::Asynchronous {
        return proxy_session_async(
            upstream,
            primary_endpoint,
            secondary_endpoint,
            leaf_transport,
            state,
        );
    }
    let (hello, hello_payload) = read_frame(&mut upstream)?;
    if hello.op != ZCNBLK_FAN_WAL_OP_HELLO || !hello_payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WAL failover ingress requires HELLO as its first frame",
        ));
    }
    let lane = hello.lane_id;
    let mut primary = connect_transport_leaf(&primary_endpoint, lane, &leaf_transport)?;
    let mut secondary = connect_transport_leaf(&secondary_endpoint, lane, &leaf_transport)?;
    let primary_hello = send_request_and_read_result(&mut primary, hello, &[])?;
    let secondary_hello = send_request_and_read_result(&mut secondary, hello, &[])?;
    if primary_hello.0.op != ZCNBLK_FAN_WAL_OP_HELLO_ACK
        || secondary_hello.0.op != ZCNBLK_FAN_WAL_OP_HELLO_ACK
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "regional WAL leaf omitted HELLO_ACK",
        ));
    }
    validate_mirror_results(&primary_hello, &secondary_hello)?;
    write_frame(&mut upstream, primary_hello.0, &primary_hello.1)?;
    eprintln!(
        "zcnblk-wal-failover-session: lane={lane} primary={} secondary={} placement_owner=userspace write_policy=mirror-until-promoted read_policy={} {}",
        primary_endpoint.lane_addr(lane)?,
        secondary_endpoint.lane_addr(lane)?,
        state.active_label(),
        state.status(),
    );

    loop {
        let (frame, payload) = match read_frame(&mut upstream) {
            Ok(request) => request,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        let _fence = state
            .custody_fence
            .read()
            .map_err(|_| io::Error::other("custody fence poisoned"))?;
        if frame.op == ZCNBLK_FAN_WAL_OP_EOF {
            // EOF closes this ingress session, not a durability epoch.  Close
            // both downstream sessions even after one leg has been excluded
            // from placement so leaf accept loops can retire cleanly.
            write_frame(&mut primary, frame, &payload)?;
            write_frame(&mut secondary, frame, &payload)?;
            return Ok(());
        }

        let write = is_write(frame, &payload)?;
        if write {
            state.write_generation.fetch_add(1, Ordering::AcqRel);
        }
        let mask = if write || frame.op == ZCNBLK_FAN_WAL_OP_SYNC {
            state.write_mask.load(Ordering::Acquire)
        } else if frame.op == ZCNBLK_FAN_WAL_OP_READ_DESC
            || frame.op == ZCNBLK_FAN_WAL_OP_REQUEST_BATCH
        {
            state.active.load(Ordering::Acquire)
        } else {
            state.write_mask.load(Ordering::Acquire)
        };

        let (primary_result, secondary_result) = match mask {
            BOTH_MASK => {
                let (primary_result, secondary_result) = send_mirrored_request_and_read_results(
                    &mut primary,
                    &mut secondary,
                    frame,
                    &payload,
                )?;
                (Some(primary_result), Some(secondary_result))
            }
            PRIMARY_MASK => (
                Some(send_request_and_read_result(&mut primary, frame, &payload)?),
                None,
            ),
            SECONDARY_MASK => (
                None,
                Some(send_request_and_read_result(
                    &mut secondary,
                    frame,
                    &payload,
                )?),
            ),
            _ => return Err(io::Error::other("WAL mirror has no active placement leg")),
        };
        if let (Some(primary_result), Some(secondary_result)) = (&primary_result, &secondary_result)
        {
            validate_mirror_results(primary_result, secondary_result)?;
        }
        let result = match state.active.load(Ordering::Acquire) {
            PRIMARY_MASK => primary_result.as_ref().or(secondary_result.as_ref()),
            SECONDARY_MASK => secondary_result.as_ref().or(primary_result.as_ref()),
            _ => None,
        }
        .ok_or_else(|| io::Error::other("no regional WAL result is available"))?;
        write_frame(&mut upstream, result.0, &result.1)?;

        if frame.op == ZCNBLK_FAN_WAL_OP_SYNC {
            let generation = state.write_generation.load(Ordering::Acquire);
            state.synced_generation.store(generation, Ordering::Release);
            state.sync_epoch.store(frame.sync_epoch, Ordering::Release);
        }
    }
}

fn control_session(stream: TcpStream, state: &FailoverState) -> io::Result<()> {
    let mut command = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut command)?;
    let command = command.trim();
    let response = match command {
        "status" => format!("OK {}", state.status()),
        "secondary" | "promote secondary" => match state.promote_secondary() {
            Ok(response) => response,
            Err(error) => format!("ERR kind={:?} message={error}", error.kind()),
        },
        "primary" | "promote primary" => {
            "ERR kind=Unsupported message=reverse promotion requires replica repair".to_string()
        }
        "pause replication" | "pause" if state.mode == ReplicationMode::Asynchronous => {
            state.replication_paused.store(true, Ordering::Release);
            format!("OK {}", state.status())
        }
        "resume replication" | "resume" if state.mode == ReplicationMode::Asynchronous => {
            state.replication_paused.store(false, Ordering::Release);
            state.replication_progress.1.notify_all();
            format!("OK {}", state.status())
        }
        value if value.starts_with("secondary accept-loss ") => {
            let mut fields = value["secondary accept-loss ".len()..].splitn(2, ' ');
            let generation = fields.next().and_then(|value| value.parse::<u64>().ok());
            let reason = fields.next().unwrap_or_default();
            match generation {
                Some(generation) => match state.promote_secondary_accept_loss(generation, reason) {
                    Ok(response) => response,
                    Err(error) => format!("ERR kind={:?} message={error}", error.kind()),
                },
                None => "ERR kind=InvalidInput message=accept-loss requires GENERATION REASON"
                    .to_string(),
            }
        }
        other => format!(
            "ERR kind=InvalidInput message=unknown command {other:?}; use status, secondary, pause, resume, or secondary accept-loss GENERATION REASON"
        ),
    };
    let mut stream = stream;
    writeln!(stream, "{response}")
}

fn run_control(listener: TcpListener, state: Arc<FailoverState>) -> io::Result<()> {
    for accepted in listener.incoming() {
        let stream = accepted?;
        if let Err(error) = control_session(stream, &state) {
            eprintln!("zcnblk-wal-failover-control-error: {error}");
        }
    }
    Ok(())
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: zcnblk-wal-failover LISTEN_BASE PRIMARY_BASE SECONDARY_BASE CONTROL_ADDR [LANES]",
    )
}

pub fn main_entry() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let listen = Endpoint::parse(&args.next().ok_or_else(usage)?)?;
    let primary = Endpoint::parse(&args.next().ok_or_else(usage)?)?;
    let secondary = Endpoint::parse(&args.next().ok_or_else(usage)?)?;
    let control_addr = resolve_one(&args.next().ok_or_else(usage)?)?;
    let lanes = args
        .next()
        .map(|value| value.parse::<u32>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(1);
    if lanes == 0 || args.next().is_some() {
        return Err(usage());
    }

    let mode = ReplicationMode::from_env()?;
    let ingress_transport =
        WalTransport::from_env("ZCNBLK_WAL_FAILOVER_INGRESS", WalTransportKind::Tcp)?;
    let leaf_transport = WalTransport::from_env("ZCNBLK_WAL_FAILOVER_LEAF", WalTransportKind::Tcp)?;
    if mode == ReplicationMode::Asynchronous && ingress_transport.kind == WalTransportKind::OfiRdm {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "asynchronous declared-loss failover requires a fenceable TCP ingress; OFI ingress fencing is not yet implemented",
        ));
    }
    if env::var("ZCNBLK_WAL_FAILOVER_OFI_RMA_WRITES")
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the WAL mirror does not advertise one-sided RMA payload windows until its userspace fan owns a registered fan-out arena; use OFI/RDM message transport",
        ));
    }
    let fence_source_ip = env::var("ZCNBLK_WAL_FAILOVER_FENCE_SOURCE_IP")
        .ok()
        .map(|value| {
            value.parse::<IpAddr>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid ZCNBLK_WAL_FAILOVER_FENCE_SOURCE_IP {value:?}: {error}"),
                )
            })
        })
        .transpose()?;
    let state = Arc::new(FailoverState::with_mode_and_fence(mode, fence_source_ip));
    let control = TcpListener::bind(control_addr)?;
    let control_state = Arc::clone(&state);
    thread::Builder::new()
        .name("zcwal-failover-control".into())
        .spawn(move || {
            if let Err(error) = run_control(control, control_state) {
                eprintln!("zcnblk-wal-failover-control-fatal: {error}");
            }
        })?;

    println!(
        "zcnblk-wal-failover: listen={}:{} primary={}:{} secondary={}:{} control={} lanes={lanes} ingress_transport={} leaf_transport={} ofi_provider={} ofi_endpoint={} one_sided_rma_payload=disabled-fail-closed topology=client-block-edge->userspace-failover->regional-userspace-leaves placement_owner=userspace block_client_placement=no initial_active=primary replication_mode={} initial_write_policy={} promotion_fence={} declared_loss_fence_source_ip={}",
        listen.host,
        listen.base_port,
        primary.host,
        primary.base_port,
        secondary.host,
        secondary.base_port,
        control_addr,
        ingress_transport.kind.label(),
        leaf_transport.kind.label(),
        leaf_transport.provider,
        leaf_transport.endpoint,
        mode.label(),
        if mode == ReplicationMode::Asynchronous {
            "regional-sync-plus-cross-region-async"
        } else {
            "cross-region-sync-mirror"
        },
        if mode == ReplicationMode::Asynchronous {
            "clean-caught-up-sync-hwm-or-explicit-declared-loss-hwm"
        } else {
            "fully-mirrored-sync-hwm"
        },
        fence_source_ip
            .map(|address| address.to_string())
            .unwrap_or_else(|| "disabled".to_string()),
    );

    let mut handles = Vec::with_capacity(lanes as usize);
    for lane in 0..lanes {
        let address = listen.lane_addr(lane)?;
        let primary = primary.clone();
        let secondary = secondary.clone();
        let state = Arc::clone(&state);
        let ingress_transport = ingress_transport.clone();
        let leaf_transport = leaf_transport.clone();
        handles.push(
            thread::Builder::new()
                .name(format!("zcwal-failover-{lane}"))
                .spawn(move || -> io::Result<()> {
                    pin_current_thread_for_lane("zcnblk-wal-failover-lane", lane as usize)?;
                    if ingress_transport.kind == WalTransportKind::Tcp {
                        let listener = TcpListener::bind(address)?;
                        for accepted in listener.incoming() {
                            let upstream = WalStream::Tcp(accepted?);
                            let primary = primary.clone();
                            let secondary = secondary.clone();
                            let state = Arc::clone(&state);
                            let leaf_transport = leaf_transport.clone();
                            thread::Builder::new()
                                .name(format!("zcwal-failover-session-{lane}"))
                                .spawn(move || {
                                    if let Err(error) = proxy_session(
                                        upstream,
                                        primary,
                                        secondary,
                                        leaf_transport,
                                        state,
                                    ) {
                                        eprintln!("zcnblk-wal-failover-session-error: lane={lane} error={error}");
                                    }
                                })?;
                        }
                    } else {
                        loop {
                            let upstream = ZcOfiMessageStream::connect_on_domain(
                                &ingress_transport.provider,
                                &ingress_transport.endpoint,
                                &address.ip().to_string(),
                                address.port(),
                                true,
                                false,
                                ingress_transport.lane_domain(lane)?,
                            )?;
                            if let Err(error) = proxy_session(
                                WalStream::Ofi(upstream),
                                primary.clone(),
                                secondary.clone(),
                                leaf_transport.clone(),
                                Arc::clone(&state),
                            ) {
                                eprintln!("zcnblk-wal-failover-session-error: transport=ofi-rdm lane={lane} error={error}");
                            }
                        }
                    }
                    #[allow(unreachable_code)]
                    Ok(())
                })?,
        );
    }
    for handle in handles {
        handle
            .join()
            .map_err(|_| io::Error::other("WAL failover listener panicked"))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(zc_has_libfabric)]
    fn ofi_test_port() -> u16 {
        for _ in 0..100 {
            let socket = TcpListener::bind(("127.0.0.2", 0)).unwrap();
            let port = socket.local_addr().unwrap().port();
            drop(socket);
            let Some(control) = port.checked_add(1000) else {
                continue;
            };
            let probes = [
                TcpListener::bind(("127.0.0.2", control)),
                TcpListener::bind(("127.0.0.3", control)),
            ];
            if probes.iter().all(Result::is_ok) {
                return port;
            }
        }
        panic!("could not reserve OFI test service/control ports")
    }

    #[cfg(zc_has_libfabric)]
    fn ofi_memory_leaf(bind: &'static str, port: u16) -> io::Result<Vec<u8>> {
        let mut stream =
            ZcOfiMessageStream::connect_on_domain("sockets", "rdm", bind, port, true, false, None)?;
        let mut memory = vec![0u8; 64 * 1024];
        loop {
            let (frame, payload) = read_frame(&mut stream)?;
            match frame.op {
                ZCNBLK_FAN_WAL_OP_HELLO => write_frame(
                    &mut stream,
                    ZcnblkFanWalFrame {
                        op: ZCNBLK_FAN_WAL_OP_HELLO_ACK,
                        payload_len: 0,
                        ..frame
                    },
                    &[],
                )?,
                ZCNBLK_FAN_WAL_OP_WRITE_DESC => {
                    let start = frame.leaf_offset as usize;
                    let end = start + payload.len();
                    memory[start..end].copy_from_slice(&payload);
                    write_frame(
                        &mut stream,
                        ZcnblkFanWalFrame {
                            op: ZCNBLK_FAN_WAL_OP_RESULT,
                            payload_len: 0,
                            ..frame
                        },
                        &[],
                    )?;
                }
                ZCNBLK_FAN_WAL_OP_READ_DESC => {
                    let start = frame.leaf_offset as usize;
                    let end = start + frame.payload_len as usize;
                    write_frame(
                        &mut stream,
                        ZcnblkFanWalFrame {
                            op: ZCNBLK_FAN_WAL_OP_RESULT,
                            ..frame
                        },
                        &memory[start..end],
                    )?;
                }
                ZCNBLK_FAN_WAL_OP_SYNC => write_frame(
                    &mut stream,
                    ZcnblkFanWalFrame {
                        op: ZCNBLK_FAN_WAL_OP_RESULT,
                        payload_len: 0,
                        ..frame
                    },
                    &[],
                )?,
                ZCNBLK_FAN_WAL_OP_EOF => return Ok(memory),
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("test leaf got unsupported op {other}"),
                    ));
                }
            }
        }
    }

    #[test]
    fn promotion_requires_synced_generation_and_is_one_way() {
        let state = FailoverState::new();
        state.write_generation.store(3, Ordering::Release);
        assert_eq!(
            state.promote_secondary().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        state.synced_generation.store(3, Ordering::Release);
        let result = state.promote_secondary().unwrap();
        assert!(result.starts_with("OK active=secondary"));
        assert_eq!(state.write_mask.load(Ordering::Acquire), SECONDARY_MASK);
        assert_eq!(state.placement_epoch.load(Ordering::Acquire), 2);
    }

    #[test]
    fn request_batch_write_detection_uses_descriptors() {
        let batch = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_REQUEST_BATCH,
            segment_count: 2,
            payload_len: (ZCNBLK_FAN_WAL_HEADER_LEN * 2) as u32,
            ..ZcnblkFanWalFrame::default()
        };
        let read = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_READ_DESC,
            ..ZcnblkFanWalFrame::default()
        };
        let write = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_WRITE_DESC,
            ..ZcnblkFanWalFrame::default()
        };
        let mut payload = Vec::new();
        payload.extend_from_slice(&read.encode());
        payload.extend_from_slice(&write.encode());
        assert!(request_batch_contains_write(batch, &payload).unwrap());
    }

    #[test]
    fn asynchronous_declared_loss_requires_exact_remote_durable_hwm() {
        let state = FailoverState::with_mode(ReplicationMode::Asynchronous);
        state.write_generation.store(20, Ordering::Release);
        state.synced_generation.store(20, Ordering::Release);
        state
            .secondary_write_generation
            .store(10, Ordering::Release);
        state
            .secondary_synced_generation
            .store(10, Ordering::Release);
        assert!(
            state
                .promote_secondary_accept_loss(9, "regional disaster")
                .is_err()
        );
        let result = state
            .promote_secondary_accept_loss(10, "regional disaster")
            .unwrap();
        assert!(result.contains("declared_loss=true"));
        assert!(result.contains("first_missing=Some(11)"));
        assert!(result.contains("last_missing=Some(20)"));
        assert_eq!(state.active.load(Ordering::Acquire), SECONDARY_MASK);
        assert_eq!(state.placement_epoch.load(Ordering::Acquire), 2);
    }

    #[test]
    fn ofi_rdm_is_distinct_from_one_sided_rma_payloads() {
        assert_eq!(
            WalTransportKind::parse("ofi", "TEST").unwrap(),
            WalTransportKind::OfiRdm
        );
        assert!(WalTransportKind::parse("rdma-write", "TEST").is_err());
        let transport = WalTransport {
            kind: WalTransportKind::OfiRdm,
            provider: "efa".to_string(),
            endpoint: "rdm".to_string(),
            domains: vec!["efa_0-rdm".to_string()],
        };
        assert_eq!(transport.lane_domain(0).unwrap(), Some("efa_0-rdm"));
        assert_eq!(transport.lane_domain(31).unwrap(), Some("efa_0-rdm"));
    }

    #[cfg(zc_has_libfabric)]
    #[test]
    fn ofi_rdm_mirror_preserves_write_sync_read_on_both_leaves() {
        let leaf_port = ofi_test_port();
        let primary_leaf = thread::spawn(move || ofi_memory_leaf("127.0.0.2", leaf_port));
        let secondary_leaf = thread::spawn(move || ofi_memory_leaf("127.0.0.3", leaf_port));

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let fan_address = listener.local_addr().unwrap();
        let state = Arc::new(FailoverState::new());
        let fan_state = Arc::clone(&state);
        let fan = thread::spawn(move || -> io::Result<()> {
            let (upstream, _) = listener.accept()?;
            proxy_session(
                WalStream::Tcp(upstream),
                Endpoint::parse(&format!("127.0.0.2:{leaf_port}"))?,
                Endpoint::parse(&format!("127.0.0.3:{leaf_port}"))?,
                WalTransport {
                    kind: WalTransportKind::OfiRdm,
                    provider: "sockets".to_string(),
                    endpoint: "rdm".to_string(),
                    domains: Vec::new(),
                },
                fan_state,
            )
        });

        let mut client = TcpStream::connect(fan_address).unwrap();
        let hello = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_HELLO,
            lane_id: 0,
            lane_count: 1,
            branch_count: 1,
            placement_epoch: 1,
            ..ZcnblkFanWalFrame::default()
        };
        write_frame(&mut client, hello, &[]).unwrap();
        assert_eq!(
            read_frame(&mut client).unwrap().0.op,
            ZCNBLK_FAN_WAL_OP_HELLO_ACK
        );

        let expected = b"mirrored-ofi-rdm";
        let write = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_WRITE_DESC,
            lane_id: 0,
            lane_count: 1,
            branch_count: 1,
            placement_epoch: 1,
            payload_len: expected.len() as u32,
            logical_len: expected.len() as u32,
            ..ZcnblkFanWalFrame::default()
        };
        write_frame(&mut client, write, expected).unwrap();
        assert_eq!(
            read_frame(&mut client).unwrap().0.op,
            ZCNBLK_FAN_WAL_OP_RESULT
        );

        let sync = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_SYNC,
            lane_id: 0,
            lane_count: 1,
            branch_count: 1,
            placement_epoch: 1,
            sync_epoch: 7,
            ..ZcnblkFanWalFrame::default()
        };
        write_frame(&mut client, sync, &[]).unwrap();
        assert_eq!(
            read_frame(&mut client).unwrap().0.op,
            ZCNBLK_FAN_WAL_OP_RESULT
        );

        let read = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_READ_DESC,
            lane_id: 0,
            lane_count: 1,
            branch_count: 1,
            placement_epoch: 1,
            payload_len: expected.len() as u32,
            logical_len: expected.len() as u32,
            ..ZcnblkFanWalFrame::default()
        };
        write_frame(&mut client, read, &[]).unwrap();
        let (result, payload) = read_frame(&mut client).unwrap();
        assert_eq!(result.op, ZCNBLK_FAN_WAL_OP_RESULT);
        assert_eq!(payload, expected);

        write_frame(
            &mut client,
            ZcnblkFanWalFrame {
                op: ZCNBLK_FAN_WAL_OP_EOF,
                lane_id: 0,
                lane_count: 1,
                branch_count: 1,
                placement_epoch: 1,
                ..ZcnblkFanWalFrame::default()
            },
            &[],
        )
        .unwrap();
        drop(client);

        fan.join().unwrap().unwrap();
        let primary = primary_leaf.join().unwrap().unwrap();
        let secondary = secondary_leaf.join().unwrap().unwrap();
        assert_eq!(&primary[..expected.len()], expected);
        assert_eq!(&secondary[..expected.len()], expected);
        assert_eq!(state.write_generation.load(Ordering::Acquire), 1);
        assert_eq!(state.synced_generation.load(Ordering::Acquire), 1);
        assert_eq!(state.sync_epoch.load(Ordering::Acquire), 7);
    }
}
