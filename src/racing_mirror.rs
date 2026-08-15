//! Userspace racing high-water mirror.
//!
//! Placement happens here, after the client block edge.  A first-hop stage
//! appends one framed-log leg to local terminal media and forwards the same
//! pipe buffers to a remote userspace terminal writer.  The acknowledged
//! high-water mark is the contiguous minimum of the two durable legs.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub const HEADER_LEN: usize = 64;
pub const MAX_PAYLOAD: usize = 32 * 1024;
const MAGIC: u64 = 0x5a43_524d_4952_5231; // ZCRMIRR1
const VERSION: u32 = 1;
const KIND_DATA: u32 = 1;
const KIND_ACK: u32 = 2;
const HANDSHAKE_SEQUENCE: u64 = u64::MAX;
const FLAG_MORE: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameHeader {
    pub kind: u32,
    pub sequence: u64,
    pub payload_len: u32,
    pub flags: u32,
    pub value: u64,
}

impl FrameHeader {
    pub fn data(sequence: u64, payload_len: u32, seed: u64) -> Self {
        Self::data_with_more(sequence, payload_len, seed, false)
    }

    pub fn data_with_more(sequence: u64, payload_len: u32, seed: u64, more: bool) -> Self {
        Self {
            kind: KIND_DATA,
            sequence,
            payload_len,
            flags: if more { FLAG_MORE } else { 0 },
            value: seed,
        }
    }

    fn ends_sync_batch(self) -> bool {
        self.kind == KIND_DATA && self.flags & FLAG_MORE == 0
    }

    pub fn ack(sequence: u64, highwater: u64) -> Self {
        Self {
            kind: KIND_ACK,
            sequence,
            payload_len: 0,
            flags: 0,
            value: highwater,
        }
    }

    pub fn encode(self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        out[8..12].copy_from_slice(&VERSION.to_le_bytes());
        out[12..16].copy_from_slice(&self.kind.to_le_bytes());
        out[16..24].copy_from_slice(&self.sequence.to_le_bytes());
        out[24..28].copy_from_slice(&self.payload_len.to_le_bytes());
        out[28..32].copy_from_slice(&self.flags.to_le_bytes());
        out[32..40].copy_from_slice(&self.value.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8; HEADER_LEN]) -> io::Result<Self> {
        let u32_at = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let u64_at = |at| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        if u64_at(0) != MAGIC || u32_at(8) != VERSION {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "invalid mirror frame magic/version",
            ));
        }
        let header = Self {
            kind: u32_at(12),
            sequence: u64_at(16),
            payload_len: u32_at(24),
            flags: u32_at(28),
            value: u64_at(32),
        };
        if !matches!(header.kind, KIND_DATA | KIND_ACK)
            || header.payload_len as usize > MAX_PAYLOAD
            || (header.kind == KIND_ACK && header.payload_len != 0)
            || header.flags & !FLAG_MORE != 0
            || (header.kind == KIND_ACK && header.flags != 0)
        {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "invalid mirror frame fields",
            ));
        }
        Ok(header)
    }
}

#[derive(Debug, Default)]
pub struct ContiguousHighwater {
    next: u64,
    completed: BTreeSet<u64>,
}

impl ContiguousHighwater {
    pub fn starting_at(next: u64) -> Self {
        Self {
            next,
            completed: BTreeSet::new(),
        }
    }

    pub fn complete(&mut self, sequence: u64) -> io::Result<u64> {
        if sequence < self.next {
            return Ok(self.next);
        }
        if !self.completed.insert(sequence) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "duplicate completion",
            ));
        }
        while self.completed.remove(&self.next) {
            self.next += 1;
        }
        Ok(self.next)
    }

    pub fn next(&self) -> u64 {
        self.next
    }
}

pub fn mirror_highwater(local: &ContiguousHighwater, remote: &ContiguousHighwater) -> u64 {
    local.next().min(remote.next())
}

fn pipe_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pipe2 returned two newly owned descriptors.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn pipe_capacity(fd: RawFd) -> io::Result<usize> {
    let capacity = unsafe { libc::fcntl(fd, libc::F_GETPIPE_SZ) };
    if capacity < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(capacity as usize)
    }
}

fn cvt_count(result: libc::ssize_t) -> io::Result<usize> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

fn splice_exact(from: RawFd, to: RawFd, mut len: usize) -> io::Result<()> {
    while len != 0 {
        let done = match cvt_count(unsafe {
            libc::splice(
                from,
                std::ptr::null_mut(),
                to,
                std::ptr::null_mut(),
                len,
                libc::SPLICE_F_MOVE,
            )
        }) {
            Ok(done) => done,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if done == 0 {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "short splice"));
        }
        len -= done;
    }
    Ok(())
}

fn splice_some(from: RawFd, to: RawFd, max_len: usize) -> io::Result<usize> {
    loop {
        match cvt_count(unsafe {
            libc::splice(
                from,
                std::ptr::null_mut(),
                to,
                std::ptr::null_mut(),
                max_len,
                libc::SPLICE_F_MOVE | libc::SPLICE_F_MORE,
            )
        }) {
            Ok(0) => return Err(io::Error::new(ErrorKind::UnexpectedEof, "short splice")),
            Ok(done) => return Ok(done),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn tee_exact(from: RawFd, to: RawFd, mut len: usize) -> io::Result<()> {
    while len != 0 {
        let done = cvt_count(unsafe { libc::tee(from, to, len, libc::SPLICE_F_NONBLOCK) });
        match done {
            Ok(0) => return Err(io::Error::new(ErrorKind::WriteZero, "short tee")),
            Ok(done) => len -= done,
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::yield_now(),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn vmsplice_all(
    socket: RawFd,
    bytes: &[u8],
    read_pipe: RawFd,
    write_pipe: RawFd,
) -> io::Result<()> {
    let mut sent = 0;
    while sent < bytes.len() {
        let mut iov = libc::iovec {
            iov_base: bytes[sent..].as_ptr() as *mut libc::c_void,
            iov_len: bytes.len() - sent,
        };
        let pinned = cvt_count(unsafe { libc::vmsplice(write_pipe, &mut iov, 1, 0) })?;
        if pinned == 0 {
            return Err(io::Error::new(ErrorKind::WriteZero, "short vmsplice"));
        }
        splice_exact(read_pipe, socket, pinned)?;
        sent += pinned;
    }
    Ok(())
}

fn read_header(stream: &mut impl Read) -> io::Result<Option<FrameHeader>> {
    let mut bytes = [0u8; HEADER_LEN];
    let mut filled = 0;
    while filled < HEADER_LEN {
        match stream.read(&mut bytes[filled..])? {
            0 if filled == 0 => return Ok(None),
            0 => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "partial mirror header",
                ));
            }
            n => filled += n,
        }
    }
    FrameHeader::decode(&bytes).map(Some)
}

fn pattern_byte(seed: u64, offset: usize) -> u8 {
    let mut x = seed ^ (offset as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    (x ^ (x >> 31)) as u8
}

pub fn make_payload(seed: u64, len: usize) -> Vec<u8> {
    (0..len).map(|offset| pattern_byte(seed, offset)).collect()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanResult {
    pub frames: u64,
    pub valid_bytes: u64,
    pub incomplete_tail_bytes: u64,
}

pub fn scan_log(path: &Path, verify_payload: bool) -> io::Result<ScanResult> {
    let mut file = OpenOptions::new().read(true).open(path)?;
    let total = file.metadata()?.len();
    let mut parsed_bytes = 0u64;
    let mut parsed_frames = 0u64;
    let mut committed_bytes = 0u64;
    let mut committed_frames = 0u64;
    loop {
        if total == parsed_bytes {
            break;
        }
        if total - parsed_bytes < HEADER_LEN as u64 {
            return Ok(ScanResult {
                frames: committed_frames,
                valid_bytes: committed_bytes,
                incomplete_tail_bytes: total - committed_bytes,
            });
        }
        let Some(header) = read_header(&mut file)? else {
            break;
        };
        if header.kind != KIND_DATA || header.sequence != parsed_frames {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "non-contiguous terminal log",
            ));
        }
        let mut remaining = header.payload_len as usize;
        let mut offset = 0usize;
        let mut chunk = [0u8; 8192];
        while remaining != 0 {
            let take = remaining.min(chunk.len());
            match file.read_exact(&mut chunk[..take]) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                    return Ok(ScanResult {
                        frames: committed_frames,
                        valid_bytes: committed_bytes,
                        incomplete_tail_bytes: total - committed_bytes,
                    });
                }
                Err(error) => return Err(error),
            }
            if verify_payload
                && chunk[..take]
                    .iter()
                    .enumerate()
                    .any(|(index, byte)| *byte != pattern_byte(header.value, offset + index))
            {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "terminal payload mismatch",
                ));
            }
            remaining -= take;
            offset += take;
        }
        parsed_bytes += HEADER_LEN as u64 + header.payload_len as u64;
        parsed_frames += 1;
        if header.ends_sync_batch() {
            committed_bytes = parsed_bytes;
            committed_frames = parsed_frames;
        }
    }
    Ok(ScanResult {
        frames: committed_frames,
        valid_bytes: committed_bytes,
        incomplete_tail_bytes: total.saturating_sub(committed_bytes),
    })
}

fn open_terminal_log(path: &Path) -> io::Result<(File, u64)> {
    let scan = if path.exists() {
        scan_log(path, false)?
    } else {
        ScanResult::default()
    };
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)?;
    if scan.incomplete_tail_bytes != 0 {
        file.set_len(scan.valid_bytes)?;
    }
    file.seek(std::io::SeekFrom::Start(scan.valid_bytes))?;
    file.sync_data()?;
    Ok((file, scan.frames))
}

fn replay_suffix(local: &File, start: u64, end: u64, remote: &mut TcpStream) -> io::Result<()> {
    let mut source = local.try_clone()?;
    source.seek(std::io::SeekFrom::Start(0))?;
    let (read_pipe, write_pipe) = pipe_pair()?;
    let splice_chunk = pipe_capacity(write_pipe.as_raw_fd())?;
    for expected in 0..end {
        let header = read_header(&mut source)?.ok_or_else(|| {
            io::Error::new(ErrorKind::UnexpectedEof, "local replay prefix ended early")
        })?;
        if header.kind != KIND_DATA || header.sequence != expected {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "invalid local replay prefix",
            ));
        }
        if expected < start {
            source.seek(std::io::SeekFrom::Current(header.payload_len as i64))?;
            continue;
        }
        remote.write_all(&header.encode())?;
        let mut remaining = header.payload_len as usize;
        while remaining != 0 {
            let chunk = remaining.min(splice_chunk);
            let moved = splice_some(source.as_raw_fd(), write_pipe.as_raw_fd(), chunk)?;
            splice_exact(read_pipe.as_raw_fd(), remote.as_raw_fd(), moved)?;
            remaining -= moved;
        }
        if header.ends_sync_batch() {
            let ack = read_header(remote)?.ok_or_else(|| {
                io::Error::new(ErrorKind::UnexpectedEof, "remote closed during replay")
            })?;
            if ack.kind != KIND_ACK || ack.sequence != expected || ack.value != expected + 1 {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "invalid remote replay ACK",
                ));
            }
        }
    }
    Ok(())
}

fn connect_until(target: &str, timeout: Duration) -> io::Result<TcpStream> {
    let addresses: Vec<_> = target.to_socket_addrs()?.collect();
    if addresses.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "target resolved to no addresses",
        ));
    }
    let deadline = Instant::now() + timeout;
    loop {
        let mut last_error = None;
        for address in &addresses {
            match TcpStream::connect_timeout(address, Duration::from_millis(250)) {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = Some(error),
            }
        }
        if Instant::now() >= deadline {
            return Err(last_error
                .unwrap_or_else(|| io::Error::new(ErrorKind::TimedOut, "connect timed out")));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

pub fn run_leaf(listen: &str, path: &Path, delay_ms: u64) -> io::Result<()> {
    let listener = TcpListener::bind(listen)?;
    eprintln!("RACING_MIRROR_READY role=remote-leaf listen={listen}");
    let (mut stream, peer) = listener.accept()?;
    stream.set_nodelay(true)?;
    let (mut log, mut expected) = open_terminal_log(path)?;
    let session_start = expected;
    let mut durable_hwm = expected;
    let mut sync_batches = 0u64;
    let (read_pipe, write_pipe) = pipe_pair()?;
    let splice_chunk = pipe_capacity(write_pipe.as_raw_fd())?;
    stream.write_all(&FrameHeader::ack(HANDSHAKE_SEQUENCE, expected).encode())?;
    loop {
        let Some(header) = read_header(&mut stream)? else {
            break;
        };
        if header.kind != KIND_DATA || header.sequence != expected {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("remote sequence {} expected {}", header.sequence, expected),
            ));
        }
        log.write_all(&header.encode())?;
        let mut remaining = header.payload_len as usize;
        while remaining != 0 {
            let chunk = remaining.min(splice_chunk);
            let moved = splice_some(stream.as_raw_fd(), write_pipe.as_raw_fd(), chunk)?;
            splice_exact(read_pipe.as_raw_fd(), log.as_raw_fd(), moved)?;
            remaining -= moved;
        }
        expected += 1;
        if header.ends_sync_batch() {
            log.sync_data()?;
            if delay_ms != 0 {
                thread::sleep(Duration::from_millis(delay_ms));
            }
            durable_hwm = expected;
            sync_batches += 1;
            stream.write_all(&FrameHeader::ack(header.sequence, durable_hwm).encode())?;
        }
    }
    if expected != durable_hwm {
        return Err(io::Error::new(
            ErrorKind::UnexpectedEof,
            "remote stream ended inside a sync batch",
        ));
    }
    log.sync_data()?;
    eprintln!(
        "RACING_MIRROR_LEAF_PASS peer={peer} durable_hwm={durable_hwm} appended_frames={} sync_batches={sync_batches} payload_userspace_copy_bytes=0 header_copy_bytes={}",
        expected - session_start,
        HEADER_LEN as u64
            + (expected - session_start) * HEADER_LEN as u64 * 2
            + sync_batches * HEADER_LEN as u64
    );
    Ok(())
}

pub fn run_first_hop(listen: &str, remote: &str, local_path: &Path) -> io::Result<()> {
    let mut remote_stream = connect_until(remote, Duration::from_secs(30))?;
    remote_stream.set_nodelay(true)?;
    let remote_start = read_header(&mut remote_stream)?.ok_or_else(|| {
        io::Error::new(ErrorKind::UnexpectedEof, "remote closed before handshake")
    })?;
    if remote_start.kind != KIND_ACK || remote_start.sequence != HANDSHAKE_SEQUENCE {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "invalid remote recovery handshake",
        ));
    }
    let (mut local, local_start) = open_terminal_log(local_path)?;
    if remote_start.value > local_start {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "remote mirror leg is ahead of local leg: local={} remote={}",
                local_start, remote_start.value
            ),
        ));
    }
    if remote_start.value < local_start {
        replay_suffix(&local, remote_start.value, local_start, &mut remote_stream)?;
        eprintln!(
            "RACING_MIRROR_REPLAY_PASS from_hwm={} to_hwm={} payload_userspace_copy_bytes=0",
            remote_start.value, local_start
        );
    }
    let listener = TcpListener::bind(listen)?;
    eprintln!("RACING_MIRROR_READY role=first-hop listen={listen} remote={remote}");
    let (mut client, peer) = listener.accept()?;
    client.set_nodelay(true)?;
    client.write_all(&FrameHeader::ack(HANDSHAKE_SEQUENCE, local_start).encode())?;
    let mut expected = local_start;
    let mut local_hwm = ContiguousHighwater::starting_at(local_start);
    let mut remote_hwm = ContiguousHighwater::starting_at(local_start);
    let mut batch_start = local_start;
    let mut sync_batches = 0u64;
    let sync_file = local.try_clone()?;
    let (sync_request_tx, sync_request_rx) = mpsc::channel::<u64>();
    let (sync_done_tx, sync_done_rx) = mpsc::channel::<(u64, io::Result<()>)>();
    let sync_thread = thread::spawn(move || {
        while let Ok(highwater) = sync_request_rx.recv() {
            if sync_done_tx
                .send((highwater, sync_file.sync_data()))
                .is_err()
            {
                break;
            }
        }
    });
    let (source_read, source_write) = pipe_pair()?;
    let (remote_read, remote_write) = pipe_pair()?;
    let relay_chunk =
        pipe_capacity(source_write.as_raw_fd())?.min(pipe_capacity(remote_write.as_raw_fd())?);
    loop {
        let Some(header) = read_header(&mut client)? else {
            break;
        };
        if header.kind != KIND_DATA || header.sequence != expected {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("client sequence {} expected {}", header.sequence, expected),
            ));
        }
        local.write_all(&header.encode())?;
        remote_stream.write_all(&header.encode())?;

        let payload_len = header.payload_len as usize;
        let mut remaining = payload_len;
        while remaining != 0 {
            let chunk = remaining.min(relay_chunk);
            let moved = splice_some(client.as_raw_fd(), source_write.as_raw_fd(), chunk)?;
            tee_exact(source_read.as_raw_fd(), remote_write.as_raw_fd(), moved)?;
            splice_exact(source_read.as_raw_fd(), local.as_raw_fd(), moved)?;
            splice_exact(remote_read.as_raw_fd(), remote_stream.as_raw_fd(), moved)?;
            remaining -= moved;
        }
        expected += 1;
        if header.ends_sync_batch() {
            sync_request_tx
                .send(expected)
                .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "local sync worker stopped"))?;
            let remote_ack = read_header(&mut remote_stream)?.ok_or_else(|| {
                io::Error::new(ErrorKind::UnexpectedEof, "remote closed before ACK")
            })?;
            if remote_ack.kind != KIND_ACK
                || remote_ack.sequence != header.sequence
                || remote_ack.value != expected
            {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "invalid remote durable ACK",
                ));
            }
            let (local_done, result) = sync_done_rx.recv().map_err(|_| {
                io::Error::new(ErrorKind::BrokenPipe, "local sync worker lost completion")
            })?;
            if local_done != expected {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "local sync worker returned the wrong high-water mark",
                ));
            }
            result?;
            for sequence in batch_start..expected {
                local_hwm.complete(sequence)?;
                remote_hwm.complete(sequence)?;
            }
            batch_start = expected;
            sync_batches += 1;
            let highwater = mirror_highwater(&local_hwm, &remote_hwm);
            client.write_all(&FrameHeader::ack(header.sequence, highwater).encode())?;
        }
    }
    if batch_start != expected {
        return Err(io::Error::new(
            ErrorKind::UnexpectedEof,
            "client stream ended inside a sync batch",
        ));
    }
    remote_stream.shutdown(Shutdown::Write)?;
    drop(sync_request_tx);
    sync_thread
        .join()
        .map_err(|_| io::Error::other("local sync worker panicked"))?;
    eprintln!(
        "RACING_MIRROR_FIRST_HOP_PASS peer={peer} durable_hwm={} appended_frames={} sync_batches={sync_batches} payload_userspace_copy_bytes=0 header_copy_bytes={}",
        mirror_highwater(&local_hwm, &remote_hwm),
        expected - local_start,
        HEADER_LEN as u64 * 2
            + (expected - local_start) * HEADER_LEN as u64 * 3
            + sync_batches * HEADER_LEN as u64 * 2
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub struct ClientResult {
    pub frames: u64,
    pub recovered_from: u64,
    pub payload_bytes: u64,
    pub window: u64,
    pub elapsed: Duration,
}

pub fn run_client(target: &str, frames: u64, payload_len: usize) -> io::Result<ClientResult> {
    run_client_window(target, frames, payload_len, 1)
}

pub fn run_client_window(
    target: &str,
    frames: u64,
    payload_len: usize,
    window: u64,
) -> io::Result<ClientResult> {
    if payload_len == 0 || payload_len > MAX_PAYLOAD {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("payload must be 1..={MAX_PAYLOAD}"),
        ));
    }
    if window == 0 || window > 4096 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "window must be 1..=4096",
        ));
    }
    let mut stream = connect_until(target, Duration::from_secs(30))?;
    stream.set_nodelay(true)?;
    let handshake = read_header(&mut stream)?.ok_or_else(|| {
        io::Error::new(
            ErrorKind::UnexpectedEof,
            "first hop closed before handshake",
        )
    })?;
    if handshake.kind != KIND_ACK || handshake.sequence != HANDSHAKE_SEQUENCE {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "invalid mirror recovery handshake",
        ));
    }
    let first_sequence = handshake.value;
    let (source_read, source_write) = pipe_pair()?;
    let start = Instant::now();
    let final_sequence = first_sequence
        .checked_add(frames)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "sequence range overflow"))?;
    let mut sequence = first_sequence;
    while sequence < final_sequence {
        let batch_end = sequence.saturating_add(window).min(final_sequence);
        let mut payload_leases = Vec::with_capacity((batch_end - sequence) as usize);
        while sequence < batch_end {
            let seed = sequence ^ 0xa5a5_5a5a_d3c4_b2e1;
            let payload = make_payload(seed, payload_len);
            let more = sequence + 1 < batch_end;
            stream.write_all(
                &FrameHeader::data_with_more(sequence, payload_len as u32, seed, more).encode(),
            )?;
            vmsplice_all(
                stream.as_raw_fd(),
                &payload,
                source_read.as_raw_fd(),
                source_write.as_raw_fd(),
            )?;
            payload_leases.push(payload);
            sequence += 1;
        }
        let ack = read_header(&mut stream)?.ok_or_else(|| {
            io::Error::new(ErrorKind::UnexpectedEof, "first hop closed before ACK")
        })?;
        if ack.kind != KIND_ACK || ack.sequence != batch_end - 1 || ack.value != batch_end {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "invalid mirror durable ACK",
            ));
        }
        drop(payload_leases);
    }
    stream.shutdown(Shutdown::Write)?;
    Ok(ClientResult {
        frames,
        recovered_from: first_sequence,
        payload_bytes: frames * payload_len as u64,
        window,
        elapsed: start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Seek;
    use std::path::PathBuf;

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zcracing-{label}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ))
    }

    #[test]
    fn header_round_trip() {
        let header = FrameHeader::data(19, 4096, 88);
        assert_eq!(FrameHeader::decode(&header.encode()).unwrap(), header);
    }

    #[test]
    fn highwater_waits_for_holes_and_slowest_leg() {
        let mut local = ContiguousHighwater::starting_at(0);
        let mut remote = ContiguousHighwater::starting_at(0);
        local.complete(1).unwrap();
        local.complete(0).unwrap();
        assert_eq!(local.next(), 2);
        assert_eq!(mirror_highwater(&local, &remote), 0);
        remote.complete(0).unwrap();
        assert_eq!(mirror_highwater(&local, &remote), 1);
        remote.complete(1).unwrap();
        assert_eq!(mirror_highwater(&local, &remote), 2);
    }

    #[test]
    fn scanner_ignores_partial_tail() {
        let path = temp_path("partial");
        let mut file = File::create(&path).unwrap();
        let payload = make_payload(7, 4096);
        file.write_all(&FrameHeader::data(0, 4096, 7).encode())
            .unwrap();
        file.write_all(&payload).unwrap();
        file.write_all(&FrameHeader::data(1, 4096, 8).encode())
            .unwrap();
        file.write_all(&payload[..17]).unwrap();
        file.flush().unwrap();
        let scan = scan_log(&path, true).unwrap();
        assert_eq!(scan.frames, 1);
        assert_eq!(scan.valid_bytes, (HEADER_LEN + 4096) as u64);
        assert_eq!(scan.incomplete_tail_bytes, (HEADER_LEN + 17) as u64);
        let (mut reopened, frames) = open_terminal_log(&path).unwrap();
        assert_eq!(frames, 1);
        assert_eq!(reopened.metadata().unwrap().len(), scan.valid_bytes);
        reopened.seek(std::io::SeekFrom::End(0)).unwrap();
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn scanner_discards_a_complete_uncommitted_batch_tail() {
        let path = temp_path("uncommitted-batch");
        let mut file = File::create(&path).unwrap();
        let payload0 = make_payload(7, 4096);
        let payload1 = make_payload(8, 4096);
        file.write_all(&FrameHeader::data(0, 4096, 7).encode())
            .unwrap();
        file.write_all(&payload0).unwrap();
        file.write_all(&FrameHeader::data_with_more(1, 4096, 8, true).encode())
            .unwrap();
        file.write_all(&payload1).unwrap();
        file.flush().unwrap();

        let scan = scan_log(&path, true).unwrap();
        assert_eq!(scan.frames, 1);
        assert_eq!(scan.valid_bytes, (HEADER_LEN + 4096) as u64);
        assert_eq!(scan.incomplete_tail_bytes, (HEADER_LEN + 4096) as u64);
        let (reopened, frames) = open_terminal_log(&path).unwrap();
        assert_eq!(frames, 1);
        assert_eq!(reopened.metadata().unwrap().len(), scan.valid_bytes);
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }
}
