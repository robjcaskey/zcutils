use super::{
    BlockDeviceTopology, FixedSendBuffers, LatencyHistogram, RawRing, RawRingCqeMode,
    RawRingOptions, RawRingSqMode, RawRingStats, SlotWalBufferMode, SlotWalMode, SlotWalTarget,
    SlotWalTargetKind, block_device_size, cpu_numa_node, current_cpu, current_tid, env_truthy,
    explicit_affinity_map_configured, format_cpu_list, linux_dev_major_minor, memlock_rlimit_bytes,
    parse_size_arg, pin_current_thread_if_requested_to_cpu, read_thread_context_switches,
    standard_slot_wal_buffer_mode, thread_cpu_time, validate_slot_wal_common,
    validate_slot_wal_write_target_safety, warn_hugetlb_pressure, zc_topology_strict_enabled,
};
use serde::Serialize;
use std::collections::HashSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const REQUEST_ALIGNMENT: u64 = 4 * 1024;
const MIN_CAPACITY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TRANSFER_BYTES: usize = 64 * 1024;
const SIZE_CLASSES: [usize; 5] = [4 * 1024, 8 * 1024, 16 * 1024, 32 * 1024, 64 * 1024];
const SIZE_WEIGHTS: [u32; 5] = [400, 240, 200, 80, 80];
const STREAM_COUNT: usize = 8;
const PROFILE_WEIGHT: u32 = 1_000;
const WRITE_BUFFER_MAGIC: &[u8; 8] = b"ZCWLOAD1";
const ZCNBLK_MODULE_SYSFS: &str = "/sys/module/zcnblk_client_mod";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum IoKind {
    Read,
    Write,
}

impl IoKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum AreaId {
    Data,
    User,
    Journal,
}

impl AreaId {
    fn as_str(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::User => "user",
            Self::Journal => "journal",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransferModel {
    Fixed8K,
    Mixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddressModel {
    Uniform,
    Reuse { start_ppm: u32, end_ppm: u32 },
    Sequential,
}

#[derive(Clone, Copy, Debug)]
struct StreamSpec {
    name: &'static str,
    area: AreaId,
    weight: u32,
    read_percent: u32,
    transfer: TransferModel,
    address: AddressModel,
}

const STREAMS: [StreamSpec; STREAM_COUNT] = [
    StreamSpec {
        name: "data-uniform",
        area: AreaId::Data,
        weight: 35,
        read_percent: 50,
        transfer: TransferModel::Fixed8K,
        address: AddressModel::Uniform,
    },
    StreamSpec {
        name: "data-reuse-a",
        area: AreaId::Data,
        weight: 281,
        read_percent: 50,
        transfer: TransferModel::Fixed8K,
        address: AddressModel::Reuse {
            start_ppm: 150_000,
            end_ppm: 200_000,
        },
    },
    StreamSpec {
        name: "data-scan",
        area: AreaId::Data,
        weight: 70,
        read_percent: 100,
        transfer: TransferModel::Mixed,
        address: AddressModel::Sequential,
    },
    StreamSpec {
        name: "data-reuse-b",
        area: AreaId::Data,
        weight: 210,
        read_percent: 50,
        transfer: TransferModel::Fixed8K,
        address: AddressModel::Reuse {
            start_ppm: 700_000,
            end_ppm: 750_000,
        },
    },
    StreamSpec {
        name: "user-uniform",
        area: AreaId::User,
        weight: 18,
        read_percent: 30,
        transfer: TransferModel::Fixed8K,
        address: AddressModel::Uniform,
    },
    StreamSpec {
        name: "user-reuse",
        area: AreaId::User,
        weight: 70,
        read_percent: 30,
        transfer: TransferModel::Fixed8K,
        address: AddressModel::Reuse {
            start_ppm: 470_000,
            end_ppm: 520_000,
        },
    },
    StreamSpec {
        name: "user-scan",
        area: AreaId::User,
        weight: 35,
        read_percent: 100,
        transfer: TransferModel::Mixed,
        address: AddressModel::Sequential,
    },
    StreamSpec {
        name: "journal-append",
        area: AreaId::Journal,
        weight: 281,
        read_percent: 0,
        transfer: TransferModel::Mixed,
        address: AddressModel::Sequential,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    len: u64,
}

impl ByteRange {
    fn end(self) -> u64 {
        self.start + self.len
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WorkloadLayout {
    capacity: u64,
    data: ByteRange,
    user: ByteRange,
    journal: ByteRange,
}

impl WorkloadLayout {
    fn new(capacity: u64) -> io::Result<Self> {
        if capacity < MIN_CAPACITY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("capacity must be at least {MIN_CAPACITY_BYTES} bytes"),
            ));
        }
        if capacity % MAX_TRANSFER_BYTES as u64 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("capacity must be aligned to {MAX_TRANSFER_BYTES} bytes"),
            ));
        }

        let data_len = align_down(proportional(capacity, 45, 100), REQUEST_ALIGNMENT);
        let user_len = align_down(proportional(capacity, 45, 100), REQUEST_ALIGNMENT);
        let journal_len = capacity - data_len - user_len;
        let data = ByteRange {
            start: 0,
            len: data_len,
        };
        let user = ByteRange {
            start: data.end(),
            len: user_len,
        };
        let journal = ByteRange {
            start: user.end(),
            len: journal_len,
        };
        Ok(Self {
            capacity,
            data,
            user,
            journal,
        })
    }

    fn area(self, id: AreaId) -> ByteRange {
        match id {
            AreaId::Data => self.data,
            AreaId::User => self.user,
            AreaId::Journal => self.journal,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct WorkloadRequest {
    sequence: u64,
    stream: usize,
    area: AreaId,
    kind: IoKind,
    offset: u64,
    len: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct StreamState {
    cursor: u64,
    reuse_position: u64,
    initialized: bool,
}

#[derive(Clone)]
struct WorkloadGenerator {
    layout: WorkloadLayout,
    rng: u64,
    sequence: u64,
    states: [StreamState; STREAM_COUNT],
}

impl WorkloadGenerator {
    fn new(layout: WorkloadLayout, seed: u64, worker: usize) -> Self {
        let rng =
            seed ^ (worker as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xd1b5_4a32_d192_ed03;
        Self {
            layout,
            rng,
            sequence: 0,
            states: [StreamState::default(); STREAM_COUNT],
        }
    }

    fn next(&mut self) -> WorkloadRequest {
        let stream = weighted_index(&mut self.rng, &STREAMS.map(|spec| spec.weight));
        let spec = STREAMS[stream];
        let len = match spec.transfer {
            TransferModel::Fixed8K => 8 * 1024,
            TransferModel::Mixed => {
                let index = weighted_index(&mut self.rng, &SIZE_WEIGHTS);
                SIZE_CLASSES[index]
            }
        };
        let kind = if next_random(&mut self.rng) % 100 < spec.read_percent as u64 {
            IoKind::Read
        } else {
            IoKind::Write
        };
        let area = self.layout.area(spec.area);
        let offset = match spec.address {
            AddressModel::Uniform => uniform_offset(&mut self.rng, area, len),
            AddressModel::Reuse { start_ppm, end_ppm } => reuse_offset(
                &mut self.rng,
                &mut self.states[stream],
                subrange(area, start_ppm, end_ppm),
                len,
            ),
            AddressModel::Sequential => {
                sequential_offset(&mut self.rng, &mut self.states[stream], area, len)
            }
        };
        let request = WorkloadRequest {
            sequence: self.sequence,
            stream,
            area: spec.area,
            kind,
            offset,
            len,
        };
        self.sequence = self.sequence.wrapping_add(1);
        request
    }
}

fn align_down(value: u64, alignment: u64) -> u64 {
    value / alignment * alignment
}

fn proportional(value: u64, numerator: u32, denominator: u32) -> u64 {
    debug_assert_ne!(denominator, 0);
    ((u128::from(value) * u128::from(numerator)) / u128::from(denominator)) as u64
}

fn next_random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn weighted_index<const N: usize>(rng: &mut u64, weights: &[u32; N]) -> usize {
    let total: u32 = weights.iter().sum();
    let ticket = (next_random(rng) % total as u64) as u32;
    let mut cumulative = 0u32;
    for (index, weight) in weights.iter().enumerate() {
        cumulative += *weight;
        if ticket < cumulative {
            return index;
        }
    }
    N - 1
}

fn max_aligned_offset(range: ByteRange, len: usize) -> u64 {
    align_down(range.end() - len as u64, REQUEST_ALIGNMENT)
}

fn uniform_offset(rng: &mut u64, range: ByteRange, len: usize) -> u64 {
    let max = max_aligned_offset(range, len);
    let slots = (max - range.start) / REQUEST_ALIGNMENT + 1;
    range.start + (next_random(rng) % slots) * REQUEST_ALIGNMENT
}

fn subrange(area: ByteRange, start_ppm: u32, end_ppm: u32) -> ByteRange {
    let start = align_down(
        area.start + proportional(area.len, start_ppm, 1_000_000),
        REQUEST_ALIGNMENT,
    );
    let mut end = align_down(
        area.start + proportional(area.len, end_ppm, 1_000_000),
        REQUEST_ALIGNMENT,
    );
    end = end.max(start + MAX_TRANSFER_BYTES as u64).min(area.end());
    ByteRange {
        start,
        len: end - start,
    }
}

fn reuse_offset(rng: &mut u64, state: &mut StreamState, range: ByteRange, len: usize) -> u64 {
    let max = max_aligned_offset(range, len);
    let slots = (max - range.start) / REQUEST_ALIGNMENT + 1;
    if !state.initialized {
        state.reuse_position = next_random(rng) % slots;
        state.initialized = true;
    }
    if slots == 1 {
        return range.start;
    }
    let random = next_random(rng);
    let level = random.trailing_zeros().min(20);
    let radius = (1u64 << level).min(slots.saturating_sub(1).max(1));
    let distance = next_random(rng) % (radius + 1);
    if next_random(rng) & 1 == 0 {
        state.reuse_position = (state.reuse_position + distance) % slots;
    } else {
        state.reuse_position = modular_sub(state.reuse_position, distance, slots);
    }
    range.start + state.reuse_position * REQUEST_ALIGNMENT
}

fn modular_sub(position: u64, distance: u64, modulus: u64) -> u64 {
    debug_assert!(position < modulus);
    debug_assert!(distance < modulus);
    if position >= distance {
        position - distance
    } else {
        modulus - (distance - position)
    }
}

fn sequential_offset(rng: &mut u64, state: &mut StreamState, range: ByteRange, len: usize) -> u64 {
    let max = max_aligned_offset(range, len);
    if !state.initialized {
        let slots = (max - range.start) / REQUEST_ALIGNMENT + 1;
        state.cursor = range.start + next_random(rng) % slots * REQUEST_ALIGNMENT;
        state.initialized = true;
    }
    if state.cursor > max {
        state.cursor = range.start;
    }
    let offset = state.cursor;
    state.cursor = offset + len as u64;
    offset
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Engine {
    Uring,
    Sync,
}

impl Engine {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "uring" | "io-uring" | "io_uring" | "fixed" => Ok(Self::Uring),
            "sync" | "blocking" | "pread" => Ok(Self::Sync),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown engine {other:?}; use uring or sync"),
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Uring => "uring-fixed",
            Self::Sync => "sync",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BufferChoice {
    Auto,
    SmallPages,
    HugeTlb,
}

impl BufferChoice {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "small" | "small-pages" | "4k" => Ok(Self::SmallPages),
            "hugetlb" | "huge" | "2m" => Ok(Self::HugeTlb),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown buffer mode {other:?}; use auto, small-pages, or hugetlb"),
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionContract {
    Unspecified,
    LocalAck,
    RemoteAck,
    Durable,
}

impl CompletionContract {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "unspecified" | "unknown" => Ok(Self::Unspecified),
            "local" | "local-ack" => Ok(Self::LocalAck),
            "remote" | "remote-ack" => Ok(Self::RemoteAck),
            "durable" | "sync" => Ok(Self::Durable),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown completion contract {other:?}; use local-ack, remote-ack, or durable"
                ),
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::LocalAck => "local-ack",
            Self::RemoteAck => "remote-ack",
            Self::Durable => "durable",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LaneAssignment {
    lane: usize,
    worker: usize,
    cpu: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KthreadAssignment {
    kthread: usize,
    cpu: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TopologyMaps {
    lanes: Vec<LaneAssignment>,
    kthreads: Vec<KthreadAssignment>,
}

impl TopologyMaps {
    fn lane_label(&self) -> String {
        if self.lanes.is_empty() {
            return "unreported".to_string();
        }
        self.lanes
            .iter()
            .map(|entry| format!("lane{}:worker{}:cpu{}", entry.lane, entry.worker, entry.cpu))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn kthread_label(&self) -> String {
        if self.kthreads.is_empty() {
            return "unreported".to_string();
        }
        self.kthreads
            .iter()
            .map(|entry| format!("kthread{}:cpu{}", entry.kthread, entry.cpu))
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn parse_map_index(value: &str, prefix: &str, field: &str) -> io::Result<usize> {
    let digits = value.strip_prefix(prefix).unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {field} {value:?}; expected {prefix}N or N"),
        ));
    }
    digits.parse::<usize>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {field} {value:?}: {error}"),
        )
    })
}

fn parse_lane_map(value: &str) -> io::Result<Vec<LaneAssignment>> {
    if value.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "lane map must not be empty",
        ));
    }
    let mut assignments = Vec::new();
    let mut lanes = HashSet::new();
    for raw in value.split(',') {
        let fields: Vec<_> = raw.trim().split(':').collect();
        if fields.len() != 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid lane map entry {raw:?}; expected laneN:workerN:cpuN"),
            ));
        }
        let assignment = LaneAssignment {
            lane: parse_map_index(fields[0], "lane", "lane")?,
            worker: parse_map_index(fields[1], "worker", "worker")?,
            cpu: parse_map_index(fields[2], "cpu", "CPU")?,
        };
        if !lanes.insert(assignment.lane) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("duplicate lane {} in lane map", assignment.lane),
            ));
        }
        assignments.push(assignment);
    }
    assignments.sort_by_key(|entry| entry.lane);
    Ok(assignments)
}

fn parse_kthread_map(value: &str) -> io::Result<Vec<KthreadAssignment>> {
    if value.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "kthread map must not be empty",
        ));
    }
    let mut assignments = Vec::new();
    let mut kthreads = HashSet::new();
    for raw in value.split(',') {
        let fields: Vec<_> = raw.trim().split(':').collect();
        if fields.len() != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid kthread map entry {raw:?}; expected kthreadN:cpuN"),
            ));
        }
        let assignment = KthreadAssignment {
            kthread: parse_map_index(fields[0], "kthread", "kthread")?,
            cpu: parse_map_index(fields[1], "cpu", "CPU")?,
        };
        if !kthreads.insert(assignment.kthread) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("duplicate kthread {} in kthread map", assignment.kthread),
            ));
        }
        assignments.push(assignment);
    }
    assignments.sort_by_key(|entry| entry.kthread);
    Ok(assignments)
}

fn parse_topology_maps(cfg: &RunConfig) -> io::Result<TopologyMaps> {
    Ok(TopologyMaps {
        lanes: cfg
            .lane_map
            .as_deref()
            .map(parse_lane_map)
            .transpose()?
            .unwrap_or_default(),
        kthreads: cfg
            .kthread_map
            .as_deref()
            .map(parse_kthread_map)
            .transpose()?
            .unwrap_or_default(),
    })
}

#[derive(Clone)]
struct RunConfig {
    target: String,
    capacity: usize,
    engine: Engine,
    workers: usize,
    depth: usize,
    ring_entries: u32,
    duration: Duration,
    ops_per_worker: Option<u64>,
    target_iops: u64,
    seed: u64,
    pin_workers: bool,
    buffer_choice: BufferChoice,
    latency_sample_rate: u64,
    completion_batch: usize,
    pace_spin: Duration,
    lane_map: Option<String>,
    kthread_map: Option<String>,
    completion: CompletionContract,
    transport_rtt_ns: Option<u64>,
    repeats: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            target: String::new(),
            capacity: 1024 * 1024 * 1024,
            engine: Engine::Uring,
            workers: 8,
            depth: 64,
            ring_entries: 256,
            duration: Duration::from_secs(10),
            ops_per_worker: None,
            target_iops: 0,
            seed: 0x6a09_e667_f3bc_c909,
            pin_workers: true,
            buffer_choice: BufferChoice::Auto,
            latency_sample_rate: 1,
            completion_batch: 64,
            pace_spin: Duration::from_micros(50),
            lane_map: env::var("ZC_LANE_MAP")
                .ok()
                .filter(|value| !value.is_empty()),
            kthread_map: env::var("ZCWORKLOAD_KTHREAD_CPU_MAP")
                .ok()
                .filter(|value| !value.is_empty()),
            completion: CompletionContract::Unspecified,
            transport_rtt_ns: None,
            repeats: 1,
        }
    }
}

impl RunConfig {
    fn effective_depth_per_worker(&self) -> usize {
        match self.engine {
            Engine::Uring => self.depth,
            Engine::Sync => 1,
        }
    }

    fn aggregate_depth(&self) -> usize {
        self.workers
            .saturating_mul(self.effective_depth_per_worker())
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct CounterSet {
    ops: u64,
    reads: u64,
    writes: u64,
    bytes: u64,
    stream_ops: [u64; STREAM_COUNT],
    size_ops: [u64; SIZE_CLASSES.len()],
}

impl CounterSet {
    fn record(&mut self, request: WorkloadRequest) {
        self.ops += 1;
        self.bytes += request.len as u64;
        match request.kind {
            IoKind::Read => self.reads += 1,
            IoKind::Write => self.writes += 1,
        }
        self.stream_ops[request.stream] += 1;
        if let Some(index) = SIZE_CLASSES.iter().position(|size| *size == request.len) {
            self.size_ops[index] += 1;
        }
    }

    fn merge(&mut self, other: Self) {
        self.ops += other.ops;
        self.reads += other.reads;
        self.writes += other.writes;
        self.bytes += other.bytes;
        for (dst, src) in self.stream_ops.iter_mut().zip(other.stream_ops) {
            *dst += src;
        }
        for (dst, src) in self.size_ops.iter_mut().zip(other.size_ops) {
            *dst += src;
        }
    }
}

struct WorkerResult {
    worker: usize,
    elapsed: Duration,
    cpu: Duration,
    counters: CounterSet,
    voluntary_switches: u64,
    involuntary_switches: u64,
    migrations: u64,
    target_cpu: i32,
    affinity_applied: bool,
    end_cpu: i32,
    numa_node: Option<i32>,
    latency: LatencyHistogram,
    read_latency: LatencyHistogram,
    write_latency: LatencyHistogram,
    schedule_lag: LatencyHistogram,
    ring_stats: RawRingStats,
}

#[derive(Clone, Copy, Debug)]
struct RunSummary {
    iops: f64,
    mib_per_sec: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartState {
    Waiting,
    Started(Instant),
    Cancelled,
}

struct StartGate {
    state: Mutex<StartState>,
    changed: Condvar,
}

impl StartGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(StartState::Waiting),
            changed: Condvar::new(),
        }
    }

    fn wait(&self) -> io::Result<Instant> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("zcworkload start gate was poisoned"))?;
        loop {
            match *state {
                StartState::Waiting => {
                    state = self
                        .changed
                        .wait(state)
                        .map_err(|_| io::Error::other("zcworkload start gate was poisoned"))?;
                }
                StartState::Started(epoch) => return Ok(epoch),
                StartState::Cancelled => {
                    return Err(io::Error::other("zcworkload start cancelled"));
                }
            }
        }
    }

    fn start(&self, epoch: Instant) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("zcworkload start gate was poisoned"))?;
        if *state != StartState::Waiting {
            return Err(io::Error::other("zcworkload start gate is not waiting"));
        }
        *state = StartState::Started(epoch);
        self.changed.notify_all();
        Ok(())
    }

    fn cancel(&self) {
        if let Ok(mut state) = self.state.lock()
            && *state == StartState::Waiting
        {
            *state = StartState::Cancelled;
            self.changed.notify_all();
        }
    }
}

#[derive(Debug)]
enum WorkerInit {
    Ready { worker: usize },
    Failed { worker: usize, error: String },
}

struct RatePacer {
    epoch: Instant,
    rate: u64,
    issued: u64,
}

impl RatePacer {
    fn new(epoch: Instant, rate: u64) -> Self {
        Self {
            epoch,
            rate,
            issued: 0,
        }
    }

    fn due(&self) -> Instant {
        if self.rate == 0 {
            return self.epoch;
        }
        let ns = (u128::from(self.issued) * 1_000_000_000u128 / u128::from(self.rate))
            .min(u128::from(u64::MAX)) as u64;
        self.epoch + Duration::from_nanos(ns)
    }

    fn advance(&mut self) {
        self.issued += 1;
    }
}

fn wait_for_due_before_completion(
    stop_submitting: bool,
    target_iops: u64,
    free_slots: usize,
) -> bool {
    !stop_submitting && target_iops != 0 && free_slots != 0
}

fn worker_rate(total: u64, workers: usize, worker: usize) -> u64 {
    if total == 0 {
        return 0;
    }
    let base = total / workers as u64;
    base + u64::from((worker as u64) < total % workers as u64)
}

fn wait_until(deadline: Instant, spin_window: Duration) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remaining = deadline - now;
        if remaining > spin_window + Duration::from_micros(20) {
            thread::sleep(remaining - spin_window);
        } else {
            std::hint::spin_loop();
        }
    }
}

fn should_sample(sequence: u64, sample_rate: u64) -> bool {
    sample_rate != 0 && sequence % sample_rate == 0
}

fn fill_write_buffers(buffers: &FixedSendBuffers, count: usize, seed: u64) {
    for index in 0..count {
        let buffer =
            unsafe { std::slice::from_raw_parts_mut(buffers.ptr(index), MAX_TRANSFER_BYTES) };
        match index % 10 {
            0..=3 => {
                let text = b"account ledger transaction history customer payment balance ";
                for (offset, byte) in buffer.iter_mut().enumerate() {
                    *byte = text[offset % text.len()];
                }
            }
            4..=5 => {
                let mut state = seed ^ index as u64;
                for chunk in buffer.chunks_mut(8) {
                    let bytes = next_random(&mut state).to_le_bytes();
                    let len = chunk.len();
                    chunk.copy_from_slice(&bytes[..len]);
                }
            }
            _ => buffer.fill(0),
        }
    }
}

fn stamp_write_buffer(buffer: *mut u8, request: WorkloadRequest, worker: usize) {
    unsafe {
        ptr::copy_nonoverlapping(
            WRITE_BUFFER_MAGIC.as_ptr(),
            buffer,
            WRITE_BUFFER_MAGIC.len(),
        );
        ptr::copy_nonoverlapping(request.sequence.to_le_bytes().as_ptr(), buffer.add(8), 8);
        ptr::copy_nonoverlapping(request.offset.to_le_bytes().as_ptr(), buffer.add(16), 8);
        ptr::copy_nonoverlapping((worker as u64).to_le_bytes().as_ptr(), buffer.add(24), 8);
        ptr::copy_nonoverlapping(
            (request.stream as u64).to_le_bytes().as_ptr(),
            buffer.add(32),
            8,
        );
    }
}

fn validate_target_spec(target: &str) -> io::Result<()> {
    if let Some(name) = Path::new(target).file_name().and_then(|name| name.to_str())
        && Path::new(target).parent() == Some(Path::new("/dev"))
        && let Some(suffix) = name.strip_prefix("zcnblk")
        && !suffix.is_empty()
        && suffix.bytes().all(|byte| byte.is_ascii_digit())
        && target != "/dev/zcnblk0"
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zcworkload accepts exactly /dev/zcnblk0 as the network client block edge",
        ));
    }
    Ok(())
}

fn target_open_path(target: &SlotWalTarget) -> io::Result<PathBuf> {
    match &target.kind {
        SlotWalTargetKind::PartUuid(_) => {
            let link = fs::symlink_metadata(target.open_path())?;
            if !link.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} must be the expected by-partuuid symlink",
                        target.open_path().display()
                    ),
                ));
            }
            fs::canonicalize(target.open_path())
        }
        _ => {
            let metadata = fs::symlink_metadata(target.open_path())?;
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "refusing unexpected symlink for synthetic target {}",
                        target.open_path().display()
                    ),
                ));
            }
            Ok(target.open_path().to_path_buf())
        }
    }
}

fn read_uevent_value<'a>(contents: &'a str, key: &str) -> Option<&'a str> {
    contents.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|value| value.strip_prefix('='))
    })
}

fn verify_target_identity(target: &SlotWalTarget, metadata: &fs::Metadata) -> io::Result<PathBuf> {
    if !metadata.file_type().is_block_device() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "opened zcworkload target is not a block device",
        ));
    }
    let sysfs = Path::new("/sys/dev/block").join(linux_dev_major_minor(metadata));
    let canonical = fs::canonicalize(&sysfs).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("opened target has no live sysfs block identity: {error}"),
        )
    })?;
    let device_name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid sysfs device name"))?;
    let subsystem = fs::canonicalize(sysfs.join("subsystem"))?;
    if subsystem.file_name().and_then(|name| name.to_str()) != Some("block") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "opened target is not attached to the sysfs block subsystem",
        ));
    }
    let uevent = fs::read_to_string(sysfs.join("uevent"))?;
    if read_uevent_value(&uevent, "DEVNAME") != Some(device_name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "opened target sysfs DEVNAME does not match its block identity",
        ));
    }

    let expected_name = match &target.kind {
        SlotWalTargetKind::NullBlock => target.open_path().file_name(),
        SlotWalTargetKind::RamBlockDisk => target.open_path().file_name(),
        SlotWalTargetKind::ZcBlockRamDisk => target.open_path().file_name(),
        SlotWalTargetKind::ZcNetworkBlockClient => {
            if target.label() != "/dev/zcnblk0" || !Path::new(ZCNBLK_MODULE_SYSFS).is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "network target must be live /dev/zcnblk0 from zcnblk_client",
                ));
            }
            target.open_path().file_name()
        }
        SlotWalTargetKind::PartUuid(partuuid) => {
            if read_uevent_value(&uevent, "PARTUUID")
                .is_none_or(|value| !value.eq_ignore_ascii_case(partuuid))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("opened block device does not have PARTUUID={partuuid}"),
                ));
            }
            None
        }
    };
    if let Some(expected_name) = expected_name
        && expected_name.to_str() != Some(device_name)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "target path claims {:?}, but its live sysfs identity is {device_name:?}",
                expected_name
            ),
        ));
    }
    Ok(sysfs)
}

fn validate_target_not_live(
    target: &SlotWalTarget,
    metadata: &fs::Metadata,
    sysfs: &Path,
) -> io::Result<()> {
    let major_minor = linux_dev_major_minor(metadata);
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")?;
    if mountinfo.lines().any(|line| {
        line.split_whitespace()
            .nth(2)
            .is_some_and(|value| value == major_minor)
    }) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing write to mounted {}", target.label()),
        ));
    }

    let mut holders = fs::read_dir(sysfs.join("holders"))?;
    if holders.next().transpose()?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing write to {} while it has device holders",
                target.label()
            ),
        ));
    }

    if let Ok(swaps) = fs::read_to_string("/proc/swaps") {
        for swap_path in swaps
            .lines()
            .skip(1)
            .filter_map(|line| line.split_whitespace().next())
        {
            if fs::metadata(swap_path).ok().is_some_and(|swap| {
                swap.file_type().is_block_device() && swap.rdev() == metadata.rdev()
            }) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("refusing write to active swap {}", target.label()),
                ));
            }
        }
    }
    Ok(())
}

fn open_validated_target(
    target: &SlotWalTarget,
    expected_device_bytes: u64,
) -> io::Result<(Arc<fs::File>, fs::Metadata, PathBuf)> {
    let open_path = target_open_path(target)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&open_path)?;
    let metadata = file.metadata()?;
    let sysfs = verify_target_identity(target, &metadata)?;
    validate_slot_wal_write_target_safety(target, &metadata)?;
    validate_target_not_live(target, &metadata, &sysfs)?;

    let current = fs::metadata(target.open_path())?;
    if current.rdev() != metadata.rdev() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target path identity changed during validation",
        ));
    }
    let device_bytes = block_device_size(file.as_raw_fd())?;
    if device_bytes != expected_device_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target capacity changed during validation",
        ));
    }
    Ok((Arc::new(file), metadata, sysfs))
}

fn queue_max_hw_bytes(sysfs: &Path) -> Option<usize> {
    let read_limit = |path: &Path| {
        fs::read_to_string(path.join("queue/max_hw_sectors_kb"))
            .ok()?
            .trim()
            .parse::<usize>()
            .ok()?
            .checked_mul(1024)
    };

    read_limit(sysfs).or_else(|| {
        let canonical = fs::canonicalize(sysfs).ok()?;
        read_limit(canonical.parent()?)
    })
}

fn max_queue_fragments_per_logical_op(queue_max_bytes: usize) -> usize {
    MAX_TRANSFER_BYTES.div_ceil(queue_max_bytes.max(1))
}

fn allocation_alignment(buffer_mode: SlotWalBufferMode) -> io::Result<usize> {
    match buffer_mode {
        SlotWalBufferMode::SmallPages => super::page_size(),
        SlotWalBufferMode::HugeTlb => super::default_hugepage_size(),
    }
}

fn validate_direct_io_geometry(
    required_alignment: usize,
    buffer_mode: SlotWalBufferMode,
) -> io::Result<()> {
    if required_alignment == 0 || !required_alignment.is_power_of_two() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid direct-I/O alignment {required_alignment}"),
        ));
    }
    if REQUEST_ALIGNMENT as usize % required_alignment != 0
        || SIZE_CLASSES
            .iter()
            .any(|size| size % required_alignment != 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "enterprise profile uses 4 KiB offsets/transfers and cannot satisfy target direct-I/O alignment {required_alignment}"
            ),
        ));
    }
    let alignment = allocation_alignment(buffer_mode)?;
    if alignment % required_alignment != 0 || MAX_TRANSFER_BYTES % required_alignment != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "buffer mode alignment {alignment} cannot satisfy target direct-I/O alignment {required_alignment}"
            ),
        ));
    }
    Ok(())
}

fn validate_buffer_geometry(
    buffers: &FixedSendBuffers,
    required_alignment: usize,
) -> io::Result<()> {
    if buffers.base_addr() % required_alignment != 0 || buffers.stride() % required_alignment != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "allocated direct-I/O buffers base=0x{:x} stride={} do not satisfy alignment {required_alignment}",
                buffers.base_addr(),
                buffers.stride()
            ),
        ));
    }
    Ok(())
}

fn sync_transfer(
    fd: RawFd,
    buffer: *mut u8,
    len: usize,
    offset: u64,
    kind: IoKind,
) -> io::Result<()> {
    let mut done = 0usize;
    while done < len {
        let ret = match kind {
            IoKind::Read => unsafe {
                libc::pread(
                    fd,
                    buffer.add(done).cast(),
                    len - done,
                    (offset + done as u64) as libc::off_t,
                )
            },
            IoKind::Write => unsafe {
                libc::pwrite(
                    fd,
                    buffer.add(done).cast_const().cast(),
                    len - done,
                    (offset + done as u64) as libc::off_t,
                )
            },
        };
        if ret < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ret == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "zero-length block I/O completion",
            ));
        }
        done += ret as usize;
    }
    Ok(())
}

fn run_sync_worker(
    worker: usize,
    cfg: RunConfig,
    layout: WorkloadLayout,
    target_file: Arc<fs::File>,
    buffer_mode: SlotWalBufferMode,
    required_alignment: usize,
    planned_cpu: Option<usize>,
    gate: Arc<StartGate>,
    ready: mpsc::Sender<WorkerInit>,
) -> io::Result<WorkerResult> {
    let affinity = pin_current_thread_if_requested_to_cpu(
        "zcworkload-worker",
        worker,
        cfg.pin_workers,
        planned_cpu,
    );
    if cfg.pin_workers && !affinity.applied && zc_topology_strict_enabled() {
        return Err(io::Error::other(format!(
            "strict topology requires worker {worker} affinity to be applied"
        )));
    }
    let numa_node = (affinity.target_cpu >= 0)
        .then(|| cpu_numa_node(affinity.target_cpu as usize))
        .flatten();
    let buffers = buffer_mode.allocate_for_worker(2, MAX_TRANSFER_BYTES, numa_node)?;
    validate_buffer_geometry(&buffers, required_alignment)?;
    fill_write_buffers(&buffers, 1, cfg.seed ^ worker as u64);
    let fd = target_file.as_raw_fd();
    let mut generator = WorkloadGenerator::new(layout, cfg.seed, worker);
    let mut counters = CounterSet::default();
    let mut latency = LatencyHistogram::new();
    let mut read_latency = LatencyHistogram::new();
    let mut write_latency = LatencyHistogram::new();
    let mut schedule_lag = LatencyHistogram::new();

    ready
        .send(WorkerInit::Ready { worker })
        .map_err(|_| io::Error::other("zcworkload coordinator exited during worker setup"))?;
    drop(ready);
    let started = gate.wait()?;
    let tid = current_tid();
    let start_switches = read_thread_context_switches(tid).unwrap_or_default();
    let start_cpu_time = thread_cpu_time().unwrap_or_default();
    let deadline = started + cfg.duration;
    let mut pacer = RatePacer::new(started, worker_rate(cfg.target_iops, cfg.workers, worker));
    loop {
        if let Some(limit) = cfg.ops_per_worker {
            if counters.ops >= limit {
                break;
            }
        } else if Instant::now() >= deadline {
            break;
        }
        let due = pacer.due();
        if cfg.target_iops != 0 {
            if cfg.ops_per_worker.is_none() && due >= deadline {
                break;
            }
            wait_until(due, cfg.pace_spin);
        }
        let request = generator.next();
        let buffer = if request.kind == IoKind::Write {
            let ptr = buffers.ptr(0);
            stamp_write_buffer(ptr, request, worker);
            ptr
        } else {
            buffers.ptr(1)
        };
        let submitted = Instant::now();
        if cfg.target_iops != 0 {
            schedule_lag.record_duration(submitted.saturating_duration_since(due));
        }
        sync_transfer(fd, buffer, request.len, request.offset, request.kind)?;
        if should_sample(request.sequence, cfg.latency_sample_rate) {
            let duration = if cfg.target_iops == 0 {
                submitted.elapsed()
            } else {
                due.elapsed()
            };
            latency.record_duration(duration);
            match request.kind {
                IoKind::Read => read_latency.record_duration(duration),
                IoKind::Write => write_latency.record_duration(duration),
            }
        }
        counters.record(request);
        pacer.advance();
    }
    let elapsed = started.elapsed();
    let cpu = thread_cpu_time()
        .unwrap_or(start_cpu_time)
        .saturating_sub(start_cpu_time);
    let switches = read_thread_context_switches(tid).unwrap_or(start_switches);
    Ok(WorkerResult {
        worker,
        elapsed,
        cpu,
        counters,
        voluntary_switches: switches.voluntary.saturating_sub(start_switches.voluntary),
        involuntary_switches: switches
            .involuntary
            .saturating_sub(start_switches.involuntary),
        migrations: switches
            .migrations
            .saturating_sub(start_switches.migrations),
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
        end_cpu: current_cpu(),
        numa_node,
        latency,
        read_latency,
        write_latency,
        schedule_lag,
        ring_stats: RawRingStats::default(),
    })
}

fn run_uring_worker(
    worker: usize,
    cfg: RunConfig,
    layout: WorkloadLayout,
    target_file: Arc<fs::File>,
    buffer_mode: SlotWalBufferMode,
    required_alignment: usize,
    planned_cpu: Option<usize>,
    gate: Arc<StartGate>,
    ready: mpsc::Sender<WorkerInit>,
) -> io::Result<WorkerResult> {
    let affinity = pin_current_thread_if_requested_to_cpu(
        "zcworkload-worker",
        worker,
        cfg.pin_workers,
        planned_cpu,
    );
    if cfg.pin_workers && !affinity.applied && zc_topology_strict_enabled() {
        return Err(io::Error::other(format!(
            "strict topology requires worker {worker} affinity to be applied"
        )));
    }
    let numa_node = (affinity.target_cpu >= 0)
        .then(|| cpu_numa_node(affinity.target_cpu as usize))
        .flatten();
    let buffer_count = cfg
        .depth
        .checked_mul(2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer count overflow"))?;
    let buffers = buffer_mode.allocate_for_worker(buffer_count, MAX_TRANSFER_BYTES, numa_node)?;
    validate_buffer_geometry(&buffers, required_alignment)?;
    fill_write_buffers(&buffers, cfg.depth, cfg.seed ^ worker as u64);
    let mut fds = [target_file.as_raw_fd()];
    let mut iovecs = buffers.iovecs(MAX_TRANSFER_BYTES);
    let mut ring = RawRing::new_with_options(
        cfg.ring_entries,
        cfg.ring_entries.saturating_mul(2),
        RawRingOptions {
            stats_enabled: true,
            cqe_mode: RawRingCqeMode::Cqe16,
            sq_mode: RawRingSqMode::Normal,
            io_poll_mode: crate::RawRingIoPollMode::Off,
            registered_ring_fd: false,
            sq_thread_idle_ms: 0,
        },
    )?;
    ring.register_files(&mut fds)?;
    ring.register_buffers(&mut iovecs)?;

    let mut generator = WorkloadGenerator::new(layout, cfg.seed, worker);
    let mut counters = CounterSet::default();
    let mut latency = LatencyHistogram::new();
    let mut read_latency = LatencyHistogram::new();
    let mut write_latency = LatencyHistogram::new();
    let mut schedule_lag = LatencyHistogram::new();
    let mut free_slots: Vec<usize> = (0..cfg.depth).rev().collect();
    let mut requests = vec![None::<WorkloadRequest>; cfg.depth];
    let mut submitted_at = vec![None::<Instant>; cfg.depth];
    let mut submitted = 0u64;
    let mut completed = 0u64;
    let mut stop_submitting = false;

    ready
        .send(WorkerInit::Ready { worker })
        .map_err(|_| io::Error::other("zcworkload coordinator exited during worker setup"))?;
    drop(ready);
    let started = gate.wait()?;
    let tid = current_tid();
    let start_switches = read_thread_context_switches(tid).unwrap_or_default();
    let start_cpu_time = thread_cpu_time().unwrap_or_default();
    let deadline = started + cfg.duration;
    let mut pacer = RatePacer::new(started, worker_rate(cfg.target_iops, cfg.workers, worker));

    'run: while !stop_submitting || completed < submitted {
        while !stop_submitting && !free_slots.is_empty() {
            if let Some(limit) = cfg.ops_per_worker {
                if submitted >= limit {
                    stop_submitting = true;
                    break;
                }
            } else if Instant::now() >= deadline {
                stop_submitting = true;
                break;
            }

            let due = pacer.due();
            if cfg.target_iops != 0 && Instant::now() < due {
                if cfg.ops_per_worker.is_none() && due >= deadline {
                    stop_submitting = true;
                    break;
                }
                break;
            }

            let slot = free_slots.pop().expect("free slot checked");
            let request = generator.next();
            let submitted_now = Instant::now();
            if cfg.target_iops != 0 {
                schedule_lag.record_duration(submitted_now.saturating_duration_since(due));
            }
            let len = u32::try_from(request.len).expect("transfer sizes fit in u32");
            match request.kind {
                IoKind::Read => {
                    let buffer_index = cfg.depth + slot;
                    ring.queue_read_fixed_file(
                        0,
                        buffers.ptr(buffer_index),
                        len,
                        request.offset,
                        u16::try_from(buffer_index).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "registered read buffer index exceeds u16",
                            )
                        })?,
                        slot as u64,
                    )?;
                }
                IoKind::Write => {
                    let buffer = buffers.ptr(slot);
                    stamp_write_buffer(buffer, request, worker);
                    ring.queue_write_fixed_file(
                        0,
                        buffer.cast_const(),
                        len,
                        request.offset,
                        u16::try_from(slot).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "registered write buffer index exceeds u16",
                            )
                        })?,
                        slot as u64,
                    )?;
                }
            }
            requests[slot] = Some(request);
            submitted_at[slot] = should_sample(request.sequence, cfg.latency_sample_rate)
                .then_some(if cfg.target_iops == 0 {
                    submitted_now
                } else {
                    due
                });
            submitted += 1;
            pacer.advance();
        }

        ring.submit_pending()?;
        if completed == submitted {
            if !stop_submitting && cfg.target_iops != 0 {
                let due = pacer.due();
                if cfg.ops_per_worker.is_none() && due >= deadline {
                    stop_submitting = true;
                } else {
                    wait_until(due, cfg.pace_spin);
                }
            }
            continue;
        }
        for batch_index in 0..cfg.completion_batch {
            let cqe = if batch_index == 0 {
                match ring.try_pop_cqe() {
                    Some(cqe) => cqe,
                    None if wait_for_due_before_completion(
                        stop_submitting,
                        cfg.target_iops,
                        free_slots.len(),
                    ) =>
                    {
                        let due = pacer.due();
                        if cfg.ops_per_worker.is_none() && due >= deadline {
                            stop_submitting = true;
                            ring.wait_cqe_min(1)?
                        } else {
                            wait_until(due, cfg.pace_spin);
                            continue 'run;
                        }
                    }
                    None => ring.wait_cqe_min(1)?,
                }
            } else {
                match ring.try_pop_cqe() {
                    Some(cqe) => cqe,
                    None => break,
                }
            };
            let slot = cqe.user_data as usize;
            if slot >= cfg.depth {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("completion returned invalid slot {slot}"),
                ));
            }
            if cqe.res < 0 {
                return Err(io::Error::from_raw_os_error(-cqe.res));
            }
            let request = requests[slot].take().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "completion slot was not in flight",
                )
            })?;
            if cqe.res as usize != request.len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "short {} completion: got {} expected {}",
                        request.kind.as_str(),
                        cqe.res,
                        request.len
                    ),
                ));
            }
            if let Some(started) = submitted_at[slot].take() {
                let duration = started.elapsed();
                latency.record_duration(duration);
                match request.kind {
                    IoKind::Read => read_latency.record_duration(duration),
                    IoKind::Write => write_latency.record_duration(duration),
                }
            }
            counters.record(request);
            completed += 1;
            free_slots.push(slot);
        }
    }

    let elapsed = started.elapsed();
    let cpu = thread_cpu_time()
        .unwrap_or(start_cpu_time)
        .saturating_sub(start_cpu_time);
    let switches = read_thread_context_switches(tid).unwrap_or(start_switches);
    let ring_stats = ring.stats();
    ring.unregister_buffers()?;
    ring.unregister_files()?;
    Ok(WorkerResult {
        worker,
        elapsed,
        cpu,
        counters,
        voluntary_switches: switches.voluntary.saturating_sub(start_switches.voluntary),
        involuntary_switches: switches
            .involuntary
            .saturating_sub(start_switches.involuntary),
        migrations: switches
            .migrations
            .saturating_sub(start_switches.migrations),
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
        end_cpu: current_cpu(),
        numa_node,
        latency,
        read_latency,
        write_latency,
        schedule_lag,
        ring_stats,
    })
}

fn latency_fields(prefix: &str, histogram: &LatencyHistogram) -> String {
    format!(
        "{prefix}_samples={} {prefix}_min_ns={} {prefix}_avg_ns={} {prefix}_p50_ns={} \
         {prefix}_p95_ns={} {prefix}_p99_ns={} {prefix}_p999_ns={} {prefix}_max_ns={}",
        histogram.count,
        histogram.min_ns(),
        histogram.avg_ns(),
        histogram.percentile_ns(50, 100),
        histogram.percentile_ns(95, 100),
        histogram.percentile_ns(99, 100),
        histogram.percentile_ns(999, 1000),
        histogram.max_ns(),
    )
}

fn hctx_affinity_enabled() -> Option<bool> {
    fs::read_to_string(format!("{ZCNBLK_MODULE_SYSFS}/parameters/hctx_affinity"))
        .ok()
        .map(|value| matches!(value.trim(), "1" | "Y" | "y"))
}

fn estimate_memlock_bytes(cfg: &RunConfig, buffer_mode: SlotWalBufferMode) -> io::Result<u64> {
    let alignment = allocation_alignment(buffer_mode)?;
    let stride = MAX_TRANSFER_BYTES
        .checked_add(alignment - 1)
        .map(|value| value / alignment * alignment)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer stride overflow"))?;
    let buffers = cfg
        .workers
        .checked_mul(cfg.depth)
        .and_then(|value| value.checked_mul(2))
        .and_then(|value| value.checked_mul(stride))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "memlock estimate overflow"))?;
    let ring_bytes_per_worker = (cfg.ring_entries as usize)
        .checked_mul(64 + 16 * 2 + 8)
        .and_then(|value| value.checked_add(alignment.saturating_mul(4)))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ring estimate overflow"))?;
    let subtotal = buffers
        .checked_add(
            cfg.workers
                .checked_mul(ring_bytes_per_worker)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "ring estimate overflow")
                })?,
        )
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "memlock estimate overflow"))?;
    let with_headroom = subtotal
        .checked_add(subtotal / 4)
        .and_then(|value| value.checked_add(cfg.workers.saturating_mul(1024 * 1024)))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "memlock estimate overflow"))?;
    u64::try_from(with_headroom)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "memlock estimate exceeds u64"))
}

fn validate_topology_coverage(
    cfg: &RunConfig,
    maps: &TopologyMaps,
    planned_cpus: &[usize],
) -> Vec<String> {
    let mut issues = Vec::new();
    if maps.lanes.len() != cfg.workers {
        issues.push(format!(
            "lane map has {} entries but the run has {} workers/lanes",
            maps.lanes.len(),
            cfg.workers
        ));
    }
    let mut workers = HashSet::new();
    for (expected_lane, entry) in maps.lanes.iter().enumerate() {
        if entry.lane != expected_lane {
            issues.push(format!(
                "lane map does not cover contiguous lane {expected_lane}"
            ));
        }
        if entry.worker >= cfg.workers {
            issues.push(format!(
                "lane {} names out-of-range worker {}",
                entry.lane, entry.worker
            ));
        } else {
            workers.insert(entry.worker);
            if planned_cpus
                .get(entry.worker)
                .is_some_and(|cpu| *cpu != entry.cpu)
            {
                issues.push(format!(
                    "lane {} reports worker {} on CPU {}, planned CPU is {}",
                    entry.lane, entry.worker, entry.cpu, planned_cpus[entry.worker]
                ));
            }
        }
        if entry.cpu >= libc::CPU_SETSIZE as usize
            || !Path::new(&format!("/sys/devices/system/cpu/cpu{}", entry.cpu)).exists()
        {
            issues.push(format!(
                "lane {} names unavailable CPU {}",
                entry.lane, entry.cpu
            ));
        }
    }
    if workers.len() != cfg.workers {
        issues.push("lane map does not cover every worker exactly once".to_string());
    }

    if maps.kthreads.len() != cfg.workers {
        issues.push(format!(
            "kthread map has {} entries but the run has {} workers/lanes",
            maps.kthreads.len(),
            cfg.workers
        ));
    }
    for (expected, entry) in maps.kthreads.iter().enumerate() {
        if entry.kthread != expected {
            issues.push(format!(
                "kthread map does not cover contiguous kthread {expected}"
            ));
        }
        if entry.cpu >= libc::CPU_SETSIZE as usize
            || !Path::new(&format!("/sys/devices/system/cpu/cpu{}", entry.cpu)).exists()
        {
            issues.push(format!(
                "kthread {} names unavailable CPU {}",
                entry.kthread, entry.cpu
            ));
        }
    }
    issues
}

fn preflight(
    cfg: &RunConfig,
    target: &SlotWalTarget,
    buffer_mode: SlotWalBufferMode,
    maps: &TopologyMaps,
    planned_cpus: &[usize],
) -> io::Result<bool> {
    let mut issues = Vec::new();
    if !cfg.pin_workers {
        issues.push("workers are not pinned".to_string());
    } else if !explicit_affinity_map_configured() {
        issues.push(
            "worker pinning uses an implicit map; set URING_PLAY_PIN_CPU_LIST explicitly"
                .to_string(),
        );
    }
    if buffer_mode == SlotWalBufferMode::SmallPages {
        issues.push(
            "small-page buffers are a smoke setting; use hugetlb for representative runs"
                .to_string(),
        );
    }
    if cfg.engine == Engine::Uring {
        let locked_bytes = estimate_memlock_bytes(cfg, buffer_mode)?;
        if let Some(limit) = memlock_rlimit_bytes()? {
            if limit < locked_bytes {
                issues.push(format!(
                    "memlock limit {limit} is below estimated locked footprint {locked_bytes}, including ring overhead and 25% headroom"
                ));
            }
        }
        if cfg.ring_entries < (cfg.depth as u32).saturating_mul(2).max(64) {
            issues.push(format!(
                "ring_entries={} is too small for depth={}; use at least {}",
                cfg.ring_entries,
                cfg.depth,
                (cfg.depth as u32).saturating_mul(2).max(64)
            ));
        }
        if !env_truthy("URING_PLAY_ENTER_NO_IOWAIT") {
            issues.push("URING_PLAY_ENTER_NO_IOWAIT is disabled".to_string());
        }
    }
    if cfg.engine == Engine::Uring && cfg.completion_batch == 1 && cfg.depth > 1 {
        issues.push("completion batching is disabled at depth greater than one".to_string());
    }

    let network_edge = matches!(&target.kind, SlotWalTargetKind::ZcNetworkBlockClient);
    if network_edge {
        if maps.lanes.is_empty() {
            issues.push("lane-to-worker/CPU mapping is unreported; pass --lane-map".to_string());
        }
        if maps.kthreads.is_empty() {
            issues.push("client kthread CPU mapping is unreported; pass --kthread-map".to_string());
        }
        if !maps.lanes.is_empty() || !maps.kthreads.is_empty() {
            issues.extend(validate_topology_coverage(cfg, maps, planned_cpus));
        }
        if hctx_affinity_enabled() != Some(true) {
            issues.push("zcnblk hctx_affinity is not proven enabled".to_string());
        }
        if cfg.completion == CompletionContract::Unspecified {
            issues.push("completion contract is unspecified".to_string());
        }
        if matches!(cfg.effective_depth_per_worker(), 1 | 2 | 4 | 8 | 16)
            && cfg.transport_rtt_ns.is_none()
        {
            issues.push(
                "low-depth network run has no measured transport RTT; pass --transport-rtt-ns"
                    .to_string(),
            );
        }
    }

    if issues.is_empty() {
        return Ok(true);
    }
    for issue in &issues {
        eprintln!("PERF WARNING: zcworkload: {issue}");
    }
    if zc_topology_strict_enabled() {
        return Err(io::Error::other(format!(
            "TOPOLOGY ERROR: zcworkload has {} unresolved performance preflight issue(s)",
            issues.len()
        )));
    }
    Ok(false)
}

fn planned_cpus(target: &SlotWalTarget, cfg: &RunConfig) -> io::Result<Vec<usize>> {
    if !cfg.pin_workers {
        return Ok(Vec::new());
    }
    let metadata = fs::metadata(target.open_path())?;
    let topology = BlockDeviceTopology::from_metadata(&metadata)?;
    Ok(topology.planned_cpus(cfg.workers, true))
}

fn resolve_buffer_mode(cfg: &RunConfig) -> io::Result<SlotWalBufferMode> {
    let count = cfg
        .workers
        .checked_mul(cfg.depth)
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer count overflow"))?;
    match cfg.buffer_choice {
        BufferChoice::Auto => {
            standard_slot_wal_buffer_mode("zcworkload", count, MAX_TRANSFER_BYTES)
        }
        BufferChoice::SmallPages => Ok(SlotWalBufferMode::SmallPages),
        BufferChoice::HugeTlb => {
            let needed = count
                .checked_mul(super::default_hugepage_size()?)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "hugetlb estimate overflow")
                })?;
            warn_hugetlb_pressure("zcworkload", needed)?;
            Ok(SlotWalBufferMode::HugeTlb)
        }
    }
}

fn validate_run_config(cfg: &RunConfig) -> io::Result<()> {
    if cfg.target.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "run requires --target",
        ));
    }
    validate_target_spec(&cfg.target)?;
    parse_topology_maps(cfg)?;
    if cfg.workers == 0 || cfg.depth == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workers and depth must be non-zero",
        ));
    }
    if cfg.repeats == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "repeats must be non-zero",
        ));
    }
    if cfg.depth.checked_mul(2).is_none_or(|count| count > 16_384) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "twice the per-worker depth must not exceed 16384 registered buffers",
        ));
    }
    if cfg.ring_entries < cfg.depth as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ring entries must be at least the per-worker depth",
        ));
    }
    if cfg.duration.is_zero() && cfg.ops_per_worker.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "duration must be non-zero unless --ops-per-worker is supplied",
        ));
    }
    if cfg.target_iops != 0 && cfg.target_iops < cfg.workers as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a non-zero aggregate rate must be at least the worker count",
        ));
    }
    if cfg.completion_batch == 0 || cfg.completion_batch > cfg.depth {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "completion batch must be in 1..=depth",
        ));
    }
    if cfg.transport_rtt_ns == Some(0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "transport RTT must be a positive measured value",
        ));
    }
    WorkloadLayout::new(cfg.capacity as u64)?;
    Ok(())
}

fn run_workload_once(
    cfg: RunConfig,
    repeat_index: usize,
    repeat_count: usize,
) -> io::Result<RunSummary> {
    validate_run_config(&cfg)?;
    let topology_maps = parse_topology_maps(&cfg)?;
    let buffer_mode = resolve_buffer_mode(&cfg)?;
    let (target, device_bytes, required_alignment, segment_bytes) = validate_slot_wal_common(
        &cfg.target,
        cfg.capacity,
        REQUEST_ALIGNMENT as usize,
        SlotWalMode::Write,
        buffer_mode,
    )?;
    validate_direct_io_geometry(required_alignment, buffer_mode)?;
    let layout = WorkloadLayout::new(cfg.capacity as u64)?;
    let cpus = planned_cpus(&target, &cfg)?;
    let topology_preflight_passed = preflight(&cfg, &target, buffer_mode, &topology_maps, &cpus)?;
    let (target_file, target_metadata, target_sysfs) =
        open_validated_target(&target, device_bytes)?;
    let queue_max_bytes = queue_max_hw_bytes(&target_sysfs);

    println!(
        "zcworkload-plan: repeat={} repeats={} target={} capacity={} device_bytes={} engine={} workers={} \
         configured_depth_per_worker={} effective_depth_per_worker={} aggregate_depth={} \
         ring_entries={} duration_seconds={:.3} \
         ops_per_worker={} target_iops={} buffers={} required_alignment={} segment_bytes={} \
         completion_batch={} latency_sample_rate={} completion_contract={} \
         logical_transfer_bytes=4096,8192,16384,32768,65536 queue_max_hw_bytes={} \
         max_queue_fragments_per_logical_op={}",
        repeat_index,
        repeat_count,
        target.label(),
        cfg.capacity,
        device_bytes,
        cfg.engine.as_str(),
        cfg.workers,
        cfg.depth,
        cfg.effective_depth_per_worker(),
        cfg.aggregate_depth(),
        cfg.ring_entries,
        cfg.duration.as_secs_f64(),
        cfg.ops_per_worker
            .map(|value| value.to_string())
            .unwrap_or_else(|| "duration".to_string()),
        cfg.target_iops,
        buffer_mode.as_str(),
        required_alignment,
        segment_bytes,
        cfg.completion_batch,
        cfg.latency_sample_rate,
        cfg.completion.as_str(),
        queue_max_bytes
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        queue_max_bytes
            .map(max_queue_fragments_per_logical_op)
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    );
    println!(
        "zcworkload-layout: data_start={} data_bytes={} user_start={} user_bytes={} \
         journal_start={} journal_bytes={}",
        layout.data.start,
        layout.data.len,
        layout.user.start,
        layout.user.len,
        layout.journal.start,
        layout.journal.len,
    );
    println!(
        "zcworkload-topology: worker_cpu_map={} lane_cpu_map={} kthread_cpu_map={} \
         pin_workers={} hctx_affinity={} strict={}",
        format_cpu_list(&cpus),
        topology_maps.lane_label(),
        topology_maps.kthread_label(),
        cfg.pin_workers,
        hctx_affinity_enabled()
            .map(|enabled| enabled.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        zc_topology_strict_enabled(),
    );

    let gate = Arc::new(StartGate::new());
    let (init_tx, init_rx) = mpsc::channel();
    let mut handles = Vec::with_capacity(cfg.workers);
    for worker in 0..cfg.workers {
        let worker_cfg = cfg.clone();
        let worker_file = Arc::clone(&target_file);
        let worker_gate = Arc::clone(&gate);
        let worker_ready = init_tx.clone();
        let worker_failed = init_tx.clone();
        let cpu = cpus.get(worker).copied();
        handles.push(thread::spawn(move || {
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    match worker_cfg.engine {
                        Engine::Uring => run_uring_worker(
                            worker,
                            worker_cfg,
                            layout,
                            worker_file,
                            buffer_mode,
                            required_alignment,
                            cpu,
                            worker_gate,
                            worker_ready,
                        ),
                        Engine::Sync => run_sync_worker(
                            worker,
                            worker_cfg,
                            layout,
                            worker_file,
                            buffer_mode,
                            required_alignment,
                            cpu,
                            worker_gate,
                            worker_ready,
                        ),
                    }
                }));
            match outcome {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(error)) => {
                    let _ = worker_failed.send(WorkerInit::Failed {
                        worker,
                        error: error.to_string(),
                    });
                    Err(error)
                }
                Err(_) => {
                    let error = io::Error::other(format!("zcworkload worker {worker} panicked"));
                    let _ = worker_failed.send(WorkerInit::Failed {
                        worker,
                        error: error.to_string(),
                    });
                    Err(error)
                }
            }
        }));
    }
    drop(init_tx);

    let mut ready_workers = HashSet::new();
    let mut startup_error = None;
    while ready_workers.len() < cfg.workers {
        match init_rx.recv() {
            Ok(WorkerInit::Ready { worker }) => {
                if worker >= cfg.workers || !ready_workers.insert(worker) {
                    startup_error = Some(io::Error::other(format!(
                        "invalid duplicate readiness from worker {worker}"
                    )));
                    break;
                }
            }
            Ok(WorkerInit::Failed { worker, error }) => {
                startup_error = Some(io::Error::other(format!(
                    "worker {worker} initialization failed: {error}"
                )));
                break;
            }
            Err(_) => {
                startup_error = Some(io::Error::other(
                    "worker initialization channel closed before all workers were ready",
                ));
                break;
            }
        }
    }
    if let Some(error) = startup_error {
        gate.cancel();
        for handle in handles {
            let _ = handle.join();
        }
        return Err(error);
    }

    if let Err(error) = validate_target_not_live(&target, &target_metadata, &target_sysfs) {
        gate.cancel();
        for handle in handles {
            let _ = handle.join();
        }
        return Err(error);
    }
    let wall_started = Instant::now();
    if let Err(error) = gate.start(wall_started) {
        gate.cancel();
        for handle in handles {
            let _ = handle.join();
        }
        return Err(error);
    }
    let mut results = Vec::with_capacity(cfg.workers);
    for handle in handles {
        results.push(
            handle
                .join()
                .map_err(|_| io::Error::other("zcworkload worker panicked"))??,
        );
    }
    let wall = wall_started.elapsed();
    results.sort_by_key(|result| result.worker);

    let mut counters = CounterSet::default();
    let mut latency = LatencyHistogram::new();
    let mut read_latency = LatencyHistogram::new();
    let mut write_latency = LatencyHistogram::new();
    let mut schedule_lag = LatencyHistogram::new();
    let mut cpu = Duration::ZERO;
    let mut voluntary = 0u64;
    let mut involuntary = 0u64;
    let mut migrations = 0u64;
    let mut ring_stats = RawRingStats::default();
    for result in &results {
        counters.merge(result.counters);
        latency.merge(&result.latency);
        read_latency.merge(&result.read_latency);
        write_latency.merge(&result.write_latency);
        schedule_lag.merge(&result.schedule_lag);
        cpu += result.cpu;
        voluntary += result.voluntary_switches;
        involuntary += result.involuntary_switches;
        migrations += result.migrations;
        ring_stats.sqes_queued += result.ring_stats.sqes_queued;
        ring_stats.submit_syscalls += result.ring_stats.submit_syscalls;
        ring_stats.wait_syscalls += result.ring_stats.wait_syscalls;
        ring_stats.sqes_submitted += result.ring_stats.sqes_submitted;
        ring_stats.cqes_popped += result.ring_stats.cqes_popped;
        ring_stats.try_pop_empty += result.ring_stats.try_pop_empty;
        ring_stats.wait_cqe_calls += result.ring_stats.wait_cqe_calls;
        ring_stats.submit_short += result.ring_stats.submit_short;
        ring_stats.cqe_spin_loops += result.ring_stats.cqe_spin_loops;
    }
    for result in &results {
        println!(
            "zcworkload-worker: worker={} target_cpu={} affinity_applied={} end_cpu={} numa_node={} \
             ops={} reads={} writes={} bytes={} seconds={:.6} ops_per_sec={:.0} \
             cpu_seconds={:.6} voluntary_ctxt_switches={} involuntary_ctxt_switches={} migrations={}",
            result.worker,
            result.target_cpu,
            result.affinity_applied,
            result.end_cpu,
            result
                .numa_node
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            result.counters.ops,
            result.counters.reads,
            result.counters.writes,
            result.counters.bytes,
            result.elapsed.as_secs_f64(),
            result.counters.ops as f64 / result.elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
            result.cpu.as_secs_f64(),
            result.voluntary_switches,
            result.involuntary_switches,
            result.migrations,
        );
    }

    let seconds = wall.as_secs_f64().max(f64::MIN_POSITIVE);
    let iops = counters.ops as f64 / seconds;
    let affinity_preflight_passed =
        !cfg.pin_workers || results.iter().all(|result| result.affinity_applied);
    if !affinity_preflight_passed {
        eprintln!(
            "PERF WARNING: zcworkload: requested worker affinity was not fully applied; results are non-representative"
        );
    }
    let topology_preflight_passed = topology_preflight_passed && affinity_preflight_passed;
    let network_edge = matches!(&target.kind, SlotWalTargetKind::ZcNetworkBlockClient);
    let lane_count = if network_edge {
        topology_maps.lanes.len()
    } else {
        cfg.workers
    };
    println!(
        "zcworkload-result: repeat={} repeats={} target={} engine={} completion_contract={} total_ops={} reads={} \
         writes={} bytes={} seconds={seconds:.6} ops_per_sec={iops:.0} MiBps={:.2} Gbitps={:.3} \
         configured_depth_per_worker={} effective_depth_per_worker={} workers={} lane_count={} \
         aggregate_depth={} transport_rtt_ns={} theoretical_iops=not-applicable-mixed-workload \
         actual_theoretical_percent=not-applicable measurement_scope=shared-system \
         topology_preflight_passed={} representative=false total_cpu_seconds={:.6} cpu_ns_per_op={:.1}",
        repeat_index,
        repeat_count,
        target.label(),
        cfg.engine.as_str(),
        cfg.completion.as_str(),
        counters.ops,
        counters.reads,
        counters.writes,
        counters.bytes,
        counters.bytes as f64 / (1024.0 * 1024.0) / seconds,
        counters.bytes as f64 * 8.0 / 1_000_000_000.0 / seconds,
        cfg.depth,
        cfg.effective_depth_per_worker(),
        cfg.workers,
        lane_count,
        cfg.aggregate_depth(),
        cfg.transport_rtt_ns
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unreported".to_string()),
        topology_preflight_passed,
        cpu.as_secs_f64(),
        cpu.as_secs_f64() * 1_000_000_000.0 / counters.ops.max(1) as f64,
    );
    let remote_reads = u64::from(network_edge).saturating_mul(counters.reads);
    let remote_ack_writes = if network_edge && cfg.completion == CompletionContract::RemoteAck {
        counters.writes
    } else {
        0
    };
    let local_ack_writes = if network_edge && cfg.completion == CompletionContract::LocalAck {
        counters.writes
    } else {
        0
    };
    let durable_writes = if cfg.completion == CompletionContract::Durable {
        counters.writes
    } else {
        0
    };
    println!(
        "zcworkload-completion: remote_reads={} remote_read_iops={:.0} \
         remote_ack_writes={} remote_ack_write_iops={:.0} local_ack_writes={} \
         local_ack_write_iops={:.0} durable_writes={} durable_write_iops={:.0} \
         explicit_sync_fua_drains=0 rtt_efficiency=not-reported \
         reason=mixed-workload-depth-not-attributed-by-completion-semantics",
        remote_reads,
        remote_reads as f64 / seconds,
        remote_ack_writes,
        remote_ack_writes as f64 / seconds,
        local_ack_writes,
        local_ack_writes as f64 / seconds,
        durable_writes,
        durable_writes as f64 / seconds,
    );
    println!(
        "zcworkload-context: voluntary_ctxt_switches={voluntary} \
         involuntary_ctxt_switches={involuntary} migrations={migrations} \
         ctxt_switches_per_1k_ops={:.3}",
        (voluntary + involuntary) as f64 * 1000.0 / counters.ops.max(1) as f64,
    );
    println!(
        "zcworkload-latency: sample_rate={} response_origin={} {} {} {} {}",
        cfg.latency_sample_rate,
        if cfg.target_iops == 0 {
            "submission"
        } else {
            "scheduled-due"
        },
        latency_fields("all", &latency),
        latency_fields("read", &read_latency),
        latency_fields("write", &write_latency),
        latency_fields("schedule_lag", &schedule_lag),
    );
    println!(
        "zcworkload-ring: sqes={} submissions={} submit_syscalls={} wait_syscalls={} \
         wait_calls={} cqes={} try_pop_empty={} submit_short={} cqe_spin_loops={}",
        ring_stats.sqes_queued,
        ring_stats.sqes_submitted,
        ring_stats.submit_syscalls,
        ring_stats.wait_syscalls,
        ring_stats.wait_cqe_calls,
        ring_stats.cqes_popped,
        ring_stats.try_pop_empty,
        ring_stats.submit_short,
        ring_stats.cqe_spin_loops,
    );
    for (index, spec) in STREAMS.iter().enumerate() {
        println!(
            "zcworkload-stream: index={index} name={} area={} ops={} observed_percent={:.3} \
             target_percent={:.3}",
            spec.name,
            spec.area.as_str(),
            counters.stream_ops[index],
            counters.stream_ops[index] as f64 * 100.0 / counters.ops.max(1) as f64,
            spec.weight as f64 * 100.0 / PROFILE_WEIGHT as f64,
        );
    }
    Ok(RunSummary {
        iops,
        mib_per_sec: counters.bytes as f64 / (1024.0 * 1024.0) / seconds,
    })
}

fn run_workload(cfg: RunConfig) -> io::Result<()> {
    validate_run_config(&cfg)?;
    let mut summaries = Vec::with_capacity(cfg.repeats);
    for repeat_index in 1..=cfg.repeats {
        summaries.push(run_workload_once(cfg.clone(), repeat_index, cfg.repeats)?);
    }
    let mut iops: Vec<_> = summaries.iter().map(|summary| summary.iops).collect();
    let mut mib_per_sec: Vec<_> = summaries
        .iter()
        .map(|summary| summary.mib_per_sec)
        .collect();
    iops.sort_by(f64::total_cmp);
    mib_per_sec.sort_by(f64::total_cmp);
    let middle = iops.len() / 2;
    let median = if iops.len() % 2 == 0 {
        (iops[middle - 1] + iops[middle]) / 2.0
    } else {
        iops[middle]
    };
    let median_mib = if mib_per_sec.len() % 2 == 0 {
        (mib_per_sec[middle - 1] + mib_per_sec[middle]) / 2.0
    } else {
        mib_per_sec[middle]
    };
    println!(
        "zcworkload-spread: repeats={} measurement_scope=shared-system \
         iops_min={:.0} iops_median={median:.0} iops_max={:.0} \
         MiBps_min={:.2} MiBps_median={median_mib:.2} MiBps_max={:.2} representative=false",
        cfg.repeats,
        iops[0],
        iops[iops.len() - 1],
        mib_per_sec[0],
        mib_per_sec[mib_per_sec.len() - 1],
    );
    Ok(())
}

#[derive(Serialize)]
struct SampleReport {
    profile: &'static str,
    seed: u64,
    capacity: u64,
    requests: u64,
    counters: CounterSet,
    invalid_alignment: u64,
    out_of_bounds: u64,
    streams: Vec<SampleStream>,
    sizes: Vec<SampleSize>,
}

#[derive(Serialize)]
struct SampleStream {
    index: usize,
    name: &'static str,
    area: AreaId,
    target_percent: f64,
    observed_percent: f64,
    operations: u64,
}

#[derive(Serialize)]
struct SampleSize {
    bytes: usize,
    operations: u64,
    observed_percent: f64,
}

fn sample_workload(capacity: u64, requests: u64, seed: u64, json: bool) -> io::Result<()> {
    if requests == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sample request count must be non-zero",
        ));
    }
    let layout = WorkloadLayout::new(capacity)?;
    let mut generator = WorkloadGenerator::new(layout, seed, 0);
    let mut counters = CounterSet::default();
    let mut invalid_alignment = 0u64;
    let mut out_of_bounds = 0u64;
    for _ in 0..requests {
        let request = generator.next();
        if request.offset % REQUEST_ALIGNMENT != 0 {
            invalid_alignment += 1;
        }
        let area = layout.area(request.area);
        if request.offset < area.start || request.offset + request.len as u64 > area.end() {
            out_of_bounds += 1;
        }
        counters.record(request);
    }
    let streams = STREAMS
        .iter()
        .enumerate()
        .map(|(index, spec)| SampleStream {
            index,
            name: spec.name,
            area: spec.area,
            target_percent: spec.weight as f64 * 100.0 / PROFILE_WEIGHT as f64,
            observed_percent: counters.stream_ops[index] as f64 * 100.0
                / counters.ops.max(1) as f64,
            operations: counters.stream_ops[index],
        })
        .collect();
    let sizes = SIZE_CLASSES
        .iter()
        .enumerate()
        .map(|(index, size)| SampleSize {
            bytes: *size,
            operations: counters.size_ops[index],
            observed_percent: counters.size_ops[index] as f64 * 100.0 / counters.ops.max(1) as f64,
        })
        .collect();
    let report = SampleReport {
        profile: "enterprise-mixed-v1",
        seed,
        capacity,
        requests,
        counters,
        invalid_alignment,
        out_of_bounds,
        streams,
        sizes,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "zcworkload-sample: profile={} seed={} capacity={} requests={} reads={} writes={} \
             bytes={} invalid_alignment={} out_of_bounds={}",
            report.profile,
            seed,
            capacity,
            requests,
            counters.reads,
            counters.writes,
            counters.bytes,
            invalid_alignment,
            out_of_bounds,
        );
        for stream in report.streams {
            println!(
                "zcworkload-sample-stream: index={} name={} area={} ops={} target_percent={:.3} \
                 observed_percent={:.3}",
                stream.index,
                stream.name,
                stream.area.as_str(),
                stream.operations,
                stream.target_percent,
                stream.observed_percent,
            );
        }
        for size in report.sizes {
            println!(
                "zcworkload-sample-size: bytes={} ops={} observed_percent={:.3}",
                size.bytes, size.operations, size.observed_percent,
            );
        }
    }
    Ok(())
}

fn parse_u64(value: &str, flag: &str) -> io::Result<u64> {
    value.replace('_', "").parse::<u64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {flag} value {value:?}: {error}"),
        )
    })
}

fn parse_usize(value: &str, flag: &str) -> io::Result<usize> {
    usize::try_from(parse_u64(value, flag)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{flag} value does not fit usize"),
        )
    })
}

fn parse_bool(value: &str, flag: &str) -> io::Result<bool> {
    match value {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{flag} expects true or false"),
        )),
    }
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> io::Result<String> {
    args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{flag} requires a value"),
        )
    })
}

fn print_help() {
    println!(
        "zcworkload - deterministic enterprise mixed block workload\n\
         \n\
         Usage:\n\
           zcworkload sample [--capacity SIZE] [--requests N] [--seed N] [--json]\n\
           zcworkload run --target DEVICE [OPTIONS]\n\
         \n\
         Run options:\n\
           --capacity SIZE                 active address space, default 1G\n\
           --engine uring|sync             default uring\n\
           --workers N                     default 8\n\
           --depth N                       per-worker depth, default 64\n\
           --ring-entries N                default 256\n\
           --duration SECONDS              default 10\n\
           --ops-per-worker N              fixed-operation smoke mode\n\
           --rate IOPS                     aggregate open-loop target; 0 saturates\n\
           --buffers auto|small-pages|hugetlb\n\
           --pin true|false                default true\n\
           --latency-sample-rate N         default 1; 0 disables\n\
           --completion-batch N            default 64\n\
           --pace-spin-us N                default 50\n\
           --lane-map MAP                  laneN:workerN:cpuN,...; required for network runs\n\
           --kthread-map MAP               kthreadN:cpuN,... client mapping\n\
           --completion local-ack|remote-ack|durable\n\
           --transport-rtt-ns N            measured RTT for low-depth efficiency\n\
           --repeats N                     repeat local runs and report min/median/max\n\
         \n\
         Writes are destructive. Synthetic block targets and exactly /dev/zcnblk0 are accepted\n\
         directly. Real media must use the repository PARTUUID allowlist and raw-write\n\
         confirmation environment variables."
    );
}

pub fn main_entry() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_string());
    match command.as_str() {
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "sample" => {
            let mut capacity = 1024 * 1024 * 1024usize;
            let mut requests = 1_000_000u64;
            let mut seed = 0x6a09_e667_f3bc_c909u64;
            let mut json = false;
            while let Some(flag) = args.next() {
                match flag.as_str() {
                    "--capacity" => {
                        capacity = parse_size_arg(&next_value(&mut args, &flag)?, &flag)?
                    }
                    "--requests" => requests = parse_u64(&next_value(&mut args, &flag)?, &flag)?,
                    "--seed" => seed = parse_u64(&next_value(&mut args, &flag)?, &flag)?,
                    "--json" => json = true,
                    "--help" | "-h" => {
                        print_help();
                        return Ok(());
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown sample option {flag:?}"),
                        ));
                    }
                }
            }
            sample_workload(capacity as u64, requests, seed, json)
        }
        "run" => {
            let mut cfg = RunConfig::default();
            while let Some(flag) = args.next() {
                match flag.as_str() {
                    "--target" => cfg.target = next_value(&mut args, &flag)?,
                    "--capacity" => {
                        cfg.capacity = parse_size_arg(&next_value(&mut args, &flag)?, &flag)?
                    }
                    "--engine" => cfg.engine = Engine::parse(&next_value(&mut args, &flag)?)?,
                    "--workers" => {
                        cfg.workers = parse_usize(&next_value(&mut args, &flag)?, &flag)?
                    }
                    "--depth" | "--iodepth" => {
                        cfg.depth = parse_usize(&next_value(&mut args, &flag)?, &flag)?
                    }
                    "--ring-entries" => {
                        cfg.ring_entries =
                            u32::try_from(parse_u64(&next_value(&mut args, &flag)?, &flag)?)
                                .map_err(|_| {
                                    io::Error::new(
                                        io::ErrorKind::InvalidInput,
                                        "ring entries does not fit u32",
                                    )
                                })?
                    }
                    "--duration" => {
                        let seconds =
                            next_value(&mut args, &flag)?
                                .parse::<f64>()
                                .map_err(|error| {
                                    io::Error::new(
                                        io::ErrorKind::InvalidInput,
                                        format!("invalid duration: {error}"),
                                    )
                                })?;
                        if !seconds.is_finite() || seconds < 0.0 {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "duration must be a finite non-negative number",
                            ));
                        }
                        cfg.duration = Duration::from_secs_f64(seconds);
                    }
                    "--ops-per-worker" => {
                        cfg.ops_per_worker = Some(parse_u64(&next_value(&mut args, &flag)?, &flag)?)
                    }
                    "--rate" | "--target-iops" => {
                        cfg.target_iops = parse_u64(&next_value(&mut args, &flag)?, &flag)?
                    }
                    "--seed" => cfg.seed = parse_u64(&next_value(&mut args, &flag)?, &flag)?,
                    "--pin" => cfg.pin_workers = parse_bool(&next_value(&mut args, &flag)?, &flag)?,
                    "--buffers" => {
                        cfg.buffer_choice = BufferChoice::parse(&next_value(&mut args, &flag)?)?
                    }
                    "--latency-sample-rate" => {
                        cfg.latency_sample_rate = parse_u64(&next_value(&mut args, &flag)?, &flag)?
                    }
                    "--completion-batch" => {
                        cfg.completion_batch = parse_usize(&next_value(&mut args, &flag)?, &flag)?
                    }
                    "--pace-spin-us" => {
                        cfg.pace_spin =
                            Duration::from_micros(parse_u64(&next_value(&mut args, &flag)?, &flag)?)
                    }
                    "--lane-map" => cfg.lane_map = Some(next_value(&mut args, &flag)?),
                    "--kthread-map" => cfg.kthread_map = Some(next_value(&mut args, &flag)?),
                    "--completion" => {
                        cfg.completion = CompletionContract::parse(&next_value(&mut args, &flag)?)?
                    }
                    "--transport-rtt-ns" => {
                        cfg.transport_rtt_ns =
                            Some(parse_u64(&next_value(&mut args, &flag)?, &flag)?)
                    }
                    "--repeats" => {
                        cfg.repeats = parse_usize(&next_value(&mut args, &flag)?, &flag)?
                    }
                    "--help" | "-h" => {
                        print_help();
                        return Ok(());
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown run option {flag:?}"),
                        ));
                    }
                }
            }
            run_workload(cfg)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown zcworkload command {command:?}; use sample or run"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_weights_are_complete() {
        assert_eq!(
            STREAMS.iter().map(|stream| stream.weight).sum::<u32>(),
            PROFILE_WEIGHT
        );
        assert_eq!(SIZE_WEIGHTS.iter().sum::<u32>(), PROFILE_WEIGHT);
    }

    #[test]
    fn layout_is_contiguous_and_has_expected_shape() {
        let layout = WorkloadLayout::new(1024 * 1024 * 1024).unwrap();
        assert_eq!(layout.data.start, 0);
        assert_eq!(layout.data.end(), layout.user.start);
        assert_eq!(layout.user.end(), layout.journal.start);
        assert_eq!(layout.journal.end(), layout.capacity);
        assert!((layout.data.len as f64 / layout.capacity as f64 - 0.45).abs() < 0.001);
        assert!((layout.user.len as f64 / layout.capacity as f64 - 0.45).abs() < 0.001);
        assert!((layout.journal.len as f64 / layout.capacity as f64 - 0.10).abs() < 0.001);
    }

    #[test]
    fn large_layout_and_reuse_windows_do_not_overflow() {
        let capacity = 64 * 1024 * 1024 * 1024 * 1024u64;
        let layout = WorkloadLayout::new(capacity).unwrap();
        assert_eq!(layout.journal.end(), capacity);
        let range = subrange(layout.data, 700_000, 750_000);
        assert!(range.start >= layout.data.start);
        assert!(range.end() <= layout.data.end());
        assert!(range.len >= MAX_TRANSFER_BYTES as u64);
    }

    #[test]
    fn generated_requests_are_deterministic_aligned_and_bounded() {
        let layout = WorkloadLayout::new(1024 * 1024 * 1024).unwrap();
        let mut left = WorkloadGenerator::new(layout, 42, 3);
        let mut right = WorkloadGenerator::new(layout, 42, 3);
        for _ in 0..100_000 {
            let request = left.next();
            assert_eq!(request, right.next());
            assert_eq!(request.offset % REQUEST_ALIGNMENT, 0);
            assert!(SIZE_CLASSES.contains(&request.len));
            let area = layout.area(request.area);
            assert!(request.offset >= area.start);
            assert!(request.offset + request.len as u64 <= area.end());
        }
    }

    #[test]
    fn sampled_stream_distribution_tracks_profile() {
        let layout = WorkloadLayout::new(1024 * 1024 * 1024).unwrap();
        let mut generator = WorkloadGenerator::new(layout, 7, 0);
        let mut counts = [0u64; STREAM_COUNT];
        let samples = 1_000_000u64;
        for _ in 0..samples {
            counts[generator.next().stream] += 1;
        }
        for (index, spec) in STREAMS.iter().enumerate() {
            let observed = counts[index] as f64 / samples as f64;
            let expected = spec.weight as f64 / PROFILE_WEIGHT as f64;
            assert!(
                (observed - expected).abs() < 0.002,
                "{} observed={observed} expected={expected}",
                spec.name
            );
        }
    }

    #[test]
    fn localized_streams_remain_in_their_windows() {
        let layout = WorkloadLayout::new(1024 * 1024 * 1024).unwrap();
        for (index, spec) in STREAMS.iter().enumerate() {
            let AddressModel::Reuse { start_ppm, end_ppm } = spec.address else {
                continue;
            };
            let range = subrange(layout.area(spec.area), start_ppm, end_ppm);
            let mut rng = 9;
            let mut state = StreamState::default();
            for _ in 0..10_000 {
                let offset = reuse_offset(&mut rng, &mut state, range, 8 * 1024);
                assert!(offset >= range.start, "stream {index}");
                assert!(offset + 8 * 1024 <= range.end(), "stream {index}");
            }
        }
    }

    #[test]
    fn modular_reuse_subtraction_wraps_within_the_window() {
        assert_eq!(modular_sub(2, 5, 11), 8);
        assert_eq!(modular_sub(8, 5, 11), 3);
    }

    #[test]
    fn sequential_mixed_transfers_are_contiguous() {
        let range = ByteRange {
            start: 1024 * 1024,
            len: 1024 * 1024,
        };
        let mut rng = 1;
        let mut state = StreamState {
            cursor: range.start + 64 * 1024,
            initialized: true,
            ..StreamState::default()
        };
        let first = sequential_offset(&mut rng, &mut state, range, 4 * 1024);
        let second = sequential_offset(&mut rng, &mut state, range, 8 * 1024);
        let third = sequential_offset(&mut rng, &mut state, range, 32 * 1024);
        assert_eq!(second, first + 4 * 1024);
        assert_eq!(third, second + 8 * 1024);
    }

    #[test]
    fn sequential_initial_positions_use_worker_seeded_rng() {
        let range = ByteRange {
            start: 0,
            len: 1024 * 1024,
        };
        let mut left_rng =
            WorkloadGenerator::new(WorkloadLayout::new(MIN_CAPACITY_BYTES).unwrap(), 42, 0).rng;
        let mut right_rng =
            WorkloadGenerator::new(WorkloadLayout::new(MIN_CAPACITY_BYTES).unwrap(), 42, 1).rng;
        let left = sequential_offset(&mut left_rng, &mut StreamState::default(), range, 8 * 1024);
        let right = sequential_offset(&mut right_rng, &mut StreamState::default(), range, 8 * 1024);
        assert_ne!(left, right);
    }

    #[test]
    fn direct_io_geometry_rejects_alignment_larger_than_smallest_transfer() {
        assert!(validate_direct_io_geometry(4096, SlotWalBufferMode::SmallPages).is_ok());
        let error = validate_direct_io_geometry(8192, SlotWalBufferMode::SmallPages).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn logical_operations_may_span_multiple_queue_fragments() {
        assert_eq!(max_queue_fragments_per_logical_op(64 * 1024), 1);
        assert_eq!(max_queue_fragments_per_logical_op(16 * 1024), 4);
        assert_eq!(max_queue_fragments_per_logical_op(4 * 1024), 16);
    }

    #[test]
    fn network_edge_is_exactly_zcnblk_zero() {
        assert!(validate_target_spec("/dev/zcnblk0").is_ok());
        assert!(validate_target_spec("/dev/zcnblk1").is_err());
        assert!(validate_target_spec("/dev/zcnblk00").is_err());
        assert!(validate_target_spec("/dev/nullb0").is_ok());
    }

    #[test]
    fn topology_maps_are_structured_and_reject_duplicates() {
        assert_eq!(
            parse_lane_map("lane0:worker0:cpu2,lane1:worker1:cpu3").unwrap(),
            vec![
                LaneAssignment {
                    lane: 0,
                    worker: 0,
                    cpu: 2,
                },
                LaneAssignment {
                    lane: 1,
                    worker: 1,
                    cpu: 3,
                },
            ]
        );
        assert!(parse_lane_map("").is_err());
        assert!(parse_lane_map("anything").is_err());
        assert!(parse_lane_map("lane0:worker0:cpu0,lane0:worker1:cpu1").is_err());
        assert!(parse_kthread_map("kthread0:cpu4,kthread0:cpu5").is_err());
    }

    #[test]
    fn topology_coverage_checks_worker_cpu_mapping() {
        let mut cfg = RunConfig::default();
        cfg.workers = 1;
        let maps = TopologyMaps {
            lanes: parse_lane_map("lane0:worker0:cpu0").unwrap(),
            kthreads: parse_kthread_map("kthread0:cpu0").unwrap(),
        };
        assert!(validate_topology_coverage(&cfg, &maps, &[0]).is_empty());
        assert!(
            validate_topology_coverage(&cfg, &maps, &[1])
                .iter()
                .any(|issue| issue.contains("planned CPU"))
        );
    }

    #[test]
    fn zero_transport_rtt_is_rejected() {
        let mut cfg = RunConfig::default();
        cfg.target = "/dev/zcnblk0".to_string();
        cfg.transport_rtt_ns = Some(0);
        assert!(validate_run_config(&cfg).is_err());
    }

    #[test]
    fn start_gate_releases_with_shared_epoch_and_cancels() {
        let gate = Arc::new(StartGate::new());
        let waiter = Arc::clone(&gate);
        let handle = thread::spawn(move || waiter.wait());
        let epoch = Instant::now();
        gate.start(epoch).unwrap();
        assert_eq!(handle.join().unwrap().unwrap(), epoch);

        let gate = Arc::new(StartGate::new());
        let waiter = Arc::clone(&gate);
        let handle = thread::spawn(move || waiter.wait());
        gate.cancel();
        assert!(handle.join().unwrap().is_err());
    }

    #[test]
    fn paced_worker_waits_for_due_time_while_a_slot_is_free() {
        assert!(wait_for_due_before_completion(false, 10_000, 1));
        assert!(!wait_for_due_before_completion(false, 10_000, 0));
        assert!(!wait_for_due_before_completion(false, 0, 1));
        assert!(!wait_for_due_before_completion(true, 10_000, 1));
    }
}
