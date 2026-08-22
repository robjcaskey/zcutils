//! A zcutils-owned, placement-free iSCSI target.
//!
//! This implementation deliberately does not use an iSCSI protocol crate.
//! The wire engine implements the RFC 7143 PDUs needed by Linux open-iscsi,
//! and the SCSI subset needed by a 4 KiB direct-access volume. User payloads
//! go directly between the TCP socket and lane-local HugeTLB arena leases.
//! Mirroring, striping, spill, tiering, locality, and backpressure remain in
//! the separate userspace volume stage behind `/dev/zcnblk0`.

use std::collections::HashMap;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, IoSlice, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use zcutils::zcnblk_app_arena::{
    ZcnblkAppArena, ZcnblkAppArenaBuffer, ZcnblkAppArenaIoCompletion, ZcnblkAppArenaIoRing,
    open_block_direct, pin_current_thread,
};

const BHS_BYTES: usize = 48;
const BLOCK_SIZE: u32 = 4096;
const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
const COMMAND_WINDOW: u32 = 4096;
const MAX_CONTROL_DATA: usize = 1024 * 1024;
const MAX_SEGMENT: u32 = 262_144;
const LANE_PIPELINE: usize = 64;

const OP_NOP_OUT: u8 = 0x00;
const OP_SCSI_COMMAND: u8 = 0x01;
const OP_LOGIN_REQUEST: u8 = 0x03;
const OP_TEXT_REQUEST: u8 = 0x04;
const OP_DATA_OUT: u8 = 0x05;
const OP_LOGOUT_REQUEST: u8 = 0x06;

const OP_NOP_IN: u8 = 0x20;
const OP_SCSI_RESPONSE: u8 = 0x21;
const OP_LOGIN_RESPONSE: u8 = 0x23;
const OP_TEXT_RESPONSE: u8 = 0x24;
const OP_DATA_IN: u8 = 0x25;
const OP_LOGOUT_RESPONSE: u8 = 0x26;
const OP_R2T: u8 = 0x31;

#[derive(Clone, Copy)]
struct Header([u8; BHS_BYTES]);

impl Header {
    fn zeroed(opcode: u8) -> Self {
        let mut value = Self([0; BHS_BYTES]);
        value.0[0] = opcode;
        value
    }

    fn opcode(&self) -> u8 {
        self.0[0] & 0x3f
    }

    fn immediate(&self) -> bool {
        self.0[0] & 0x40 != 0
    }

    fn data_len(&self) -> usize {
        ((self.0[5] as usize) << 16) | ((self.0[6] as usize) << 8) | self.0[7] as usize
    }

    fn set_data_len(&mut self, len: usize) -> io::Result<()> {
        if len > 0x00ff_ffff {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "iSCSI data segment exceeds 24-bit length",
            ));
        }
        self.0[5] = (len >> 16) as u8;
        self.0[6] = (len >> 8) as u8;
        self.0[7] = len as u8;
        Ok(())
    }

    fn u16(&self, offset: usize) -> u16 {
        u16::from_be_bytes(self.0[offset..offset + 2].try_into().unwrap())
    }

    fn u32(&self, offset: usize) -> u32 {
        u32::from_be_bytes(self.0[offset..offset + 4].try_into().unwrap())
    }

    fn u64(&self, offset: usize) -> u64 {
        u64::from_be_bytes(self.0[offset..offset + 8].try_into().unwrap())
    }

    fn put_u16(&mut self, offset: usize, value: u16) {
        self.0[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
    }

    fn put_u32(&mut self, offset: usize, value: u32) {
        self.0[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn put_u64(&mut self, offset: usize, value: u64) {
        self.0[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
    }
}

fn read_header(stream: &mut TcpStream) -> io::Result<Header> {
    let mut header = Header([0; BHS_BYTES]);
    stream.read_exact(&mut header.0)?;
    if header.0[4] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "additional iSCSI header segments are not negotiated",
        ));
    }
    if header.data_len() > MAX_CONTROL_DATA {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "iSCSI data segment exceeds target safety limit",
        ));
    }
    Ok(header)
}

fn read_padding(stream: &mut TcpStream, data_len: usize) -> io::Result<()> {
    let padding = (4 - data_len % 4) % 4;
    if padding != 0 {
        let mut bytes = [0u8; 3];
        stream.read_exact(&mut bytes[..padding])?;
    }
    Ok(())
}

fn read_small_data(stream: &mut TcpStream, header: Header) -> io::Result<Vec<u8>> {
    let len = header.data_len();
    let mut data = vec![0; len];
    stream.read_exact(&mut data)?;
    read_padding(stream, len)?;
    Ok(data)
}

fn text_pairs(data: &[u8]) -> impl Iterator<Item = (&str, &str)> {
    data.split(|byte| *byte == 0).filter_map(|entry| {
        let text = std::str::from_utf8(entry).ok()?;
        text.split_once('=')
    })
}

fn append_text(output: &mut Vec<u8>, key: &str, value: &str) {
    output.extend_from_slice(key.as_bytes());
    output.push(b'=');
    output.extend_from_slice(value.as_bytes());
    output.push(0);
}

struct FileBackend {
    file: File,
    capacity_bytes: u64,
}

struct ArenaBackend {
    arena: ZcnblkAppArena,
    device: File,
    capacity_bytes: u64,
    lane_cpus: Vec<usize>,
}

enum Backend {
    File(FileBackend),
    Arena(ArenaBackend),
}

enum Blocks {
    Bytes(Vec<u8>),
    Arena(Vec<ZcnblkAppArenaBuffer>),
}

impl Blocks {
    fn len(&self) -> usize {
        match self {
            Self::Bytes(bytes) => bytes.len(),
            Self::Arena(buffers) => buffers.len() * BLOCK_SIZE as usize,
        }
    }

    fn read_from(
        &mut self,
        stream: &mut TcpStream,
        mut offset: usize,
        len: usize,
    ) -> io::Result<()> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "write range overflow"))?;
        if end > self.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "iSCSI write data exceeds expected transfer length",
            ));
        }
        match self {
            Self::Bytes(bytes) => stream.read_exact(&mut bytes[offset..end]),
            Self::Arena(buffers) => {
                let mut left = len;
                while left != 0 {
                    let index = offset / BLOCK_SIZE as usize;
                    let within = offset % BLOCK_SIZE as usize;
                    let take = left.min(BLOCK_SIZE as usize - within);
                    let slice = buffers[index].as_mut_slice()?;
                    stream.read_exact(&mut slice[within..within + take])?;
                    offset += take;
                    left -= take;
                }
                Ok(())
            }
        }
    }
}

impl Backend {
    fn open_file(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let capacity_bytes = file.metadata()?.len();
        validate_capacity(capacity_bytes)?;
        Ok(Self::File(FileBackend {
            file,
            capacity_bytes,
        }))
    }

    fn open_arena(socket: &Path, device: &Path, lane_cpus: Vec<usize>) -> io::Result<Self> {
        let arena = ZcnblkAppArena::connect(socket)?;
        if arena.slot_bytes() != BLOCK_SIZE as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "arena slot size {} is not the required 4096 bytes",
                    arena.slot_bytes()
                ),
            ));
        }
        if lane_cpus.len() != arena.channels() as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "strict arena topology has {} lanes but {} lane CPUs",
                    arena.channels(),
                    lane_cpus.len()
                ),
            ));
        }
        let device = open_block_direct(device)?;
        let mut capacity_bytes = 0u64;
        if unsafe { libc::ioctl(device.as_raw_fd(), BLKGETSIZE64, &mut capacity_bytes) } != 0 {
            return Err(io::Error::last_os_error());
        }
        validate_capacity(capacity_bytes)?;
        drop(ZcnblkAppArenaIoRing::new(2).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("lane-local io_uring preflight failed: {error}"),
            )
        })?);
        Ok(Self::Arena(ArenaBackend {
            arena,
            device,
            capacity_bytes,
            lane_cpus,
        }))
    }

    fn capacity_bytes(&self) -> u64 {
        match self {
            Self::File(backend) => backend.capacity_bytes,
            Self::Arena(backend) => backend.capacity_bytes,
        }
    }

    fn channels(&self) -> usize {
        match self {
            Self::File(_) => 1,
            Self::Arena(backend) => backend.arena.channels() as usize,
        }
    }

    fn lane_cpus(&self) -> &[usize] {
        match self {
            Self::File(_) => &[],
            Self::Arena(backend) => &backend.lane_cpus,
        }
    }

    fn allocate_write(&self, lane: usize, len: usize) -> io::Result<Blocks> {
        if len == 0 || len % BLOCK_SIZE as usize != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write length is not a non-zero 4 KiB multiple",
            ));
        }
        match self {
            Self::File(_) => Ok(Blocks::Bytes(vec![0; len])),
            Self::Arena(backend) => {
                let mut buffers = Vec::with_capacity(len / BLOCK_SIZE as usize);
                for _ in 0..len / BLOCK_SIZE as usize {
                    buffers.push(allocate_arena(
                        &backend.arena,
                        &backend.device,
                        lane as u32,
                    )?);
                }
                Ok(Blocks::Arena(buffers))
            }
        }
    }

    fn read_blocks(&self, lane: usize, lba: u64, blocks: u32) -> io::Result<Blocks> {
        self.check_range(lba, blocks)?;
        match self {
            Self::File(backend) => {
                let mut bytes = vec![0; blocks as usize * BLOCK_SIZE as usize];
                backend
                    .file
                    .read_exact_at(&mut bytes, lba * u64::from(BLOCK_SIZE))?;
                Ok(Blocks::Bytes(bytes))
            }
            Self::Arena(backend) => {
                let mut buffers = Vec::with_capacity(blocks as usize);
                for index in 0..blocks {
                    let mut buffer = allocate_arena(&backend.arena, &backend.device, lane as u32)?;
                    buffer.read_at(
                        &backend.device,
                        (lba + u64::from(index)) * u64::from(BLOCK_SIZE),
                    )?;
                    buffers.push(buffer);
                }
                Ok(Blocks::Arena(buffers))
            }
        }
    }

    fn write_blocks(&self, lba: u64, mut blocks: Blocks) -> io::Result<()> {
        let count = u32::try_from(blocks.len() / BLOCK_SIZE as usize)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "write too large"))?;
        self.check_range(lba, count)?;
        match (self, &mut blocks) {
            (Self::File(backend), Blocks::Bytes(bytes)) => backend
                .file
                .write_all_at(bytes, lba * u64::from(BLOCK_SIZE)),
            (Self::Arena(backend), Blocks::Arena(buffers)) => {
                for (index, buffer) in buffers.iter_mut().enumerate() {
                    buffer.write_at(
                        &backend.device,
                        (lba + index as u64) * u64::from(BLOCK_SIZE),
                    )?;
                }
                Ok(())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "backend payload type mismatch",
            )),
        }
    }

    fn flush(&self) -> io::Result<()> {
        match self {
            Self::File(backend) => backend.file.sync_all(),
            Self::Arena(backend) => backend.device.sync_all(),
        }
    }

    fn check_range(&self, lba: u64, blocks: u32) -> io::Result<()> {
        let end = lba
            .checked_add(u64::from(blocks))
            .and_then(|blocks| blocks.checked_mul(u64::from(BLOCK_SIZE)))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "LBA overflow"))?;
        if blocks == 0 || end > self.capacity_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SCSI transfer is outside the volume",
            ));
        }
        Ok(())
    }
}

fn validate_capacity(capacity_bytes: u64) -> io::Result<()> {
    if capacity_bytes == 0 || capacity_bytes % u64::from(BLOCK_SIZE) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "capacity must be a non-zero multiple of 4096 bytes",
        ));
    }
    Ok(())
}

fn allocate_arena(
    arena: &ZcnblkAppArena,
    device: &File,
    lane: u32,
) -> io::Result<ZcnblkAppArenaBuffer> {
    match arena.allocate(lane) {
        Ok(buffer) => Ok(buffer),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            // Pressure is exceptional. A barrier frees transferred dirty
            // leases without adding any operation to the ordinary hot path.
            device.sync_all()?;
            arena.allocate(lane)
        }
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy)]
struct Command {
    itt: u32,
    lun: u64,
    expected: u32,
    cmd_sn: u32,
}

enum LaneTask {
    Read {
        command: Command,
        lba: u64,
        blocks: u32,
    },
    Write {
        command: Command,
        lba: u64,
        blocks: Blocks,
        fua: bool,
    },
    Barrier(Sender<io::Result<()>>),
}

enum Completion {
    Data {
        command: Command,
        payload: Blocks,
    },
    Good(Command),
    Check(Command, Vec<u8>),
    R2t {
        command: Command,
        offset: u32,
        len: u32,
    },
    Text {
        itt: u32,
        data: Vec<u8>,
    },
    Nop {
        itt: u32,
        ttt: u32,
        data: Vec<u8>,
    },
    Logout {
        itt: u32,
        ack: Sender<()>,
    },
}

struct PendingWrite {
    command: Command,
    lba: u64,
    fua: bool,
    received: usize,
    blocks: Blocks,
}

struct Session {
    discovery: bool,
    stat_sn: u32,
    exp_cmd_sn: u32,
}

struct TargetSessions {
    lane_mask: AtomicU32,
    valid_lane_mask: u32,
    lane_senders: Mutex<Vec<Option<SyncSender<LaneTask>>>>,
}

impl TargetSessions {
    fn new(lanes: usize) -> io::Result<Arc<Self>> {
        if lanes == 0 || lanes > u32::BITS as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "iSCSI session sharding supports 1..=32 edge lanes",
            ));
        }
        Ok(Arc::new(Self {
            lane_mask: AtomicU32::new(0),
            valid_lane_mask: if lanes == u32::BITS as usize {
                u32::MAX
            } else {
                (1u32 << lanes) - 1
            },
            lane_senders: Mutex::new(vec![None; lanes]),
        }))
    }

    fn acquire(self: &Arc<Self>) -> io::Result<SessionLane> {
        loop {
            let current = self.lane_mask.load(Ordering::Acquire);
            let available = self.valid_lane_mask & !current;
            if available == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "all iSCSI edge-lane session shards are occupied",
                ));
            }
            let lane = available.trailing_zeros() as usize;
            let bit = 1u32 << lane;
            if self
                .lane_mask
                .compare_exchange_weak(current, current | bit, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(SessionLane {
                    sessions: Arc::clone(self),
                    lane,
                });
            }
        }
    }

    fn register(&self, lane: usize, sender: SyncSender<LaneTask>) -> io::Result<()> {
        let mut senders = self
            .lane_senders
            .lock()
            .map_err(|_| io::Error::other("iSCSI session registry poisoned"))?;
        senders[lane] = Some(sender);
        Ok(())
    }

    fn barrier_all(&self) -> io::Result<()> {
        let senders = self
            .lane_senders
            .lock()
            .map_err(|_| io::Error::other("iSCSI session registry poisoned"))?
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        let (ack_tx, ack_rx) = mpsc::channel();
        for sender in &senders {
            sender
                .send(LaneTask::Barrier(ack_tx.clone()))
                .map_err(channel_closed)?;
        }
        drop(ack_tx);
        for _ in 0..senders.len() {
            ack_rx.recv().map_err(channel_closed)??;
        }
        Ok(())
    }
}

struct SessionLane {
    sessions: Arc<TargetSessions>,
    lane: usize,
}

impl Drop for SessionLane {
    fn drop(&mut self) {
        if let Ok(mut senders) = self.sessions.lane_senders.lock() {
            senders[self.lane] = None;
        }
        self.sessions
            .lane_mask
            .fetch_and(!(1u32 << self.lane), Ordering::Release);
    }
}

fn update_exp_cmd(exp: &AtomicU32, header: Header) {
    if header.immediate() {
        return;
    }
    let next = header.u32(24).wrapping_add(1);
    exp.fetch_max(next, Ordering::Release);
}

fn serial_fields(header: &mut Header, stat_sn: u32, exp_cmd_sn: u32) {
    header.put_u32(24, stat_sn);
    header.put_u32(28, exp_cmd_sn);
    header.put_u32(32, exp_cmd_sn.wrapping_add(COMMAND_WINDOW - 1));
}

fn login(stream: &mut TcpStream, target: &str) -> io::Result<Session> {
    let mut stat_sn = 0u32;
    let mut discovery = false;
    let mut target_seen = false;
    loop {
        let request = read_header(stream)?;
        if request.opcode() != OP_LOGIN_REQUEST {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "first iSCSI PDU is not a Login Request",
            ));
        }
        let data = read_small_data(stream, request)?;
        let mut response_text = Vec::new();
        let current_stage = (request.0[1] >> 2) & 0x3;
        let next_stage = request.0[1] & 0x3;
        let transit = request.0[1] & 0x80 != 0;
        for (key, value) in text_pairs(&data) {
            match key {
                "SessionType" => discovery = value == "Discovery",
                "TargetName" => target_seen = value == target,
                "InitiatorName" | "InitiatorAlias" => {}
                "AuthMethod" => append_text(&mut response_text, key, "None"),
                "HeaderDigest" | "DataDigest" => append_text(&mut response_text, key, "None"),
                "MaxConnections" => append_text(&mut response_text, key, "1"),
                "InitialR2T" => append_text(&mut response_text, key, "Yes"),
                "ImmediateData" => append_text(&mut response_text, key, "Yes"),
                "MaxRecvDataSegmentLength" => {
                    append_text(&mut response_text, key, &MAX_SEGMENT.to_string())
                }
                "MaxBurstLength" | "FirstBurstLength" => {
                    let offered = value.parse::<u32>().unwrap_or(MAX_SEGMENT);
                    append_text(
                        &mut response_text,
                        key,
                        &offered.min(MAX_SEGMENT).to_string(),
                    );
                }
                "DefaultTime2Wait" => append_text(&mut response_text, key, "2"),
                "DefaultTime2Retain" => append_text(&mut response_text, key, "0"),
                "MaxOutstandingR2T" => append_text(&mut response_text, key, "1"),
                "DataPDUInOrder" | "DataSequenceInOrder" => {
                    append_text(&mut response_text, key, "Yes")
                }
                "ErrorRecoveryLevel" => append_text(&mut response_text, key, "0"),
                "IFMarker" | "OFMarker" => append_text(&mut response_text, key, "No"),
                "RDMAExtensions" => append_text(&mut response_text, key, "No"),
                _ => append_text(&mut response_text, key, "NotUnderstood"),
            }
        }
        if current_stage == 1
            && !response_text
                .windows(22)
                .any(|value| value == b"TargetPortalGroupTag=")
        {
            append_text(&mut response_text, "TargetPortalGroupTag", "1");
        }
        if !discovery && current_stage != 0 && !target_seen {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "normal-session login did not name this target",
            ));
        }

        let final_response = transit && next_stage == 3;
        let mut response = Header::zeroed(OP_LOGIN_RESPONSE);
        response.0[1] = (current_stage << 2) | if final_response { 0x80 | 3 } else { 0 };
        response.0[2] = 0;
        response.0[3] = 0;
        response.0[8..14].copy_from_slice(&request.0[8..14]);
        if final_response {
            response.put_u16(14, 1);
        } else {
            response.put_u16(14, request.u16(14));
        }
        response.put_u32(16, request.u32(16));
        response.put_u32(24, stat_sn);
        response.put_u32(28, request.u32(24));
        response.put_u32(32, request.u32(24).wrapping_add(COMMAND_WINDOW - 1));
        response.set_data_len(response_text.len())?;
        write_bytes_pdu(stream, &response, &response_text)?;
        stat_sn = stat_sn.wrapping_add(1);
        if final_response {
            return Ok(Session {
                discovery,
                stat_sn,
                exp_cmd_sn: request.u32(24),
            });
        }
    }
}

fn write_bytes_pdu(stream: &mut TcpStream, header: &Header, data: &[u8]) -> io::Result<()> {
    let padding = [0u8; 3];
    let pad_len = (4 - data.len() % 4) % 4;
    write_all_vectored(stream, &[&header.0, data, &padding[..pad_len]])
}

fn write_blocks_pdu(stream: &mut TcpStream, header: &Header, blocks: &Blocks) -> io::Result<()> {
    match blocks {
        Blocks::Bytes(bytes) => write_bytes_pdu(stream, header, bytes),
        Blocks::Arena(buffers) => {
            let mut slices = Vec::with_capacity(buffers.len() + 1);
            slices.push(&header.0[..]);
            for buffer in buffers {
                slices.push(buffer.as_slice()?);
            }
            write_all_vectored(stream, &slices)
        }
    }
}

fn write_all_vectored(stream: &mut TcpStream, slices: &[&[u8]]) -> io::Result<()> {
    let slices = slices
        .iter()
        .copied()
        .filter(|slice| !slice.is_empty())
        .collect::<Vec<_>>();
    let mut index = 0;
    let mut offset = 0;
    while index < slices.len() {
        let iov = slices[index..]
            .iter()
            .enumerate()
            .map(|(relative, slice)| {
                let start = if relative == 0 { offset } else { 0 };
                IoSlice::new(&slice[start..])
            })
            .collect::<Vec<_>>();
        let written = stream.write_vectored(&iov)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "iSCSI socket closed",
            ));
        }
        let mut remaining = written;
        while index < slices.len() {
            let available = slices[index].len() - offset;
            if remaining < available {
                offset += remaining;
                break;
            }
            remaining -= available;
            index += 1;
            offset = 0;
            if remaining == 0 {
                break;
            }
        }
    }
    Ok(())
}

fn writer_loop(
    mut stream: TcpStream,
    receiver: Receiver<Completion>,
    exp_cmd_sn: Arc<AtomicU32>,
    mut stat_sn: u32,
) -> io::Result<()> {
    while let Ok(completion) = receiver.recv() {
        let exp = exp_cmd_sn.load(Ordering::Acquire);
        match completion {
            Completion::Data { command, payload } => {
                let mut header = Header::zeroed(OP_DATA_IN);
                header.0[1] = 0x81;
                header.0[3] = 0;
                header.put_u64(8, command.lun);
                header.put_u32(16, command.itt);
                header.put_u32(20, u32::MAX);
                serial_fields(&mut header, stat_sn, exp);
                header.put_u32(36, 0);
                header.put_u32(40, 0);
                header.set_data_len(payload.len())?;
                if payload.len() < command.expected as usize {
                    header.0[1] |= 0x02;
                    header.put_u32(44, command.expected - payload.len() as u32);
                }
                write_blocks_pdu(&mut stream, &header, &payload)?;
            }
            Completion::Good(command) => {
                let mut header = Header::zeroed(OP_SCSI_RESPONSE);
                header.0[1] = 0x80;
                header.put_u32(16, command.itt);
                serial_fields(&mut header, stat_sn, exp);
                write_bytes_pdu(&mut stream, &header, &[])?;
            }
            Completion::Check(command, sense) => {
                let mut data = Vec::with_capacity(sense.len() + 2);
                data.extend_from_slice(&(sense.len() as u16).to_be_bytes());
                data.extend_from_slice(&sense);
                let mut header = Header::zeroed(OP_SCSI_RESPONSE);
                header.0[1] = 0x80;
                header.0[3] = 0x02;
                header.put_u32(16, command.itt);
                serial_fields(&mut header, stat_sn, exp);
                header.set_data_len(data.len())?;
                write_bytes_pdu(&mut stream, &header, &data)?;
            }
            Completion::R2t {
                command,
                offset,
                len,
            } => {
                let mut header = Header::zeroed(OP_R2T);
                header.0[1] = 0x80;
                header.put_u64(8, command.lun);
                header.put_u32(16, command.itt);
                header.put_u32(20, command.itt);
                serial_fields(&mut header, stat_sn, exp);
                header.put_u32(36, 0);
                header.put_u32(40, offset);
                header.put_u32(44, len);
                write_bytes_pdu(&mut stream, &header, &[])?;
            }
            Completion::Text { itt, data } => {
                let mut header = Header::zeroed(OP_TEXT_RESPONSE);
                header.0[1] = 0x80;
                header.put_u32(16, itt);
                header.put_u32(20, u32::MAX);
                serial_fields(&mut header, stat_sn, exp);
                header.set_data_len(data.len())?;
                write_bytes_pdu(&mut stream, &header, &data)?;
            }
            Completion::Nop { itt, ttt, data } => {
                let mut header = Header::zeroed(OP_NOP_IN);
                header.0[1] = 0x80;
                header.put_u32(16, itt);
                header.put_u32(20, ttt);
                serial_fields(&mut header, stat_sn, exp);
                header.set_data_len(data.len())?;
                write_bytes_pdu(&mut stream, &header, &data)?;
            }
            Completion::Logout { itt, ack } => {
                let mut header = Header::zeroed(OP_LOGOUT_RESPONSE);
                header.0[1] = 0x80;
                header.put_u32(16, itt);
                serial_fields(&mut header, stat_sn, exp);
                write_bytes_pdu(&mut stream, &header, &[])?;
                let _ = ack.send(());
            }
        }
        stat_sn = stat_sn.wrapping_add(1);
    }
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

fn spawn_lane_workers(
    backend: Arc<Backend>,
    completions: Sender<Completion>,
    lane_ids: &[usize],
) -> (Vec<SyncSender<LaneTask>>, Vec<thread::JoinHandle<()>>) {
    let mut senders = Vec::with_capacity(lane_ids.len());
    let mut handles = Vec::with_capacity(lane_ids.len());
    for &lane in lane_ids {
        let (sender, receiver) = mpsc::sync_channel::<LaneTask>(COMMAND_WINDOW as usize);
        let lane_backend = Arc::clone(&backend);
        let lane_completions = completions.clone();
        let cpu = backend.lane_cpus().get(lane).copied();
        handles.push(thread::spawn(move || {
            if let Some(cpu) = cpu {
                if let Err(error) = pin_current_thread(cpu) {
                    eprintln!("zciscsi-target: lane={lane} cpu={cpu} pin_error={error}");
                    return;
                }
            }
            let result = match lane_backend.as_ref() {
                Backend::Arena(_) => run_arena_lane(
                    lane,
                    &lane_backend,
                    receiver,
                    &lane_completions,
                ),
                Backend::File(_) => {
                    Ok(run_sync_lane(lane, &lane_backend, receiver, &lane_completions))
                }
            };
            let counts = result.unwrap_or_else(|error| {
                eprintln!("zciscsi-target: lane={lane} io_uring_error={error}");
                LaneCounts::default()
            });
            eprintln!(
                "zciscsi-target-lane: lane={lane} cpu={} read_blocks={reads} write_blocks={writes} barriers_or_fua={flushes}",
                cpu.map_or_else(|| "unpinned-test-only".to_string(), |value| value.to_string()),
                reads = counts.reads,
                writes = counts.writes,
                flushes = counts.flushes,
            );
        }));
        senders.push(sender);
    }
    (senders, handles)
}

#[derive(Default)]
struct LaneCounts {
    reads: u64,
    writes: u64,
    flushes: u64,
}

fn run_sync_lane(
    lane: usize,
    backend: &Arc<Backend>,
    receiver: Receiver<LaneTask>,
    completions: &Sender<Completion>,
) -> LaneCounts {
    let mut counts = LaneCounts::default();
    while let Ok(task) = receiver.recv() {
        complete_sync_task(lane, backend, completions, task, &mut counts);
    }
    counts
}

fn complete_sync_task(
    lane: usize,
    backend: &Arc<Backend>,
    completions: &Sender<Completion>,
    task: LaneTask,
    counts: &mut LaneCounts,
) {
    match task {
        LaneTask::Read {
            command,
            lba,
            blocks,
        } => match backend.read_blocks(lane, lba, blocks) {
            Ok(payload) => {
                counts.reads += u64::from(blocks);
                let _ = completions.send(Completion::Data { command, payload });
            }
            Err(_) => {
                let _ = completions.send(Completion::Check(command, sense_medium_error()));
            }
        },
        LaneTask::Write {
            command,
            lba,
            blocks,
            fua,
        } => {
            let count = blocks.len() / BLOCK_SIZE as usize;
            let result = backend
                .write_blocks(lba, blocks)
                .and_then(|()| if fua { backend.flush() } else { Ok(()) });
            match result {
                Ok(()) => {
                    counts.writes += count as u64;
                    counts.flushes += u64::from(fua);
                    let _ = completions.send(Completion::Good(command));
                }
                Err(_) => {
                    let _ = completions.send(Completion::Check(command, sense_medium_error()));
                }
            }
        }
        LaneTask::Barrier(ack) => {
            counts.flushes += 1;
            let _ = ack.send(Ok(()));
        }
    }
}

#[derive(Clone, Copy)]
enum ArenaIoKind {
    Read,
    Write { fua: bool },
}

struct ArenaInFlight {
    command: Command,
    blocks: Blocks,
    block_count: usize,
    remaining: usize,
    failed: bool,
    kind: ArenaIoKind,
}

fn run_arena_lane(
    lane: usize,
    backend: &Arc<Backend>,
    receiver: Receiver<LaneTask>,
    completions: &Sender<Completion>,
) -> io::Result<LaneCounts> {
    let arena = match backend.as_ref() {
        Backend::Arena(arena) => arena,
        Backend::File(_) => unreachable!(),
    };
    let mut ring = ZcnblkAppArenaIoRing::new((LANE_PIPELINE * 2) as u32)?;
    let mut inflight = (0..LANE_PIPELINE)
        .map(|_| None::<ArenaInFlight>)
        .collect::<Vec<_>>();
    let mut outstanding_blocks = 0usize;
    let mut deferred = None::<LaneTask>;
    let mut barrier = None::<Sender<io::Result<()>>>;
    let mut disconnected = false;
    let mut counts = LaneCounts::default();

    loop {
        while barrier.is_none() && outstanding_blocks < LANE_PIPELINE {
            let task = if let Some(task) = deferred.take() {
                task
            } else {
                match receiver.try_recv() {
                    Ok(task) => task,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            };
            if let LaneTask::Barrier(ack) = task {
                barrier = Some(ack);
                break;
            }
            let task_blocks = match &task {
                LaneTask::Read { blocks, .. } => *blocks as usize,
                LaneTask::Write { blocks, .. } => blocks.len() / BLOCK_SIZE as usize,
                LaneTask::Barrier(_) => unreachable!(),
            };
            if task_blocks > LANE_PIPELINE {
                if outstanding_blocks == 0 {
                    complete_sync_task(lane, backend, completions, task, &mut counts);
                    continue;
                }
                deferred = Some(task);
                break;
            }
            if outstanding_blocks + task_blocks > LANE_PIPELINE {
                deferred = Some(task);
                break;
            }
            let slot = inflight
                .iter()
                .position(Option::is_none)
                .expect("arena command slot available when block window has room");
            if let Some(request) =
                queue_arena_task(lane, backend, arena, &mut ring, completions, slot, task)?
            {
                outstanding_blocks += request.remaining;
                inflight[slot] = Some(request);
            }
        }

        ring.submit()?;
        if outstanding_blocks == 0 {
            if let Some(ack) = barrier.take() {
                counts.flushes += 1;
                let _ = ack.send(Ok(()));
                continue;
            }
            if disconnected {
                break;
            }
            deferred = match receiver.recv() {
                Ok(task) => Some(task),
                Err(_) => {
                    disconnected = true;
                    None
                }
            };
            continue;
        }

        let completion = ring
            .try_completion()
            .map(Ok)
            .unwrap_or_else(|| ring.wait_completion())?;
        finish_arena_io(
            backend,
            completions,
            completion,
            &mut inflight,
            &mut outstanding_blocks,
            &mut counts,
        );
        while let Some(completion) = ring.try_completion() {
            finish_arena_io(
                backend,
                completions,
                completion,
                &mut inflight,
                &mut outstanding_blocks,
                &mut counts,
            );
        }
    }
    Ok(counts)
}

fn queue_arena_task(
    lane: usize,
    backend: &Arc<Backend>,
    arena: &ArenaBackend,
    ring: &mut ZcnblkAppArenaIoRing,
    completions: &Sender<Completion>,
    slot: usize,
    task: LaneTask,
) -> io::Result<Option<ArenaInFlight>> {
    let (command, lba, mut blocks, kind) = match task {
        LaneTask::Read {
            command,
            lba,
            blocks,
        } => {
            if backend.check_range(lba, blocks).is_err() {
                let _ = completions.send(Completion::Check(command, sense_invalid_field()));
                return Ok(None);
            }
            let mut payload = Vec::with_capacity(blocks as usize);
            for _ in 0..blocks {
                payload.push(allocate_arena(&arena.arena, &arena.device, lane as u32)?);
            }
            (command, lba, Blocks::Arena(payload), ArenaIoKind::Read)
        }
        LaneTask::Write {
            command,
            lba,
            blocks,
            fua,
        } => {
            let count = u32::try_from(blocks.len() / BLOCK_SIZE as usize)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "write too large"))?;
            if backend.check_range(lba, count).is_err() {
                let _ = completions.send(Completion::Check(command, sense_invalid_field()));
                return Ok(None);
            }
            (command, lba, blocks, ArenaIoKind::Write { fua })
        }
        LaneTask::Barrier(_) => unreachable!(),
    };
    let block_count = blocks.len() / BLOCK_SIZE as usize;
    let buffers = match &mut blocks {
        Blocks::Arena(buffers) => buffers,
        Blocks::Bytes(_) => unreachable!(),
    };
    let mut remaining = 0usize;
    let mut failed = false;
    for (index, buffer) in buffers.iter_mut().enumerate() {
        let offset = (lba + index as u64) * u64::from(BLOCK_SIZE);
        let result = match kind {
            ArenaIoKind::Read => ring.queue_read(&arena.device, buffer, offset, slot as u64),
            ArenaIoKind::Write { .. } => {
                ring.queue_write(&arena.device, buffer, offset, slot as u64)
            }
        };
        if result.is_err() {
            failed = true;
            break;
        }
        remaining += 1;
    }
    if remaining == 0 {
        let _ = completions.send(Completion::Check(command, sense_medium_error()));
        return Ok(None);
    }
    Ok(Some(ArenaInFlight {
        command,
        blocks,
        block_count,
        remaining,
        failed,
        kind,
    }))
}

fn finish_arena_io(
    backend: &Arc<Backend>,
    completions: &Sender<Completion>,
    completion: ZcnblkAppArenaIoCompletion,
    inflight: &mut [Option<ArenaInFlight>],
    outstanding_blocks: &mut usize,
    counts: &mut LaneCounts,
) {
    let slot = completion.user_data as usize;
    let Some(request) = inflight.get_mut(slot).and_then(Option::as_mut) else {
        return;
    };
    request.failed |= completion.result != BLOCK_SIZE as i32;
    request.remaining = request.remaining.saturating_sub(1);
    *outstanding_blocks = outstanding_blocks.saturating_sub(1);
    if request.remaining != 0 {
        return;
    }
    let mut request = inflight[slot].take().unwrap();
    if matches!(request.kind, ArenaIoKind::Read) {
        if let Blocks::Arena(buffers) = &mut request.blocks {
            for buffer in buffers {
                if buffer.wait_reacquire(Duration::from_secs(5)).is_err() {
                    request.failed = true;
                }
            }
        }
    }
    if let ArenaIoKind::Write { fua: true } = request.kind {
        counts.flushes += 1;
        if backend.flush().is_err() {
            request.failed = true;
        }
    }
    if request.failed {
        let _ = completions.send(Completion::Check(request.command, sense_medium_error()));
        return;
    }
    match request.kind {
        ArenaIoKind::Read => {
            counts.reads += request.block_count as u64;
            let _ = completions.send(Completion::Data {
                command: request.command,
                payload: request.blocks,
            });
        }
        ArenaIoKind::Write { .. } => {
            counts.writes += request.block_count as u64;
            let _ = completions.send(Completion::Good(request.command));
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    backend: Arc<Backend>,
    target: Arc<str>,
    advertised: Arc<str>,
    sessions: Arc<TargetSessions>,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let session = login(&mut stream, &target)?;
    let session_lane = if session.discovery {
        None
    } else {
        Some(sessions.acquire()?)
    };
    let lane_ids = session_lane
        .as_ref()
        .map_or_else(Vec::new, |lease| vec![lease.lane]);
    let exp_cmd_sn = Arc::new(AtomicU32::new(session.exp_cmd_sn));
    let (completion_tx, completion_rx) = mpsc::channel();
    let writer_stream = stream.try_clone()?;
    let writer_exp = Arc::clone(&exp_cmd_sn);
    let writer = thread::spawn(move || {
        if let Err(error) = writer_loop(writer_stream, completion_rx, writer_exp, session.stat_sn) {
            eprintln!("zciscsi-target: writer_error={error}");
        }
    });
    let (lane_senders, lane_handles) =
        spawn_lane_workers(Arc::clone(&backend), completion_tx.clone(), &lane_ids);
    if let Some(lease) = &session_lane {
        sessions.register(lease.lane, lane_senders[0].clone())?;
    }
    let mut pending = HashMap::<u32, PendingWrite>::new();

    loop {
        let header = match read_header(&mut stream) {
            Ok(header) => header,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        };
        match header.opcode() {
            OP_NOP_OUT => {
                let data = read_small_data(&mut stream, header)?;
                update_exp_cmd(&exp_cmd_sn, header);
                completion_tx
                    .send(Completion::Nop {
                        itt: header.u32(16),
                        ttt: header.u32(20),
                        data,
                    })
                    .map_err(channel_closed)?;
            }
            OP_TEXT_REQUEST => {
                let data = read_small_data(&mut stream, header)?;
                update_exp_cmd(&exp_cmd_sn, header);
                let mut response = Vec::new();
                if text_pairs(&data).any(|(key, _)| key == "SendTargets") {
                    append_text(&mut response, "TargetName", &target);
                    append_text(&mut response, "TargetAddress", &format!("{advertised},1"));
                }
                completion_tx
                    .send(Completion::Text {
                        itt: header.u32(16),
                        data: response,
                    })
                    .map_err(channel_closed)?;
            }
            OP_SCSI_COMMAND if !session.discovery => {
                handle_scsi_command(
                    &mut stream,
                    header,
                    &backend,
                    &lane_senders,
                    &completion_tx,
                    &exp_cmd_sn,
                    &mut pending,
                    &sessions,
                )?;
            }
            OP_DATA_OUT if !session.discovery => {
                let len = header.data_len();
                let itt = header.u32(16);
                let offset = header.u32(40) as usize;
                let pending_write = pending.get_mut(&itt).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "Data-Out has no pending WRITE")
                })?;
                pending_write.blocks.read_from(&mut stream, offset, len)?;
                read_padding(&mut stream, len)?;
                pending_write.received = pending_write.received.max(offset + len);
                if pending_write.received == pending_write.blocks.len() {
                    let write = pending.remove(&itt).unwrap();
                    let lane = write.command.cmd_sn as usize % lane_senders.len();
                    lane_senders[lane]
                        .send(LaneTask::Write {
                            command: write.command,
                            lba: write.lba,
                            blocks: write.blocks,
                            fua: write.fua,
                        })
                        .map_err(channel_closed)?;
                }
            }
            OP_LOGOUT_REQUEST => {
                let _ = read_small_data(&mut stream, header)?;
                update_exp_cmd(&exp_cmd_sn, header);
                let (ack_tx, ack_rx) = mpsc::channel();
                completion_tx
                    .send(Completion::Logout {
                        itt: header.u32(16),
                        ack: ack_tx,
                    })
                    .map_err(channel_closed)?;
                let _ = ack_rx.recv();
                break;
            }
            _ => {
                let _ = read_small_data(&mut stream, header)?;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported iSCSI opcode 0x{:02x}", header.opcode()),
                ));
            }
        }
    }

    drop(lane_senders);
    for handle in lane_handles {
        let _ = handle.join();
    }
    drop(completion_tx);
    let _ = writer.join();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_scsi_command(
    stream: &mut TcpStream,
    header: Header,
    backend: &Arc<Backend>,
    lanes: &[SyncSender<LaneTask>],
    completions: &Sender<Completion>,
    exp_cmd_sn: &AtomicU32,
    pending: &mut HashMap<u32, PendingWrite>,
    sessions: &TargetSessions,
) -> io::Result<()> {
    update_exp_cmd(exp_cmd_sn, header);
    let command = Command {
        itt: header.u32(16),
        lun: header.u64(8),
        expected: header.u32(20),
        cmd_sn: header.u32(24),
    };
    let mut cdb = [0u8; 16];
    cdb.copy_from_slice(&header.0[32..48]);
    let lane = command.cmd_sn as usize % lanes.len();
    let data_len = header.data_len();

    if command.lun != 0 {
        let _ = read_small_data(stream, header)?;
        completions
            .send(Completion::Check(command, sense_invalid_lun()))
            .map_err(channel_closed)?;
        return Ok(());
    }

    match cdb[0] {
        0x08 | 0x28 | 0x88 => {
            let _ = read_small_data(stream, header)?;
            let (lba, blocks) = parse_rw(&cdb)?;
            if u64::from(blocks) * u64::from(BLOCK_SIZE) != u64::from(command.expected) {
                completions
                    .send(Completion::Check(command, sense_invalid_field()))
                    .map_err(channel_closed)?;
            } else {
                lanes[lane]
                    .send(LaneTask::Read {
                        command,
                        lba,
                        blocks,
                    })
                    .map_err(channel_closed)?;
            }
        }
        0x0a | 0x2a | 0x8a => {
            let (lba, blocks) = parse_rw(&cdb)?;
            let expected = blocks as usize * BLOCK_SIZE as usize;
            if expected != command.expected as usize || expected == 0 || data_len > expected {
                let _ = read_small_data(stream, header)?;
                completions
                    .send(Completion::Check(command, sense_invalid_field()))
                    .map_err(channel_closed)?;
                return Ok(());
            }
            let mut blocks_payload = backend.allocate_write(lane, expected)?;
            blocks_payload.read_from(stream, 0, data_len)?;
            read_padding(stream, data_len)?;
            let fua = cdb[1] & 0x08 != 0;
            if data_len == expected {
                lanes[lane]
                    .send(LaneTask::Write {
                        command,
                        lba,
                        blocks: blocks_payload,
                        fua,
                    })
                    .map_err(channel_closed)?;
            } else {
                pending.insert(
                    command.itt,
                    PendingWrite {
                        command,
                        lba,
                        fua,
                        received: data_len,
                        blocks: blocks_payload,
                    },
                );
                completions
                    .send(Completion::R2t {
                        command,
                        offset: data_len as u32,
                        len: (expected - data_len) as u32,
                    })
                    .map_err(channel_closed)?;
            }
        }
        0x35 | 0x91 => {
            let _ = read_small_data(stream, header)?;
            match sessions.barrier_all().and_then(|()| backend.flush()) {
                Ok(()) => completions
                    .send(Completion::Good(command))
                    .map_err(channel_closed)?,
                Err(_) => completions
                    .send(Completion::Check(command, sense_medium_error()))
                    .map_err(channel_closed)?,
            }
        }
        _ => {
            let _ = read_small_data(stream, header)?;
            match scsi_control(&cdb, backend.capacity_bytes()) {
                Ok(Some(data)) => completions
                    .send(Completion::Data {
                        command,
                        payload: Blocks::Bytes(data),
                    })
                    .map_err(channel_closed)?,
                Ok(None) => completions
                    .send(Completion::Good(command))
                    .map_err(channel_closed)?,
                Err(sense) => completions
                    .send(Completion::Check(command, sense))
                    .map_err(channel_closed)?,
            }
        }
    }
    Ok(())
}

fn parse_rw(cdb: &[u8; 16]) -> io::Result<(u64, u32)> {
    let pair = match cdb[0] {
        0x08 | 0x0a => {
            let lba =
                (u32::from(cdb[1] & 0x1f) << 16) | (u32::from(cdb[2]) << 8) | u32::from(cdb[3]);
            let blocks = if cdb[4] == 0 { 256 } else { u32::from(cdb[4]) };
            (u64::from(lba), blocks)
        }
        0x28 | 0x2a => {
            let lba = u32::from_be_bytes(cdb[2..6].try_into().unwrap());
            let blocks = u16::from_be_bytes(cdb[7..9].try_into().unwrap());
            (u64::from(lba), u32::from(blocks))
        }
        0x88 | 0x8a => {
            let lba = u64::from_be_bytes(cdb[2..10].try_into().unwrap());
            let blocks = u32::from_be_bytes(cdb[10..14].try_into().unwrap());
            (lba, blocks)
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a READ/WRITE CDB",
            ));
        }
    };
    if pair.1 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zero-block transfer",
        ));
    }
    Ok(pair)
}

fn scsi_control(cdb: &[u8; 16], capacity_bytes: u64) -> Result<Option<Vec<u8>>, Vec<u8>> {
    match cdb[0] {
        0x00 | 0x1b | 0x1e | 0x2f | 0x8f => Ok(None),
        0x03 => Ok(Some(request_sense(cdb[4] as usize))),
        0x12 => inquiry(cdb).map(Some),
        0x1a => Ok(Some(mode_sense_6(cdb))),
        0x5a => Ok(Some(mode_sense_10(cdb))),
        0x25 => Ok(Some(read_capacity_10(capacity_bytes))),
        0x9e if cdb[1] & 0x1f == 0x10 => Ok(Some(read_capacity_16(capacity_bytes))),
        0xa0 => Ok(Some(report_luns(cdb))),
        _ => Err(sense_invalid_command()),
    }
}

fn inquiry(cdb: &[u8; 16]) -> Result<Vec<u8>, Vec<u8>> {
    let allocation = u16::from_be_bytes([cdb[3], cdb[4]]) as usize;
    let mut data = if cdb[1] & 1 == 0 {
        let mut value = vec![0u8; 36];
        value[2] = 0x06;
        value[3] = 0x02;
        value[4] = 31;
        value[7] = 0x02;
        value[8..16].copy_from_slice(b"ZCUTILS ");
        value[16..32].copy_from_slice(b"userspace volume");
        value[32..36].copy_from_slice(b"0.1 ");
        value
    } else {
        match cdb[2] {
            0x00 => vec![0, 0, 0, 3, 0x00, 0x80, 0x83],
            0x80 => {
                let serial = b"anonymous-zc-volume";
                let mut value = vec![0, 0x80, 0, serial.len() as u8];
                value.extend_from_slice(serial);
                value
            }
            0x83 => {
                let identifier = b"ZCUTILS userspace-volume";
                let mut value = vec![
                    0,
                    0x83,
                    0,
                    (identifier.len() + 4) as u8,
                    0x02,
                    0x01,
                    0,
                    identifier.len() as u8,
                ];
                value.extend_from_slice(identifier);
                value
            }
            _ => return Err(sense_invalid_field()),
        }
    };
    data.truncate(allocation.min(data.len()));
    Ok(data)
}

fn request_sense(allocation: usize) -> Vec<u8> {
    let mut data = vec![0u8; 18];
    data[0] = 0x70;
    data[7] = 10;
    data.truncate(allocation.min(data.len()));
    data
}

fn caching_page() -> Vec<u8> {
    let mut page = vec![0u8; 20];
    page[0] = 0x08;
    page[1] = 0x12;
    page[2] = 0x04;
    page
}

fn mode_sense_6(cdb: &[u8; 16]) -> Vec<u8> {
    let page = cdb[2] & 0x3f;
    let mut data = vec![0u8; 4];
    if page == 0x08 || page == 0x3f {
        data.extend_from_slice(&caching_page());
    }
    data[0] = (data.len() - 1) as u8;
    data.truncate((cdb[4] as usize).min(data.len()));
    data
}

fn mode_sense_10(cdb: &[u8; 16]) -> Vec<u8> {
    let page = cdb[2] & 0x3f;
    let mut data = vec![0u8; 8];
    if page == 0x08 || page == 0x3f {
        data.extend_from_slice(&caching_page());
    }
    let length = (data.len() - 2) as u16;
    data[0..2].copy_from_slice(&length.to_be_bytes());
    let allocation = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
    data.truncate(allocation.min(data.len()));
    data
}

fn read_capacity_10(capacity_bytes: u64) -> Vec<u8> {
    let blocks = capacity_bytes / u64::from(BLOCK_SIZE);
    let last = blocks.saturating_sub(1).min(u64::from(u32::MAX)) as u32;
    let mut data = Vec::with_capacity(8);
    data.extend_from_slice(&last.to_be_bytes());
    data.extend_from_slice(&BLOCK_SIZE.to_be_bytes());
    data
}

fn read_capacity_16(capacity_bytes: u64) -> Vec<u8> {
    let mut data = vec![0u8; 32];
    let last = capacity_bytes / u64::from(BLOCK_SIZE) - 1;
    data[0..8].copy_from_slice(&last.to_be_bytes());
    data[8..12].copy_from_slice(&BLOCK_SIZE.to_be_bytes());
    data
}

fn report_luns(cdb: &[u8; 16]) -> Vec<u8> {
    let mut data = vec![0u8; 16];
    data[3] = 8;
    let allocation = u32::from_be_bytes(cdb[6..10].try_into().unwrap()) as usize;
    data.truncate(allocation.min(data.len()));
    data
}

fn fixed_sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
    let mut data = vec![0u8; 18];
    data[0] = 0x70;
    data[2] = key;
    data[7] = 10;
    data[12] = asc;
    data[13] = ascq;
    data
}

fn sense_invalid_command() -> Vec<u8> {
    fixed_sense(0x05, 0x20, 0)
}

fn sense_invalid_field() -> Vec<u8> {
    fixed_sense(0x05, 0x24, 0)
}

fn sense_invalid_lun() -> Vec<u8> {
    fixed_sense(0x05, 0x25, 0)
}

fn sense_medium_error() -> Vec<u8> {
    fixed_sense(0x03, 0x11, 0)
}

fn channel_closed(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, error.to_string())
}

struct Options {
    listen: String,
    advertise: String,
    target: String,
    leaf: Option<PathBuf>,
    arena_socket: Option<PathBuf>,
    zcnblk_device: PathBuf,
    lane_cpus: Vec<usize>,
}

fn usage() -> &'static str {
    "usage: zciscsi-target --listen HOST:PORT [--advertise HOST:PORT] --target IQN \
     (--leaf-file PATH | --arena-socket PATH --zcnblk-device PATH --lane-cpus CPU,CPU,...)"
}

fn parse_options() -> io::Result<Options> {
    let mut args = env::args().skip(1);
    let mut listen = None;
    let mut advertise = None;
    let mut target = None;
    let mut leaf = None;
    let mut arena_socket = None;
    let mut zcnblk_device = PathBuf::from("/dev/zcnblk0");
    let mut lane_cpus = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next(),
            "--advertise" => advertise = args.next(),
            "--target" => target = args.next(),
            "--leaf-file" => leaf = args.next().map(PathBuf::from),
            "--arena-socket" => arena_socket = args.next().map(PathBuf::from),
            "--zcnblk-device" => {
                zcnblk_device = args.next().map(PathBuf::from).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing --zcnblk-device")
                })?;
            }
            "--lane-cpus" => {
                lane_cpus = args
                    .next()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "missing --lane-cpus")
                    })?
                    .split(',')
                    .map(|cpu| {
                        cpu.parse::<usize>().map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidInput, "invalid --lane-cpus")
                        })
                    })
                    .collect::<io::Result<Vec<_>>>()?;
            }
            "--block-size" => {
                let value = args.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing --block-size")
                })?;
                if value != "4096" {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "zciscsi supports only a rigid 4096-byte geometry",
                    ));
                }
            }
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument {arg}\n{}", usage()),
                ));
            }
        }
    }
    let listen = listen.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, usage()))?;
    if leaf.is_some() == arena_socket.is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, usage()));
    }
    if arena_socket.is_some() && lane_cpus.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "arena mode always requires an explicit complete --lane-cpus mapping",
        ));
    }
    if leaf.is_some() && !lane_cpus.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal file test mode has no userspace placement lanes",
        ));
    }
    Ok(Options {
        advertise: advertise.unwrap_or_else(|| listen.clone()),
        listen,
        target: target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, usage()))?,
        leaf,
        arena_socket,
        zcnblk_device,
        lane_cpus,
    })
}

fn run() -> io::Result<()> {
    let options = parse_options()?;
    let (backend, backing, copies) = if let Some(path) = &options.leaf {
        (
            Backend::open_file(path)?,
            format!("terminal-file-test-only:{}", path.display()),
            "protocol-buffered-test-only",
        )
    } else {
        let socket = options.arena_socket.as_ref().unwrap();
        (
            Backend::open_arena(socket, &options.zcnblk_device, options.lane_cpus.clone())?,
            format!(
                "userspace-stage:{} via {}",
                socket.display(),
                options.zcnblk_device.display()
            ),
            "socket-to-arena=0 arena-to-block-edge=0",
        )
    };
    let backend = Arc::new(backend);
    eprintln!(
        "zciscsi-target: implementation=zcutils-rfc7143-from-scratch external_iscsi_library=no listen={} advertise={} target={} capacity_bytes={} logical_block=4096 physical_block=4096 lanes={} lane_to_cpu={:?} lane_io=io_uring lane_qd={} backing={} payload_copies={} placement_owner=downstream-userspace-raid frontend_placement=no mirror_primitive=no",
        options.listen,
        options.advertise,
        options.target,
        backend.capacity_bytes(),
        backend.channels(),
        options.lane_cpus,
        LANE_PIPELINE,
        backing,
        copies,
    );
    let listener = TcpListener::bind(&options.listen)?;
    let sessions = TargetSessions::new(backend.channels())?;
    let target: Arc<str> = Arc::from(options.target);
    let advertised: Arc<str> = Arc::from(options.advertise);
    for incoming in listener.incoming() {
        let stream = incoming?;
        let connection_backend = Arc::clone(&backend);
        let connection_target = Arc::clone(&target);
        let connection_advertised = Arc::clone(&advertised);
        let connection_sessions = Arc::clone(&sessions);
        thread::spawn(move || {
            if let Err(error) = handle_connection(
                stream,
                connection_backend,
                connection_target,
                connection_advertised,
                connection_sessions,
            ) {
                eprintln!("zciscsi-target: connection_error={error}");
            }
        });
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("zciscsi-target: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_supported_rw_cdb_widths_at_4k_geometry() {
        let mut ten = [0u8; 16];
        ten[0] = 0x28;
        ten[2..6].copy_from_slice(&1234u32.to_be_bytes());
        ten[7..9].copy_from_slice(&16u16.to_be_bytes());
        assert_eq!(parse_rw(&ten).unwrap(), (1234, 16));

        let mut sixteen = [0u8; 16];
        sixteen[0] = 0x8a;
        sixteen[2..10].copy_from_slice(&u64::from(u32::MAX).to_be_bytes());
        sixteen[10..14].copy_from_slice(&32u32.to_be_bytes());
        assert_eq!(parse_rw(&sixteen).unwrap(), (u64::from(u32::MAX), 32));
    }

    #[test]
    fn reports_a_rigid_4k_capacity() {
        assert_eq!(
            read_capacity_10(1024 * 1024),
            vec![0, 0, 0, 255, 0, 0, 16, 0]
        );
        let capacity = read_capacity_16(1024 * 1024);
        assert_eq!(u64::from_be_bytes(capacity[0..8].try_into().unwrap()), 255);
        assert_eq!(
            u32::from_be_bytes(capacity[8..12].try_into().unwrap()),
            4096
        );
    }

    #[test]
    fn login_text_has_nul_delimited_key_values() {
        let mut data = Vec::new();
        append_text(&mut data, "HeaderDigest", "None");
        append_text(&mut data, "DataDigest", "None");
        assert_eq!(
            text_pairs(&data).collect::<Vec<_>>(),
            vec![("HeaderDigest", "None"), ("DataDigest", "None")]
        );
    }

    #[test]
    fn arena_mode_rejects_non_4k_capacity() {
        assert!(validate_capacity(4095).is_err());
        assert!(validate_capacity(4096).is_ok());
    }
}
