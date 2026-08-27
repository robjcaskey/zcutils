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
use std::time::{Duration, Instant};

use zcutils::block::zcnblk::{
    ZCNBLK_FRAME_HEADER_LEN, ZCNBLK_OP_BATCH, ZCNBLK_OP_BATCH_RESP, ZCNBLK_OP_READ,
    ZCNBLK_OP_READ_RANGE_RESP, ZCNBLK_OP_READ_RESP, ZCNBLK_OP_SYNC, ZCNBLK_OP_SYNC_ACK,
    ZCNBLK_OP_WRITE, ZCNBLK_OP_WRITE_ACK, ZCNBLK_TOPOLOGY_PORT_LANE, ZCNBLK_TOPOLOGY_VALID,
    ZcnblkFrameHeader, ZcnblkFrameTopology,
};
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
const DATA_IN_BATCH: usize = 64;

const OP_NOP_OUT: u8 = 0x00;
const OP_SCSI_COMMAND: u8 = 0x01;
const OP_TASK_REQUEST: u8 = 0x02;
const OP_LOGIN_REQUEST: u8 = 0x03;
const OP_TEXT_REQUEST: u8 = 0x04;
const OP_DATA_OUT: u8 = 0x05;
const OP_LOGOUT_REQUEST: u8 = 0x06;

const OP_NOP_IN: u8 = 0x20;
const OP_SCSI_RESPONSE: u8 = 0x21;
const OP_TASK_RESPONSE: u8 = 0x22;
const OP_LOGIN_RESPONSE: u8 = 0x23;
const OP_TEXT_RESPONSE: u8 = 0x24;
const OP_DATA_IN: u8 = 0x25;
const OP_LOGOUT_RESPONSE: u8 = 0x26;
const OP_R2T: u8 = 0x31;

const TASK_FUNCTION_LOGICAL_UNIT_RESET: u8 = 5;
const TASK_RESPONSE_COMPLETE: u8 = 0;
const TASK_RESPONSE_LUN_DOES_NOT_EXIST: u8 = 2;
const TASK_RESPONSE_FUNCTION_UNSUPPORTED: u8 = 5;
const TASK_RESPONSE_REJECTED: u8 = u8::MAX;

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

struct FanBackend {
    addrs: Vec<String>,
    capacity_bytes: u64,
    lane_cpus: Vec<usize>,
    window: usize,
    batch_spin: Duration,
}

enum Backend {
    File(FileBackend),
    Arena(ArenaBackend),
    Fan(FanBackend),
}

enum Blocks {
    Bytes(Vec<u8>),
    ArenaOne(ZcnblkAppArenaBuffer),
    Arena(Vec<ZcnblkAppArenaBuffer>),
}

impl Blocks {
    fn len(&self) -> usize {
        match self {
            Self::Bytes(bytes) => bytes.len(),
            Self::ArenaOne(_) => BLOCK_SIZE as usize,
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
            Self::ArenaOne(buffer) => {
                let slice = buffer.as_mut_slice()?;
                stream.read_exact(&mut slice[offset..end])
            }
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

    fn bytes(&self) -> io::Result<&[u8]> {
        match self {
            Self::Bytes(bytes) => Ok(bytes),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "direct fan transport received a non-userspace payload",
            )),
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

    fn open_fan(
        addrs: Vec<String>,
        capacity_bytes: u64,
        lane_cpus: Vec<usize>,
        window: usize,
    ) -> io::Result<Self> {
        validate_capacity(capacity_bytes)?;
        if addrs.is_empty() || addrs.iter().any(|addr| addr.trim().is_empty()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--fan-addrs must contain one non-empty TCP address per lane",
            ));
        }
        if addrs.len() != lane_cpus.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "direct fan topology has {} addresses but {} lane CPUs",
                    addrs.len(),
                    lane_cpus.len()
                ),
            ));
        }
        if window == 0 || window > COMMAND_WINDOW as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--fan-window must be in 1..={COMMAND_WINDOW}"),
            ));
        }
        let batch_spin_us = env::var("ZCISCSI_FAN_BATCH_SPIN_US")
            .ok()
            .map(|value| {
                value.parse::<u64>().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "ZCISCSI_FAN_BATCH_SPIN_US must be an integer",
                    )
                })
            })
            .transpose()?
            .unwrap_or(2);
        if batch_spin_us > 1_000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ZCISCSI_FAN_BATCH_SPIN_US must be in 0..=1000",
            ));
        }
        Ok(Self::Fan(FanBackend {
            addrs,
            capacity_bytes,
            lane_cpus,
            window,
            batch_spin: Duration::from_micros(batch_spin_us),
        }))
    }

    fn capacity_bytes(&self) -> u64 {
        match self {
            Self::File(backend) => backend.capacity_bytes,
            Self::Arena(backend) => backend.capacity_bytes,
            Self::Fan(backend) => backend.capacity_bytes,
        }
    }

    fn channels(&self) -> usize {
        match self {
            Self::File(_) => 1,
            Self::Arena(backend) => backend.arena.channels() as usize,
            Self::Fan(backend) => backend.addrs.len(),
        }
    }

    fn lane_cpus(&self) -> &[usize] {
        match self {
            Self::File(_) => &[],
            Self::Arena(backend) => &backend.lane_cpus,
            Self::Fan(backend) => &backend.lane_cpus,
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
            Self::Fan(_) => Ok(Blocks::Bytes(vec![0; len])),
            Self::Arena(backend) => {
                if len == BLOCK_SIZE as usize {
                    return allocate_arena(&backend.arena, &backend.device, lane as u32)
                        .map(Blocks::ArenaOne);
                }
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
            Self::Fan(_) => Err(io::Error::other(
                "direct fan reads are owned by the lane-local fan transport",
            )),
            Self::Arena(backend) => {
                if blocks == 1 {
                    let mut buffer = allocate_arena(&backend.arena, &backend.device, lane as u32)?;
                    buffer.read_at(&backend.device, lba * u64::from(BLOCK_SIZE))?;
                    return Ok(Blocks::ArenaOne(buffer));
                }
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
            (Self::Arena(backend), Blocks::ArenaOne(buffer)) => {
                buffer.write_at(&backend.device, lba * u64::from(BLOCK_SIZE))
            }
            (Self::Arena(backend), Blocks::Arena(buffers)) => {
                for (index, buffer) in buffers.iter_mut().enumerate() {
                    buffer.write_at(
                        &backend.device,
                        (lba + index as u64) * u64::from(BLOCK_SIZE),
                    )?;
                }
                Ok(())
            }
            (Self::Fan(_), _) => Err(io::Error::other(
                "direct fan writes are owned by the lane-local fan transport",
            )),
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
            // Every live direct-fan lane has already processed its explicit
            // SYNC before TargetSessions::barrier_all returns.
            Self::Fan(_) => Ok(()),
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
    Task {
        itt: u32,
        response: u8,
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
    occupancy: Mutex<Vec<u32>>,
    senders: Mutex<Vec<Vec<(u32, SyncSender<LaneTask>)>>>,
    next_id: AtomicU32,
    sessions_per_lane: u32,
}

impl TargetSessions {
    fn new(lanes: usize, sessions_per_lane: u32) -> io::Result<Arc<Self>> {
        if lanes == 0 || lanes > u32::BITS as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "iSCSI session sharding supports 1..=32 edge lanes",
            ));
        }
        if sessions_per_lane == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--sessions-per-lane must be positive",
            ));
        }
        Ok(Arc::new(Self {
            occupancy: Mutex::new(vec![0; lanes]),
            senders: Mutex::new(vec![Vec::new(); lanes]),
            next_id: AtomicU32::new(1),
            sessions_per_lane,
        }))
    }

    fn acquire(self: &Arc<Self>) -> io::Result<SessionLane> {
        let mut occupancy = self
            .occupancy
            .lock()
            .map_err(|_| io::Error::other("iSCSI session occupancy poisoned"))?;
        let lane = occupancy
            .iter()
            .enumerate()
            .filter(|(_, count)| **count < self.sessions_per_lane)
            .min_by_key(|(_, count)| **count)
            .map(|(lane, _)| lane)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "all iSCSI edge-lane session shards are occupied",
                )
            })?;
        occupancy[lane] += 1;
        Ok(SessionLane {
            sessions: Arc::clone(self),
            lane,
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
        })
    }

    fn register(&self, lease: &SessionLane, sender: SyncSender<LaneTask>) -> io::Result<()> {
        self.senders
            .lock()
            .map_err(|_| io::Error::other("iSCSI session registry poisoned"))?[lease.lane]
            .push((lease.id, sender));
        Ok(())
    }

    fn barrier_all(&self) -> io::Result<()> {
        let senders = self
            .senders
            .lock()
            .map_err(|_| io::Error::other("iSCSI session registry poisoned"))?
            .iter()
            .flat_map(|lane| lane.iter().map(|(_, sender)| sender.clone()))
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
    id: u32,
}

impl Drop for SessionLane {
    fn drop(&mut self) {
        if let Ok(mut senders) = self.sessions.senders.lock() {
            senders[self.lane].retain(|(id, _)| *id != self.id);
        }
        if let Ok(mut occupancy) = self.sessions.occupancy.lock() {
            occupancy[self.lane] = occupancy[self.lane].saturating_sub(1);
        }
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
    write_all_vectored_fixed(stream, [&header.0, data, &padding[..pad_len]])
}

fn write_blocks_pdu(stream: &mut TcpStream, header: &Header, blocks: &Blocks) -> io::Result<()> {
    match blocks {
        Blocks::Bytes(bytes) => write_bytes_pdu(stream, header, bytes),
        Blocks::ArenaOne(buffer) => {
            write_all_vectored_fixed(stream, [&header.0, buffer.as_slice()?])
        }
        // A 4 KiB SCSI command is the performance-critical case. Keep its
        // header and arena payload in a fixed stack iovec so transmitting an
        // already-selected userspace buffer does not allocate or copy it.
        Blocks::Arena(buffers) if buffers.len() == 1 => {
            write_all_vectored_fixed(stream, [&header.0, buffers[0].as_slice()?])
        }
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

fn write_all_vectored_fixed<const N: usize>(
    stream: &mut TcpStream,
    slices: [&[u8]; N],
) -> io::Result<()> {
    let mut index = 0;
    let mut offset = 0;
    while index < N {
        while index < N && slices[index].is_empty() {
            index += 1;
            offset = 0;
        }
        if index == N {
            break;
        }
        let iov: [IoSlice<'_>; N] = std::array::from_fn(|relative| {
            let absolute = index + relative;
            if absolute >= N {
                IoSlice::new(&[])
            } else {
                let start = if relative == 0 { offset } else { 0 };
                IoSlice::new(&slices[absolute][start..])
            }
        });
        let written = stream.write_vectored(&iov)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "iSCSI socket closed",
            ));
        }
        advance_vectored(&slices, &mut index, &mut offset, written);
    }
    Ok(())
}

fn advance_vectored(slices: &[&[u8]], index: &mut usize, offset: &mut usize, written: usize) {
    let mut remaining = written;
    while *index < slices.len() {
        let available = slices[*index].len() - *offset;
        if remaining < available {
            *offset += remaining;
            break;
        }
        remaining -= available;
        *index += 1;
        *offset = 0;
        if remaining == 0 {
            break;
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
        advance_vectored(&slices, &mut index, &mut offset, written);
    }
    Ok(())
}

fn write_data_batch(
    stream: &mut TcpStream,
    batch: &[(Command, Blocks)],
    exp_cmd_sn: u32,
    first_stat_sn: u32,
) -> io::Result<usize> {
    let mut headers = Vec::with_capacity(batch.len());
    for (index, (command, payload)) in batch.iter().enumerate() {
        debug_assert_eq!(payload.len(), BLOCK_SIZE as usize);
        let mut header = Header::zeroed(OP_DATA_IN);
        header.0[1] = 0x81;
        header.0[3] = 0;
        header.put_u64(8, command.lun);
        header.put_u32(16, command.itt);
        header.put_u32(20, u32::MAX);
        serial_fields(
            &mut header,
            first_stat_sn.wrapping_add(index as u32),
            exp_cmd_sn,
        );
        header.put_u32(36, 0);
        header.put_u32(40, 0);
        header.set_data_len(payload.len())?;
        if payload.len() < command.expected as usize {
            header.0[1] |= 0x02;
            header.put_u32(44, command.expected - payload.len() as u32);
        }
        headers.push(header);
    }

    // The hot 4 KiB path contributes exactly two iovecs per response. Linux's
    // IOV_MAX is at least 1024, so the bounded 64-response batch stays well
    // below the ABI limit. Arena buffers remain owned by `batch` until every
    // byte has been accepted by the socket; no payload is materialized.
    let mut iovecs = Vec::with_capacity(batch.len() * 2);
    for (index, (_, payload)) in batch.iter().enumerate() {
        iovecs.push(IoSlice::new(&headers[index].0));
        match payload {
            Blocks::ArenaOne(buffer) => iovecs.push(IoSlice::new(buffer.as_slice()?)),
            Blocks::Bytes(bytes) => iovecs.push(IoSlice::new(bytes)),
            Blocks::Arena(_) => unreachable!("4 KiB batch contains a multi-block payload"),
        }
    }
    let mut remaining = iovecs.as_mut_slice();
    let mut write_calls = 0usize;
    while !remaining.is_empty() {
        let written = stream.write_vectored(remaining)?;
        write_calls += 1;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "iSCSI socket closed",
            ));
        }
        IoSlice::advance_slices(&mut remaining, written);
    }
    Ok(write_calls)
}

fn writer_loop(
    mut stream: TcpStream,
    receiver: Receiver<Completion>,
    exp_cmd_sn: Arc<AtomicU32>,
    mut stat_sn: u32,
) -> io::Result<()> {
    let mut pending = None::<Completion>;
    let mut data_batch = Vec::<(Command, Blocks)>::with_capacity(DATA_IN_BATCH);
    let mut data_pdus = 0u64;
    let mut data_batches = 0u64;
    let mut data_write_calls = 0u64;
    let mut max_data_batch = 0usize;
    loop {
        let completion = match pending.take() {
            Some(completion) => completion,
            None => match receiver.recv() {
                Ok(completion) => completion,
                Err(_) => break,
            },
        };
        let exp = exp_cmd_sn.load(Ordering::Acquire);
        match completion {
            Completion::Data { command, payload } if payload.len() == BLOCK_SIZE as usize => {
                data_batch.clear();
                data_batch.push((command, payload));
                while data_batch.len() < DATA_IN_BATCH {
                    match receiver.try_recv() {
                        Ok(Completion::Data { command, payload })
                            if payload.len() == BLOCK_SIZE as usize =>
                        {
                            data_batch.push((command, payload));
                        }
                        Ok(completion) => {
                            pending = Some(completion);
                            break;
                        }
                        Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                    }
                }
                let write_calls = write_data_batch(&mut stream, &data_batch, exp, stat_sn)?;
                data_pdus += data_batch.len() as u64;
                data_batches += 1;
                data_write_calls += write_calls as u64;
                max_data_batch = max_data_batch.max(data_batch.len());
                stat_sn = stat_sn.wrapping_add(data_batch.len() as u32);
                continue;
            }
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
            Completion::Task { itt, response } => {
                let mut header = Header::zeroed(OP_TASK_RESPONSE);
                header.0[1] = 0x80;
                header.0[2] = response;
                header.put_u32(16, itt);
                serial_fields(&mut header, stat_sn, exp);
                write_bytes_pdu(&mut stream, &header, &[])?;
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
    eprintln!(
        "zciscsi-target-writer: data_pdus={data_pdus} data_batches={data_batches} data_write_calls={data_write_calls} max_data_batch={max_data_batch} avg_pdus_per_batch={:.2} avg_pdus_per_write_call={:.2}",
        data_pdus as f64 / data_batches.max(1) as f64,
        data_pdus as f64 / data_write_calls.max(1) as f64,
    );
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
                Backend::Fan(fan) => {
                    run_fan_lane(lane, fan, receiver, &lane_completions)
                }
                Backend::File(_) => {
                    Ok(run_sync_lane(lane, &lane_backend, receiver, &lane_completions))
                }
            };
            let counts = result.unwrap_or_else(|error| {
                eprintln!("zciscsi-target: lane={lane} io_uring_error={error}");
                LaneCounts::default()
            });
            eprintln!(
                "zciscsi-target-lane: lane={lane} cpu={} read_blocks={reads} write_blocks={writes} barriers={barriers} fua_writes={fua_writes} fua_drains={fua_drains} fan_batches={fan_batches} fan_tasks={fan_tasks} avg_fan_tasks_per_batch={avg_fan_tasks_per_batch:.2} max_fan_batch={max_fan_batch}",
                cpu.map_or_else(|| "unpinned-test-only".to_string(), |value| value.to_string()),
                reads = counts.reads,
                writes = counts.writes,
                barriers = counts.barriers,
                fua_writes = counts.fua_writes,
                fua_drains = counts.fua_drains,
                fan_batches = counts.fan_batches,
                fan_tasks = counts.fan_tasks,
                avg_fan_tasks_per_batch = counts.fan_tasks as f64
                    / counts.fan_batches.max(1) as f64,
                max_fan_batch = counts.max_fan_batch,
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
    barriers: u64,
    fua_writes: u64,
    fua_drains: u64,
    fan_batches: u64,
    fan_tasks: u64,
    max_fan_batch: usize,
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

fn fan_request_id(command: Command) -> u64 {
    (u64::from(command.cmd_sn) << 32) | u64::from(command.itt)
}

fn fan_topology(
    lane: usize,
    lane_count: usize,
    request_id: u64,
) -> io::Result<ZcnblkFrameTopology> {
    let lane_id = u32::try_from(lane)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "fan lane exceeds u32"))?;
    Ok(ZcnblkFrameTopology {
        lane_id,
        lane_count: u32::try_from(lane_count).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "fan lane count exceeds u32")
        })?,
        preferred_worker: lane_id,
        queue_id: lane_id,
        request_id,
        tier_id: 0,
        topology_flags: ZCNBLK_TOPOLOGY_VALID | ZCNBLK_TOPOLOGY_PORT_LANE,
    })
}

fn fan_read_header(stream: &mut TcpStream) -> io::Result<ZcnblkFrameHeader> {
    let mut bytes = [0u8; ZCNBLK_FRAME_HEADER_LEN];
    stream.read_exact(&mut bytes)?;
    ZcnblkFrameHeader::decode(&bytes)
}

fn fan_sync(stream: &mut TcpStream, lane: usize, lane_count: usize) -> io::Result<()> {
    let request = ZcnblkFrameHeader::with_topology(
        ZCNBLK_OP_SYNC,
        0,
        0,
        0,
        0,
        fan_topology(lane, lane_count, 0)?,
    )?
    .encode();
    stream.write_all(&request)?;
    let response = fan_read_header(stream)?;
    if response.op != ZCNBLK_OP_SYNC_ACK
        || response.shard != 0
        || response.len != 0
        || response.offset != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "direct fan SYNC response mismatch op={} shard={} len={} offset={}",
                response.op, response.shard, response.len, response.offset
            ),
        ));
    }
    Ok(())
}

fn fan_task_command(task: &LaneTask) -> Option<Command> {
    match task {
        LaneTask::Read { command, .. } | LaneTask::Write { command, .. } => Some(*command),
        LaneTask::Barrier(_) => None,
    }
}

fn fan_task_geometry(task: &LaneTask) -> io::Result<(u16, u64, usize)> {
    match task {
        LaneTask::Read { lba, blocks, .. } => Ok((
            ZCNBLK_OP_READ,
            lba.checked_mul(u64::from(BLOCK_SIZE))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "fan LBA overflow"))?,
            *blocks as usize * BLOCK_SIZE as usize,
        )),
        LaneTask::Write { lba, blocks, .. } => Ok((
            ZCNBLK_OP_WRITE,
            lba.checked_mul(u64::from(BLOCK_SIZE))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "fan LBA overflow"))?,
            blocks.len(),
        )),
        LaneTask::Barrier(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "barrier cannot be encoded as fan data I/O",
        )),
    }
}

fn fan_response_task_index(
    tasks: &[LaneTask],
    seen: &[bool],
    response: ZcnblkFrameHeader,
) -> io::Result<usize> {
    let index = tasks
        .iter()
        .position(|task| {
            fan_task_command(task)
                .is_some_and(|command| fan_request_id(command) == response.topology.request_id)
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "direct fan response has an unknown request id",
            )
        })?;
    if seen[index] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "direct fan response repeated a request id",
        ));
    }
    let (request_op, request_offset, request_len) = fan_task_geometry(&tasks[index])?;
    let expected_op = if request_op == ZCNBLK_OP_READ {
        ZCNBLK_OP_READ_RESP
    } else {
        ZCNBLK_OP_WRITE_ACK
    };
    if response.op != expected_op
        || response.shard != 0
        || response.len as usize != request_len
        || response.offset != request_offset
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "direct fan response member mismatch op={} shard={} len={} offset={}",
                response.op, response.shard, response.len, response.offset
            ),
        ));
    }
    Ok(index)
}

fn fan_read_response_headers(
    stream: &mut TcpStream,
    count: usize,
) -> io::Result<Vec<ZcnblkFrameHeader>> {
    if count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "direct fan response envelope is empty",
        ));
    }
    let byte_len = count
        .checked_mul(ZCNBLK_FRAME_HEADER_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fan header overflow"))?;
    let mut encoded = vec![0u8; byte_len];
    stream.read_exact(&mut encoded)?;
    encoded
        .chunks_exact(ZCNBLK_FRAME_HEADER_LEN)
        .map(|bytes| ZcnblkFrameHeader::decode(bytes.try_into().expect("64-byte response header")))
        .collect()
}

fn fan_receive_task_responses(
    stream: &mut TcpStream,
    tasks: &[LaneTask],
) -> io::Result<Vec<Option<Blocks>>> {
    let mut payloads = (0..tasks.len())
        .map(|_| None)
        .collect::<Vec<Option<Blocks>>>();
    let mut seen = vec![false; tasks.len()];
    let mut remaining = tasks.len();
    while remaining != 0 {
        let outer = fan_read_header(stream)?;
        if matches!(outer.op, ZCNBLK_OP_READ_RESP | ZCNBLK_OP_WRITE_ACK) {
            let index = fan_response_task_index(tasks, &seen, outer)?;
            if outer.op == ZCNBLK_OP_READ_RESP {
                let mut bytes = vec![0; outer.len as usize];
                stream.read_exact(&mut bytes)?;
                payloads[index] = Some(Blocks::Bytes(bytes));
            }
            seen[index] = true;
            remaining -= 1;
            continue;
        }

        if outer.op == ZCNBLK_OP_BATCH_RESP {
            if outer.shard != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "direct fan batch response has a non-zero shard",
                ));
            }
            let members = fan_read_response_headers(stream, outer.len as usize)?;
            let mut payload_bytes = 0usize;
            for member in members {
                let index = fan_response_task_index(tasks, &seen, member)?;
                if member.op == ZCNBLK_OP_READ_RESP {
                    let len = member.len as usize;
                    let mut bytes = vec![0; len];
                    stream.read_exact(&mut bytes)?;
                    payload_bytes = payload_bytes.checked_add(len).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "fan payload overflow")
                    })?;
                    payloads[index] = Some(Blocks::Bytes(bytes));
                }
                seen[index] = true;
                remaining = remaining.checked_sub(1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "too many fan responses")
                })?;
            }
            if outer.offset != payload_bytes as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "direct fan batch response misstated read payload bytes",
                ));
            }
            continue;
        }

        if outer.op == ZCNBLK_OP_READ_RANGE_RESP {
            if outer.shard != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "direct fan range response has a non-zero shard",
                ));
            }
            let ranges = fan_read_response_headers(stream, outer.len as usize)?;
            let mut payload_bytes = 0usize;
            for range in ranges {
                let range_len = range.len as usize;
                let range_end = range
                    .offset
                    .checked_add(range_len as u64)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "range overflow"))?;
                if range.op != ZCNBLK_OP_READ_RESP
                    || range.shard != 0
                    || range_len == 0
                    || range_len % BLOCK_SIZE as usize != 0
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "direct fan range response member is invalid",
                    ));
                }
                let start_index = tasks
                    .iter()
                    .position(|task| {
                        fan_task_command(task).is_some_and(|command| {
                            fan_request_id(command) == range.topology.request_id
                        })
                    })
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "direct fan range response has an unknown first request id",
                        )
                    })?;
                let mut indices = Vec::new();
                let mut cursor = range.offset;
                for (index, task) in tasks.iter().enumerate().skip(start_index) {
                    if seen[index] {
                        break;
                    }
                    let (op, offset, len) = fan_task_geometry(task)?;
                    if op != ZCNBLK_OP_READ || offset != cursor {
                        break;
                    }
                    let end = cursor.checked_add(len as u64).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "range cursor overflow")
                    })?;
                    if end > range_end {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "direct fan range response split a pending read",
                        ));
                    }
                    indices.push((offset, index, len));
                    cursor = end;
                    if cursor == range_end {
                        break;
                    }
                }
                if indices.is_empty() || cursor != range_end {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "direct fan range response omitted or added read bytes",
                    ));
                }
                let mut range_payload = vec![0; range_len];
                stream.read_exact(&mut range_payload)?;
                let mut payload_offset = 0usize;
                for (_, index, len) in indices {
                    payloads[index] = Some(Blocks::Bytes(
                        range_payload[payload_offset..payload_offset + len].to_vec(),
                    ));
                    payload_offset += len;
                    seen[index] = true;
                    remaining = remaining.checked_sub(1).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "too many range responses")
                    })?;
                }
                payload_bytes = payload_bytes.checked_add(range_len).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "range payload overflow")
                })?;
            }
            if outer.offset != payload_bytes as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "direct fan range response misstated read payload bytes",
                ));
            }
            continue;
        }

        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected direct fan response op={}", outer.op),
        ));
    }
    Ok(payloads)
}

fn fan_send_tasks(
    stream: &mut TcpStream,
    lane: usize,
    lane_count: usize,
    tasks: &[LaneTask],
) -> io::Result<()> {
    if tasks.is_empty() || tasks.len() > LANE_PIPELINE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "direct fan wire batch must contain 1..=64 tasks",
        ));
    }
    if tasks.len() == 1 {
        let task = &tasks[0];
        let command = fan_task_command(task).expect("single fan task is data I/O");
        let (op, offset, len) = fan_task_geometry(task)?;
        let header = ZcnblkFrameHeader::with_topology(
            op,
            0,
            0,
            len,
            offset,
            fan_topology(lane, lane_count, fan_request_id(command))?,
        )?
        .encode();
        match task {
            LaneTask::Write { blocks, .. } => {
                write_all_vectored_fixed(stream, [&header, blocks.bytes()?])?;
            }
            LaneTask::Read { .. } => stream.write_all(&header)?,
            LaneTask::Barrier(_) => unreachable!(),
        }
        return Ok(());
    }
    let mut headers = Vec::with_capacity(tasks.len() * ZCNBLK_FRAME_HEADER_LEN);
    let mut write_payload_bytes = 0usize;
    for task in tasks {
        let command = fan_task_command(task).expect("fan batch contains only data I/O");
        let (op, offset, len) = fan_task_geometry(task)?;
        headers.extend_from_slice(
            &ZcnblkFrameHeader::with_topology(
                op,
                0,
                0,
                len,
                offset,
                fan_topology(lane, lane_count, fan_request_id(command))?,
            )?
            .encode(),
        );
        if op == ZCNBLK_OP_WRITE {
            write_payload_bytes = write_payload_bytes.checked_add(len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "fan batch payload overflow")
            })?;
        }
    }
    let outer = ZcnblkFrameHeader::with_topology(
        ZCNBLK_OP_BATCH,
        0,
        0,
        tasks.len(),
        write_payload_bytes as u64,
        fan_topology(lane, lane_count, 0)?,
    )?
    .encode();
    let mut slices = Vec::with_capacity(tasks.len() + 2);
    slices.push(&outer[..]);
    slices.push(&headers[..]);
    for task in tasks {
        if let LaneTask::Write { blocks, .. } = task {
            slices.push(blocks.bytes()?);
        }
    }
    write_all_vectored(stream, &slices)
}

fn fan_complete_window(
    stream: &mut TcpStream,
    lane: usize,
    lane_count: usize,
    tasks: Vec<LaneTask>,
    completions: &Sender<Completion>,
    counts: &mut LaneCounts,
) -> io::Result<()> {
    for batch in tasks.chunks(LANE_PIPELINE) {
        fan_send_tasks(stream, lane, lane_count, batch)?;
    }
    let mut payloads = fan_receive_task_responses(stream, &tasks)?;
    let fua_writes = tasks
        .iter()
        .filter(|task| matches!(task, LaneTask::Write { fua: true, .. }))
        .count() as u64;
    if fua_writes != 0 {
        fan_sync(stream, lane, lane_count)?;
        counts.fua_writes += fua_writes;
        counts.fua_drains += 1;
    }
    for (index, task) in tasks.into_iter().enumerate() {
        let command = fan_task_command(&task).expect("fan completion is data I/O");
        match task {
            LaneTask::Read { blocks, .. } => {
                counts.reads += u64::from(blocks);
                completions
                    .send(Completion::Data {
                        command,
                        payload: payloads[index].take().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "direct fan read response missing payload",
                            )
                        })?,
                    })
                    .map_err(channel_closed)?;
            }
            LaneTask::Write { blocks, .. } => {
                counts.writes += (blocks.len() / BLOCK_SIZE as usize) as u64;
                completions
                    .send(Completion::Good(command))
                    .map_err(channel_closed)?;
            }
            LaneTask::Barrier(_) => unreachable!(),
        }
    }
    Ok(())
}

fn run_fan_lane(
    lane: usize,
    backend: &FanBackend,
    receiver: Receiver<LaneTask>,
    completions: &Sender<Completion>,
) -> io::Result<LaneCounts> {
    let addr = backend.addrs.get(lane).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "direct fan lane has no TCP address",
        )
    })?;
    let mut stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    let lane_count = backend.addrs.len();
    let mut counts = LaneCounts::default();
    let mut deferred = None::<LaneTask>;
    loop {
        // Queue readiness is the batching signal.  If work survived the prior
        // fan round trip, preserve deep-QD pipelining with a tiny gather.  If
        // this lane had to block for the first request, it is latency-sensitive
        // and a lone task must leave immediately.  This is lane-local channel
        // state: no shared counter, atomic, timer heuristic, or placement logic.
        let (task, spin_single) = if let Some(task) = deferred.take() {
            (task, false)
        } else {
            match receiver.try_recv() {
                Ok(task) => (task, true),
                Err(TryRecvError::Empty) => match receiver.recv() {
                    Ok(task) => (task, false),
                    Err(_) => break,
                },
                Err(TryRecvError::Disconnected) => break,
            }
        };
        if let LaneTask::Barrier(ack) = task {
            counts.barriers += 1;
            let result = fan_sync(&mut stream, lane, lane_count);
            let failed = result.is_err();
            let _ = ack.send(result);
            if failed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "direct fan SYNC failed",
                ));
            }
            continue;
        }
        let mut tasks = Vec::with_capacity(backend.window);
        tasks.push(task);
        let mut stop_at_fua = matches!(tasks[0], LaneTask::Write { fua: true, .. });
        let gather_started = Instant::now();
        while tasks.len() < backend.window && !stop_at_fua {
            match receiver.try_recv() {
                Ok(task @ LaneTask::Barrier(_)) => {
                    deferred = Some(task);
                    break;
                }
                Ok(task) => {
                    stop_at_fua = matches!(task, LaneTask::Write { fua: true, .. });
                    tasks.push(task);
                }
                Err(TryRecvError::Empty)
                    if (tasks.len() > 1 || spin_single)
                        && !backend.batch_spin.is_zero()
                        && gather_started.elapsed() < backend.batch_spin =>
                {
                    std::hint::spin_loop();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        counts.fan_batches += 1;
        counts.fan_tasks += tasks.len() as u64;
        counts.max_fan_batch = counts.max_fan_batch.max(tasks.len());
        fan_complete_window(
            &mut stream,
            lane,
            lane_count,
            tasks,
            completions,
            &mut counts,
        )?;
    }
    let _ = stream.shutdown(Shutdown::Both);
    Ok(counts)
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
                    counts.fua_writes += u64::from(fua);
                    counts.fua_drains += u64::from(fua);
                    let _ = completions.send(Completion::Good(command));
                }
                Err(_) => {
                    let _ = completions.send(Completion::Check(command, sense_medium_error()));
                }
            }
        }
        LaneTask::Barrier(ack) => {
            counts.barriers += 1;
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
        Backend::File(_) | Backend::Fan(_) => unreachable!(),
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
                counts.barriers += 1;
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
            let payload = if blocks == 1 {
                Blocks::ArenaOne(allocate_arena(&arena.arena, &arena.device, lane as u32)?)
            } else {
                let mut payload = Vec::with_capacity(blocks as usize);
                for _ in 0..blocks {
                    payload.push(allocate_arena(&arena.arena, &arena.device, lane as u32)?);
                }
                Blocks::Arena(payload)
            };
            (command, lba, payload, ArenaIoKind::Read)
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
    let mut remaining = 0usize;
    let mut failed = false;
    let mut queue_buffer = |index: usize, buffer: &mut ZcnblkAppArenaBuffer| {
        let offset = (lba + index as u64) * u64::from(BLOCK_SIZE);
        let result = match kind {
            ArenaIoKind::Read => ring.queue_read(&arena.device, buffer, offset, slot as u64),
            ArenaIoKind::Write { .. } => {
                ring.queue_write(&arena.device, buffer, offset, slot as u64)
            }
        };
        if result.is_err() {
            failed = true;
            return false;
        }
        remaining += 1;
        true
    };
    match &mut blocks {
        Blocks::ArenaOne(buffer) => {
            let _ = queue_buffer(0, buffer);
        }
        Blocks::Arena(buffers) => {
            for (index, buffer) in buffers.iter_mut().enumerate() {
                if !queue_buffer(index, buffer) {
                    break;
                }
            }
        }
        Blocks::Bytes(_) => unreachable!(),
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
        match &mut request.blocks {
            Blocks::ArenaOne(buffer) => {
                if buffer.wait_reacquire(Duration::from_secs(5)).is_err() {
                    request.failed = true;
                }
            }
            Blocks::Arena(buffers) => {
                for buffer in buffers {
                    if buffer.wait_reacquire(Duration::from_secs(5)).is_err() {
                        request.failed = true;
                    }
                }
            }
            Blocks::Bytes(_) => unreachable!(),
        }
    }
    if let ArenaIoKind::Write { fua: true } = request.kind {
        counts.fua_writes += 1;
        counts.fua_drains += 1;
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
    rx_cpus: Arc<[usize]>,
    tx_cpus: Arc<[usize]>,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let session = login(&mut stream, &target)?;
    let session_lane = if session.discovery {
        None
    } else {
        Some(sessions.acquire()?)
    };
    if let Some(lease) = &session_lane {
        if let Some(cpu) = rx_cpus.get(lease.lane) {
            pin_current_thread(*cpu)?;
        }
    }
    let lane_ids = session_lane
        .as_ref()
        .map_or_else(Vec::new, |lease| vec![lease.lane]);
    let exp_cmd_sn = Arc::new(AtomicU32::new(session.exp_cmd_sn));
    let (completion_tx, completion_rx) = mpsc::channel();
    let writer_stream = stream.try_clone()?;
    let writer_exp = Arc::clone(&exp_cmd_sn);
    let writer_cpu = session_lane
        .as_ref()
        .and_then(|lease| tx_cpus.get(lease.lane).copied());
    let writer = thread::spawn(move || {
        if let Some(cpu) = writer_cpu
            && let Err(error) = pin_current_thread(cpu)
        {
            eprintln!("zciscsi-target: tx_cpu={cpu} pin_error={error}");
            return;
        }
        if let Err(error) = writer_loop(writer_stream, completion_rx, writer_exp, session.stat_sn) {
            eprintln!("zciscsi-target: writer_error={error}");
        }
    });
    let (lane_senders, lane_handles) =
        spawn_lane_workers(Arc::clone(&backend), completion_tx.clone(), &lane_ids);
    if let Some(lease) = &session_lane {
        sessions.register(lease, lane_senders[0].clone())?;
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
            OP_TASK_REQUEST if !session.discovery => {
                let _ = read_small_data(&mut stream, header)?;
                update_exp_cmd(&exp_cmd_sn, header);
                let function = header.0[1] & 0x7f;
                let lun = header.u64(8);
                let (response, action) = if function != TASK_FUNCTION_LOGICAL_UNIT_RESET {
                    (TASK_RESPONSE_FUNCTION_UNSUPPORTED, "function-unsupported")
                } else if lun != 0 {
                    (TASK_RESPONSE_LUN_DOES_NOT_EXIST, "lun-does-not-exist")
                } else {
                    // A successful LUN RESET is a control-plane barrier.  It
                    // must never bypass writes already admitted to any live
                    // userspace lane.  The frontend does not make placement
                    // decisions: each lane's separate userspace RAID stage
                    // owns the actual mirror drain behind this barrier.
                    match sessions.barrier_all() {
                        Ok(()) => {
                            pending.clear();
                            (TASK_RESPONSE_COMPLETE, "all-userspace-lanes-drained")
                        }
                        Err(_) => (TASK_RESPONSE_REJECTED, "userspace-lane-drain-failed"),
                    }
                };
                eprintln!(
                    "zciscsi-target-task-management: function={function} lun={lun} response={response} action={action} placement_owner=downstream-userspace-raid"
                );
                completion_tx
                    .send(Completion::Task {
                        itt: header.u32(16),
                        response,
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

    // Unregister first so the control-plane registry releases its sender
    // clone. Otherwise a clean logout can leave the lane worker waiting for
    // channel disconnect while this thread waits to join that same worker.
    drop(session_lane);
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
            } else if backend.check_range(lba, blocks).is_err() {
                completions
                    .send(Completion::Check(command, sense_lba_out_of_range()))
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
            if backend.check_range(lba, blocks).is_err() {
                let _ = read_small_data(stream, header)?;
                completions
                    .send(Completion::Check(command, sense_lba_out_of_range()))
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

fn sense_lba_out_of_range() -> Vec<u8> {
    fixed_sense(0x05, 0x21, 0)
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
    fan_addrs: Option<Vec<String>>,
    capacity_bytes: Option<u64>,
    fan_window: usize,
    zcnblk_device: PathBuf,
    lane_cpus: Vec<usize>,
    rx_cpus: Vec<usize>,
    tx_cpus: Vec<usize>,
    sessions_per_lane: u32,
}

fn usage() -> &'static str {
    "usage: zciscsi-target --listen HOST:PORT [--advertise HOST:PORT] --target IQN \
     (--leaf-file PATH | --arena-socket PATH --zcnblk-device PATH --lane-cpus CPU,... \
     --rx-cpus CPU,... --tx-cpus CPU,... | --fan-addrs HOST:PORT,... --capacity-bytes N \
     [--fan-window N] --lane-cpus CPU,... --rx-cpus CPU,... --tx-cpus CPU,...)"
}

fn parse_cpu_list(value: Option<String>, option: &str) -> io::Result<Vec<usize>> {
    value
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("missing {option}")))?
        .split(',')
        .map(|cpu| {
            cpu.parse::<usize>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("invalid {option}"))
            })
        })
        .collect()
}

fn parse_options() -> io::Result<Options> {
    let mut args = env::args().skip(1);
    let mut listen = None;
    let mut advertise = None;
    let mut target = None;
    let mut leaf = None;
    let mut arena_socket = None;
    let mut fan_addrs = None;
    let mut capacity_bytes = None;
    let mut fan_window = LANE_PIPELINE;
    let mut zcnblk_device = PathBuf::from("/dev/zcnblk0");
    let mut lane_cpus = Vec::new();
    let mut rx_cpus = Vec::new();
    let mut tx_cpus = Vec::new();
    let mut sessions_per_lane = 1u32;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next(),
            "--advertise" => advertise = args.next(),
            "--target" => target = args.next(),
            "--leaf-file" => leaf = args.next().map(PathBuf::from),
            "--arena-socket" => arena_socket = args.next().map(PathBuf::from),
            "--fan-addrs" => {
                fan_addrs = Some(
                    args.next()
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "missing --fan-addrs")
                        })?
                        .split(',')
                        .map(str::to_string)
                        .collect(),
                )
            }
            "--capacity-bytes" => {
                capacity_bytes = Some(
                    args.next()
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "missing --capacity-bytes")
                        })?
                        .parse::<u64>()
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidInput, "invalid --capacity-bytes")
                        })?,
                )
            }
            "--fan-window" => {
                fan_window = args
                    .next()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "missing --fan-window")
                    })?
                    .parse::<usize>()
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "invalid --fan-window")
                    })?;
            }
            "--zcnblk-device" => {
                zcnblk_device = args.next().map(PathBuf::from).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing --zcnblk-device")
                })?;
            }
            "--lane-cpus" => {
                lane_cpus = parse_cpu_list(args.next(), "--lane-cpus")?;
            }
            "--rx-cpus" => rx_cpus = parse_cpu_list(args.next(), "--rx-cpus")?,
            "--tx-cpus" => tx_cpus = parse_cpu_list(args.next(), "--tx-cpus")?,
            "--sessions-per-lane" => {
                sessions_per_lane = args
                    .next()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "missing --sessions-per-lane")
                    })?
                    .parse()
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "invalid --sessions-per-lane")
                    })?;
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
    if usize::from(leaf.is_some())
        + usize::from(arena_socket.is_some())
        + usize::from(fan_addrs.is_some())
        != 1
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, usage()));
    }
    if (arena_socket.is_some() || fan_addrs.is_some())
        && (lane_cpus.is_empty()
            || rx_cpus.len() != lane_cpus.len()
            || tx_cpus.len() != lane_cpus.len())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "userspace-stage mode requires equally sized, complete --lane-cpus, --rx-cpus, and --tx-cpus mappings",
        ));
    }
    if arena_socket.is_some() || fan_addrs.is_some() {
        let mut all = lane_cpus.clone();
        all.extend_from_slice(&rx_cpus);
        all.extend_from_slice(&tx_cpus);
        all.sort_unstable();
        all.dedup();
        if all.len() != lane_cpus.len() * 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "iSCSI lane, RX, and TX CPU roles must not overlap",
            ));
        }
    }
    if leaf.is_some() && (!lane_cpus.is_empty() || !rx_cpus.is_empty() || !tx_cpus.is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal file test mode has no userspace placement lanes",
        ));
    }
    if fan_addrs.is_some() != capacity_bytes.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--fan-addrs and --capacity-bytes must be supplied together",
        ));
    }
    Ok(Options {
        advertise: advertise.unwrap_or_else(|| listen.clone()),
        listen,
        target: target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, usage()))?,
        leaf,
        arena_socket,
        fan_addrs,
        capacity_bytes,
        fan_window,
        zcnblk_device,
        lane_cpus,
        rx_cpus,
        tx_cpus,
        sessions_per_lane,
    })
}

fn run() -> io::Result<()> {
    let options = parse_options()?;
    let (backend, backing, copies, lane_io) = if let Some(path) = &options.leaf {
        (
            Backend::open_file(path)?,
            format!("terminal-file-test-only:{}", path.display()),
            "protocol-buffered-test-only",
            "blocking-terminal-file-test",
        )
    } else if let Some(socket) = options.arena_socket.as_ref() {
        (
            Backend::open_arena(socket, &options.zcnblk_device, options.lane_cpus.clone())?,
            format!(
                "userspace-stage:{} via {}",
                socket.display(),
                options.zcnblk_device.display()
            ),
            "socket-to-arena=0 arena-to-block-edge=0",
            "io_uring-shared-arena",
        )
    } else {
        let addrs = options.fan_addrs.clone().expect("fan mode selected");
        (
            Backend::open_fan(
                addrs.clone(),
                options.capacity_bytes.expect("fan capacity selected"),
                options.lane_cpus.clone(),
                options.fan_window,
            )?,
            format!("direct-userspace-raid-stage:{}", addrs.join(",")),
            "iscsi-socket-to-userspace=direct fan-tcp-send=kernel-copy fan-tcp-read-to-userspace=direct",
            "lane-batched-zcnblk-tcp",
        )
    };
    let backend = Arc::new(backend);
    let (lane_qd, lane_batch) = match backend.as_ref() {
        Backend::Fan(fan) => (fan.window, LANE_PIPELINE),
        _ => (LANE_PIPELINE, LANE_PIPELINE),
    };
    let fan_batch_spin_us = match backend.as_ref() {
        Backend::Fan(fan) => fan.batch_spin.as_micros(),
        Backend::File(_) | Backend::Arena(_) => 0,
    };
    eprintln!(
        "zciscsi-target: implementation=zcutils-rfc7143-from-scratch external_iscsi_library=no listen={} advertise={} target={} capacity_bytes={} logical_block=4096 physical_block=4096 lanes={} sessions_per_lane={} lane_to_cpu={:?} rx_to_cpu={:?} tx_to_cpu={:?} role_cpu_sharing=within-role-session-shards-only lane_io={} lane_qd={} lane_batch={} fan_batch_spin_us={} backing={} payload_copies={} placement_owner=downstream-userspace-raid frontend_placement=no mirror_primitive=no",
        options.listen,
        options.advertise,
        options.target,
        backend.capacity_bytes(),
        backend.channels(),
        options.sessions_per_lane,
        options.lane_cpus,
        options.rx_cpus,
        options.tx_cpus,
        lane_io,
        lane_qd,
        lane_batch,
        fan_batch_spin_us,
        backing,
        copies,
    );
    let listener = TcpListener::bind(&options.listen)?;
    let sessions = TargetSessions::new(backend.channels(), options.sessions_per_lane)?;
    let rx_cpus: Arc<[usize]> = Arc::from(options.rx_cpus);
    let tx_cpus: Arc<[usize]> = Arc::from(options.tx_cpus);
    let target: Arc<str> = Arc::from(options.target);
    let advertised: Arc<str> = Arc::from(options.advertise);
    for incoming in listener.incoming() {
        let stream = incoming?;
        let connection_backend = Arc::clone(&backend);
        let connection_target = Arc::clone(&target);
        let connection_advertised = Arc::clone(&advertised);
        let connection_sessions = Arc::clone(&sessions);
        let connection_rx_cpus = Arc::clone(&rx_cpus);
        let connection_tx_cpus = Arc::clone(&tx_cpus);
        thread::spawn(move || {
            if let Err(error) = handle_connection(
                stream,
                connection_backend,
                connection_target,
                connection_advertised,
                connection_sessions,
                connection_rx_cpus,
                connection_tx_cpus,
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
    fn fan_backend_rejects_the_first_block_past_capacity() {
        let backend = Backend::Fan(FanBackend {
            addrs: vec!["127.0.0.1:1".to_string()],
            capacity_bytes: 2 * u64::from(BLOCK_SIZE),
            lane_cpus: vec![0],
            window: 1,
            batch_spin: Duration::ZERO,
        });
        assert!(backend.check_range(1, 1).is_ok());
        assert!(backend.check_range(2, 1).is_err());
        let sense = sense_lba_out_of_range();
        assert_eq!(sense[2], 0x05);
        assert_eq!(sense[12], 0x21);
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

    #[test]
    fn task_management_response_preserves_itt_and_serial_window() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut reader = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (writer, _) = listener.accept().unwrap();
        let (sender, receiver) = mpsc::channel();
        let writer_thread = thread::spawn(move || {
            writer_loop(writer, receiver, Arc::new(AtomicU32::new(17)), 23).unwrap()
        });
        sender
            .send(Completion::Task {
                itt: 0x1234_5678,
                response: TASK_RESPONSE_FUNCTION_UNSUPPORTED,
            })
            .unwrap();
        drop(sender);

        let mut encoded = [0u8; BHS_BYTES];
        reader.read_exact(&mut encoded).unwrap();
        let response = Header(encoded);
        assert_eq!(response.opcode(), OP_TASK_RESPONSE);
        assert_eq!(response.0[1], 0x80);
        assert_eq!(response.0[2], TASK_RESPONSE_FUNCTION_UNSUPPORTED);
        assert_eq!(response.u32(16), 0x1234_5678);
        assert_eq!(response.u32(24), 23);
        assert_eq!(response.u32(28), 17);
        assert_eq!(response.u32(32), 17 + COMMAND_WINDOW - 1);
        writer_thread.join().unwrap();
    }

    #[test]
    fn data_in_batch_preserves_pdu_boundaries_and_serials() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut reader = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut writer, _) = listener.accept().unwrap();
        let batch = vec![
            (
                Command {
                    itt: 11,
                    lun: 0,
                    expected: BLOCK_SIZE,
                    cmd_sn: 7,
                },
                Blocks::Bytes(vec![0xa5; BLOCK_SIZE as usize]),
            ),
            (
                Command {
                    itt: 12,
                    lun: 0,
                    expected: BLOCK_SIZE,
                    cmd_sn: 8,
                },
                Blocks::Bytes(vec![0x5a; BLOCK_SIZE as usize]),
            ),
        ];
        assert_eq!(write_data_batch(&mut writer, &batch, 9, 23).unwrap(), 1);

        let pdu_bytes = BHS_BYTES + BLOCK_SIZE as usize;
        let mut wire = vec![0u8; pdu_bytes * batch.len()];
        reader.read_exact(&mut wire).unwrap();
        for (index, expected_byte) in [0xa5, 0x5a].into_iter().enumerate() {
            let offset = index * pdu_bytes;
            let header = Header(wire[offset..offset + BHS_BYTES].try_into().unwrap());
            assert_eq!(header.opcode(), OP_DATA_IN);
            assert_eq!(header.data_len(), BLOCK_SIZE as usize);
            assert_eq!(header.u32(16), 11 + index as u32);
            assert_eq!(header.u32(24), 23 + index as u32);
            assert_eq!(header.u32(28), 9);
            assert!(
                wire[offset + BHS_BYTES..offset + pdu_bytes]
                    .iter()
                    .all(|byte| *byte == expected_byte)
            );
        }
    }
}
