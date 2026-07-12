use std::cell::UnsafeCell;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_CHUNK_BYTES: usize = 1024 * 1024;
const DEFAULT_CACHE_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_WORKERS: usize = 0;
const DEFAULT_BRANCHES: usize = 2;
const DEFAULT_EXTENT_BYTES: u64 = 1024 * 1024;
const DEFAULT_SLAB_SLOTS: usize = 4;

#[derive(Clone, Debug)]
struct Config {
    role: Role,
    bind: String,
    branch_binds: Vec<String>,
    peer: String,
    source_ip: Option<IpAddr>,
    base_port: u16,
    leaf_base_port: u16,
    downstream_base_port: u16,
    lanes: usize,
    branches: usize,
    bytes_per_lane: u64,
    branch_bytes_per_lane: u64,
    cache_bytes_per_lane: u64,
    chunk_bytes: usize,
    extent_bytes: u64,
    workers: usize,
    pin: bool,
    cpu_list: Vec<usize>,
    verify_pattern: bool,
    client_sink: ClientSink,
    rcvlowat_bytes: usize,
    wait_mode: WaitMode,
    cold_mode: ColdMode,
    slab_slots: usize,
    prefill_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Role {
    ClientRecv,
    FanHot,
    FanCold,
    LeafSend,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientSink {
    Read,
    WaitAll,
    SpliceNull,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitMode {
    Spin,
    Yield,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ColdMode {
    CacheThenSend,
    LaneSplice,
    LaneSpliceBlocking,
    UringZc,
    StreamTee,
    StreamDirect,
    PipeDirect,
    SlabDirect,
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
    lanes: usize,
    bytes: u64,
    aux_bytes: u64,
    waits: u64,
    elapsed: Duration,
    thread_cpu: Duration,
    phase_ingress: Duration,
    phase_egress: Duration,
    target_cpu: i32,
    affinity_applied: bool,
    start_cpu: i32,
    end_cpu: i32,
    voluntary_switches: u64,
    involuntary_switches: u64,
    migrations: u64,
    checksum: u64,
    recv_ops: u64,
    send_ops: u64,
    short_recv_ops: u64,
    short_send_ops: u64,
    max_recv_op_bytes: u64,
    max_send_op_bytes: u64,
    pipe_capacity_bytes: u64,
    zc_notifications: u64,
    zc_copied_notifications: u64,
}

#[derive(Clone)]
struct BranchProgress {
    written: Arc<AtomicU64>,
}

#[derive(Debug, Default)]
struct SlabRecvStats {
    bytes: u64,
    waits: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct UringZcStats {
    recv_ops: u64,
    send_ops: u64,
    zc_notifications: u64,
    zc_copied_notifications: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct SpliceDrainStats {
    bytes: u64,
    socket_splice_ops: u64,
    sink_splice_ops: u64,
    short_socket_splices: u64,
    short_sink_splices: u64,
    max_socket_splice_bytes: u64,
    max_sink_splice_bytes: u64,
    pipe_capacity_bytes: u64,
}

impl SpliceDrainStats {
    fn record_socket_splice(&mut self, got: usize, want: usize) {
        self.socket_splice_ops = self.socket_splice_ops.saturating_add(1);
        if got < want {
            self.short_socket_splices = self.short_socket_splices.saturating_add(1);
        }
        self.max_socket_splice_bytes = self.max_socket_splice_bytes.max(got as u64);
    }

    fn record_sink_splice(&mut self, got: usize, want: usize) {
        self.sink_splice_ops = self.sink_splice_ops.saturating_add(1);
        if got < want {
            self.short_sink_splices = self.short_sink_splices.saturating_add(1);
        }
        self.max_sink_splice_bytes = self.max_sink_splice_bytes.max(got as u64);
    }
}

impl Role {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "client-recv" | "client" | "recv" => Ok(Self::ClientRecv),
            "fan-hot" | "hot" => Ok(Self::FanHot),
            "fan-cold" | "cold" => Ok(Self::FanCold),
            "leaf-send" | "leaf" => Ok(Self::LeafSend),
            other => Err(invalid_input(format!(
                "unknown role {other:?}; use client-recv, fan-hot, fan-cold, or leaf-send"
            ))),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ClientRecv => "client-recv",
            Self::FanHot => "fan-hot",
            Self::FanCold => "fan-cold",
            Self::LeafSend => "leaf-send",
        }
    }
}

impl ClientSink {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "read" | "copy" => Ok(Self::Read),
            "waitall" | "recv-waitall" | "msg-waitall" => Ok(Self::WaitAll),
            "splice-null" | "splice" | "null" => Ok(Self::SpliceNull),
            other => Err(invalid_input(format!(
                "unknown client sink {other:?}; use read, waitall, or splice-null"
            ))),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::WaitAll => "waitall",
            Self::SpliceNull => "splice-null",
        }
    }
}

impl WaitMode {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "spin" | "busy" | "greedy" => Ok(Self::Spin),
            "yield" | "scheduler" => Ok(Self::Yield),
            other => Err(invalid_input(format!(
                "unknown wait mode {other:?}; use spin or yield"
            ))),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Spin => "spin",
            Self::Yield => "yield",
        }
    }

    fn wait_once(self) {
        match self {
            Self::Spin => std::hint::spin_loop(),
            Self::Yield => thread::yield_now(),
        }
    }
}

impl ColdMode {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "cache-then-send" | "cache" | "memfd-readback" => Ok(Self::CacheThenSend),
            "lane-splice" | "lane-local-splice" | "direct-splice" | "splice-direct" => {
                Ok(Self::LaneSplice)
            }
            "lane-splice-blocking" | "lane-local-splice-blocking" | "blocking-lane-splice" => {
                Ok(Self::LaneSpliceBlocking)
            }
            "uring-zc" | "io-uring-zc" | "uring-direct" | "uring-send-zc" => Ok(Self::UringZc),
            "stream-tee" | "tee" | "stream-cache" => Ok(Self::StreamTee),
            "stream-direct" | "direct" | "no-cache" => Ok(Self::StreamDirect),
            "pipe-direct" | "pipe" | "pipe-stream" => Ok(Self::PipeDirect),
            "slab-direct" | "slab" | "lease" | "leased-slab" => Ok(Self::SlabDirect),
            other => Err(invalid_input(format!(
                "unknown cold mode {other:?}; use cache-then-send, lane-splice, lane-splice-blocking, uring-zc, stream-tee, stream-direct, pipe-direct, or slab-direct"
            ))),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::CacheThenSend => "cache-then-send",
            Self::LaneSplice => "lane-splice",
            Self::LaneSpliceBlocking => "lane-splice-blocking",
            Self::UringZc => "uring-zc",
            Self::StreamTee => "stream-tee",
            Self::StreamDirect => "stream-direct",
            Self::PipeDirect => "pipe-direct",
            Self::SlabDirect => "slab-direct",
        }
    }

    fn uses_receiver_threads(self) -> bool {
        matches!(
            self,
            Self::CacheThenSend | Self::PipeDirect | Self::SlabDirect
        )
    }
}

pub fn cli<I>(args: I) -> io::Result<()>
where
    I: Iterator<Item = String>,
{
    let config = Config::parse(args)?;
    config.validate()?;
    print_config(&config);
    match config.role {
        Role::ClientRecv => client_recv(config),
        Role::FanHot => fan_hot(config),
        Role::FanCold => fan_cold(config),
        Role::LeafSend => leaf_send(config),
    }
}

impl Config {
    fn parse<I>(mut args: I) -> io::Result<Self>
    where
        I: Iterator<Item = String>,
    {
        let Some(role) = args.next() else {
            print_usage();
            return Err(invalid_input("missing role"));
        };
        if role == "-h" || role == "--help" || role == "help" {
            print_usage();
            std::process::exit(0);
        }
        let mut config = Self {
            role: Role::parse(&role)?,
            bind: "0.0.0.0".to_string(),
            branch_binds: env::var("URING_PLAY_ZCFAN_READCACHE_BRANCH_BINDS")
                .ok()
                .map(|value| parse_csv_strings(&value))
                .unwrap_or_default(),
            peer: "127.0.0.1".to_string(),
            source_ip: None,
            base_port: 33000,
            leaf_base_port: 34000,
            downstream_base_port: 33000,
            lanes: 16,
            branches: DEFAULT_BRANCHES,
            bytes_per_lane: 1024 * 1024 * 1024,
            branch_bytes_per_lane: 512 * 1024 * 1024,
            cache_bytes_per_lane: DEFAULT_CACHE_BYTES,
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            extent_bytes: DEFAULT_EXTENT_BYTES,
            workers: DEFAULT_WORKERS,
            pin: env_truthy("URING_PLAY_PIN_CPUS"),
            cpu_list: parse_cpu_list_env(),
            verify_pattern: env_truthy("URING_PLAY_ZCFAN_READCACHE_VERIFY"),
            client_sink: env::var("URING_PLAY_ZCFAN_READCACHE_CLIENT_SINK")
                .ok()
                .map(|value| ClientSink::parse(&value))
                .transpose()?
                .unwrap_or(ClientSink::Read),
            rcvlowat_bytes: env::var("URING_PLAY_ZCFAN_READCACHE_RCVLOWAT_BYTES")
                .ok()
                .map(|value| parse_count(&value))
                .transpose()?
                .unwrap_or(0),
            wait_mode: env::var("URING_PLAY_ZCFAN_READCACHE_WAIT_MODE")
                .ok()
                .map(|value| WaitMode::parse(&value))
                .transpose()?
                .unwrap_or(WaitMode::Spin),
            cold_mode: env::var("URING_PLAY_ZCFAN_READCACHE_COLD_MODE")
                .ok()
                .map(|value| ColdMode::parse(&value))
                .transpose()?
                .unwrap_or(ColdMode::StreamTee),
            slab_slots: env::var("URING_PLAY_ZCFAN_READCACHE_SLAB_SLOTS")
                .ok()
                .map(|value| parse_count(&value))
                .transpose()?
                .unwrap_or(DEFAULT_SLAB_SLOTS),
            prefill_bytes: env::var("URING_PLAY_ZCFAN_READCACHE_PREFILL_BYTES")
                .ok()
                .map(|value| parse_count_u64(&value))
                .transpose()?
                .unwrap_or(0),
        };

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_usage();
                    std::process::exit(0);
                }
                "--bind" => config.bind = next_arg(&mut args, "--bind")?,
                "--branch-binds" => {
                    config.branch_binds = parse_csv_strings(&next_arg(&mut args, "--branch-binds")?)
                }
                "--peer" | "--addr" => config.peer = next_arg(&mut args, "--peer")?,
                "--source-ip" | "--source-addr" => {
                    config.source_ip = Some(parse_ip(&next_arg(&mut args, "--source-ip")?)?)
                }
                "--base-port" => {
                    config.base_port = parse_u16(&next_arg(&mut args, "--base-port")?)?
                }
                "--leaf-base-port" => {
                    config.leaf_base_port = parse_u16(&next_arg(&mut args, "--leaf-base-port")?)?
                }
                "--downstream-base-port" => {
                    config.downstream_base_port =
                        parse_u16(&next_arg(&mut args, "--downstream-base-port")?)?
                }
                "--lanes" => config.lanes = parse_count(&next_arg(&mut args, "--lanes")?)?,
                "--branches" => config.branches = parse_count(&next_arg(&mut args, "--branches")?)?,
                "--bytes-per-lane" => {
                    config.bytes_per_lane =
                        parse_count_u64(&next_arg(&mut args, "--bytes-per-lane")?)?
                }
                "--branch-bytes-per-lane" => {
                    config.branch_bytes_per_lane =
                        parse_count_u64(&next_arg(&mut args, "--branch-bytes-per-lane")?)?
                }
                "--cache-bytes-per-lane" => {
                    config.cache_bytes_per_lane =
                        parse_count_u64(&next_arg(&mut args, "--cache-bytes-per-lane")?)?
                }
                "--chunk-bytes" => {
                    config.chunk_bytes = parse_count(&next_arg(&mut args, "--chunk-bytes")?)?
                }
                "--extent-bytes" => {
                    config.extent_bytes = parse_count_u64(&next_arg(&mut args, "--extent-bytes")?)?
                }
                "--workers" => config.workers = parse_count(&next_arg(&mut args, "--workers")?)?,
                "--pin" => config.pin = true,
                "--no-pin" => config.pin = false,
                "--cpu-list" => {
                    config.cpu_list = parse_cpu_list(&next_arg(&mut args, "--cpu-list")?)?
                }
                "--verify" => config.verify_pattern = true,
                "--no-verify" => config.verify_pattern = false,
                "--client-sink" => {
                    config.client_sink = ClientSink::parse(&next_arg(&mut args, "--client-sink")?)?
                }
                "--rcvlowat-bytes" | "--recv-lowat-bytes" => {
                    config.rcvlowat_bytes = parse_count(&next_arg(&mut args, "--rcvlowat-bytes")?)?
                }
                "--wait-mode" => {
                    config.wait_mode = WaitMode::parse(&next_arg(&mut args, "--wait-mode")?)?
                }
                "--cold-mode" => {
                    config.cold_mode = ColdMode::parse(&next_arg(&mut args, "--cold-mode")?)?
                }
                "--slab-slots" => {
                    config.slab_slots = parse_count(&next_arg(&mut args, "--slab-slots")?)?
                }
                "--prefill-bytes" => {
                    config.prefill_bytes =
                        parse_count_u64(&next_arg(&mut args, "--prefill-bytes")?)?
                }
                other => return Err(invalid_input(format!("unknown argument {other:?}"))),
            }
        }

        if config.workers == 0 {
            config.workers = config.lanes.max(1);
        }
        Ok(config)
    }

    fn validate(&self) -> io::Result<()> {
        if self.lanes == 0 {
            return Err(invalid_input("--lanes must be nonzero"));
        }
        if self.branches == 0 {
            return Err(invalid_input("--branches must be nonzero"));
        }
        if self.workers == 0 {
            return Err(invalid_input("--workers must be nonzero"));
        }
        if self.bytes_per_lane == 0 {
            return Err(invalid_input("--bytes-per-lane must be nonzero"));
        }
        if self.branch_bytes_per_lane == 0 {
            return Err(invalid_input("--branch-bytes-per-lane must be nonzero"));
        }
        if self.cache_bytes_per_lane == 0 {
            return Err(invalid_input("--cache-bytes-per-lane must be nonzero"));
        }
        if self.chunk_bytes == 0 {
            return Err(invalid_input("--chunk-bytes must be nonzero"));
        }
        if self.extent_bytes == 0 {
            return Err(invalid_input("--extent-bytes must be nonzero"));
        }
        if self.rcvlowat_bytes > i32::MAX as usize {
            return Err(invalid_input(
                "--rcvlowat-bytes exceeds setsockopt i32 range",
            ));
        }
        if self.slab_slots == 0 {
            return Err(invalid_input("--slab-slots must be nonzero"));
        }
        if self.role == Role::FanCold && self.branches != 2 {
            return Err(invalid_input(
                "fan-cold currently models the two-leaf stripe target; set --branches 2",
            ));
        }
        if !self.branch_binds.is_empty() && self.branch_binds.len() != self.branches {
            return Err(invalid_input(
                "--branch-binds must provide exactly one bind address per branch",
            ));
        }
        if self.role == Role::FanCold
            && self.bytes_per_lane
                != self
                    .branch_bytes_per_lane
                    .saturating_mul(self.branches as u64)
        {
            return Err(invalid_input(
                "fan-cold expects downstream bytes-per-lane == branches * branch-bytes-per-lane",
            ));
        }
        if self.pin && self.cpu_list.is_empty() {
            eprintln!(
                "PERF WARNING: zcfan-readcache-bench pinning requested without --cpu-list/URING_PLAY_PIN_CPU_LIST; implicit CPU mapping is not representative"
            );
        }
        if self.verify_pattern && self.client_sink == ClientSink::SpliceNull {
            return Err(invalid_input(
                "--verify cannot be combined with --client-sink splice-null because payload is never materialized in userspace",
            ));
        }
        if self.role == Role::ClientRecv
            && self.client_sink == ClientSink::SpliceNull
            && self.rcvlowat_bytes == 0
        {
            eprintln!(
                "PERF WARNING: client-recv splice-null has SO_RCVLOWAT disabled; bulk socket->pipe splice commonly wakes on short fragments and can create thousands of voluntary context switches. Set --rcvlowat-bytes near 256K..chunk-bytes for bulk-drain measurements, and leave it disabled for low-latency small-QD tests."
            );
        }
        if self.role == Role::FanCold
            && matches!(
                self.cold_mode,
                ColdMode::CacheThenSend
                    | ColdMode::LaneSplice
                    | ColdMode::LaneSpliceBlocking
                    | ColdMode::StreamTee
                    | ColdMode::StreamDirect
                    | ColdMode::PipeDirect
            )
            && self.rcvlowat_bytes == 0
        {
            eprintln!(
                "PERF WARNING: fan-cold leaf receive has SO_RCVLOWAT disabled; cold read/zip tests may be dominated by short socket->pipe receives instead of WAL zipper or cache behavior. Use --rcvlowat-bytes for bulk cold-cache measurements."
            );
        }
        if self.role == Role::FanCold
            && self.cold_mode.uses_receiver_threads()
            && self.pin
            && !self.cpu_list.is_empty()
            && self.cpu_list.len() < self.workers.saturating_mul(self.branches + 1)
        {
            eprintln!(
                "PERF WARNING: fan-cold has workers={} branches={} but cpu_list_len={}; receiver threads cannot all get dedicated CPUs",
                self.workers,
                self.branches,
                self.cpu_list.len()
            );
        }
        Ok(())
    }
}

fn client_recv(config: Config) -> io::Result<()> {
    let listeners = bind_lane_listeners(&config.bind, config.base_port, config.lanes)?;
    let mut streams = Vec::with_capacity(config.lanes);
    for (lane, listener) in listeners.into_iter().enumerate() {
        let (stream, peer) = listener.accept()?;
        set_stream_options(&stream)?;
        set_socket_rcvlowat(stream.as_raw_fd(), config.rcvlowat_bytes, "client-recv")?;
        crate::zc_maybe_warn_route_alignment(
            "zcfan-readcache-client-accept",
            peer,
            stream.local_addr().ok(),
        )?;
        println!(
            "zcfan-readcache-accept: role=client-recv lane={lane} peer={peer} local={}",
            stream.local_addr()?
        );
        streams.push((lane, stream));
    }
    let shards = shard_items(streams, config.workers);
    let barrier = Arc::new(Barrier::new(active_shards(&shards) + 1));
    let mut handles = Vec::new();
    for (worker, shard) in shards.into_iter().enumerate() {
        if shard.is_empty() {
            continue;
        }
        let config = config.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            recv_worker(worker, shard, config, barrier)
        }));
    }
    barrier.wait();
    collect_stats("zcfan-readcache-client-recv", handles)
}

fn fan_hot(config: Config) -> io::Result<()> {
    let streams = connect_lanes(
        &config.peer,
        config.base_port,
        config.lanes,
        config.source_ip,
    )?;
    let shards = shard_items(streams, config.workers);
    let barrier = Arc::new(Barrier::new(active_shards(&shards) + 1));
    let mut handles = Vec::new();
    for (worker, shard) in shards.into_iter().enumerate() {
        if shard.is_empty() {
            continue;
        }
        let config = config.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            hot_send_worker(worker, shard, config, barrier)
        }));
    }
    barrier.wait();
    collect_stats("zcfan-readcache-fan-hot", handles)
}

fn leaf_send(config: Config) -> io::Result<()> {
    let streams = connect_lanes(
        &config.peer,
        config.base_port,
        config.lanes,
        config.source_ip,
    )?;
    let shards = shard_items(streams, config.workers);
    let barrier = Arc::new(Barrier::new(active_shards(&shards) + 1));
    let mut handles = Vec::new();
    for (worker, shard) in shards.into_iter().enumerate() {
        if shard.is_empty() {
            continue;
        }
        let config = config.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            leaf_send_worker(worker, shard, config, barrier)
        }));
    }
    barrier.wait();
    collect_stats("zcfan-readcache-leaf-send", handles)
}

fn fan_cold(config: Config) -> io::Result<()> {
    let branch_listeners = (0..config.branches)
        .map(|branch| {
            let base_port =
                checked_port(config.leaf_base_port, branch.saturating_mul(config.lanes))?;
            bind_lane_listeners(branch_bind_for(&config, branch), base_port, config.lanes)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut branch_streams = (0..config.branches)
        .map(|_| Vec::<(usize, TcpStream)>::new())
        .collect::<Vec<_>>();
    for (branch, (branch_vec, listeners)) in branch_streams
        .iter_mut()
        .zip(branch_listeners.into_iter())
        .enumerate()
    {
        for (lane, listener) in listeners.into_iter().enumerate() {
            let (stream, peer) = listener.accept()?;
            set_stream_options(&stream)?;
            set_socket_rcvlowat(stream.as_raw_fd(), config.rcvlowat_bytes, "fan-cold-leaf")?;
            crate::zc_maybe_warn_route_alignment(
                "zcfan-readcache-cold-accept",
                peer,
                stream.local_addr().ok(),
            )?;
            println!(
                "zcfan-readcache-accept: role=fan-cold branch={branch} lane={lane} peer={peer} local={}",
                stream.local_addr()?
            );
            branch_vec.push((lane, stream));
        }
    }

    let downstream = connect_lanes(
        &config.peer,
        config.downstream_base_port,
        config.lanes,
        config.source_ip,
    )?;
    let mut lanes = Vec::with_capacity(config.lanes);
    for lane in 0..config.lanes {
        let branch0 = take_lane_stream(&mut branch_streams[0], lane)?;
        let branch1 = take_lane_stream(&mut branch_streams[1], lane)?;
        let downstream = downstream
            .iter()
            .find(|(stream_lane, _)| *stream_lane == lane)
            .ok_or_else(|| invalid_input(format!("missing downstream lane {lane}")))?
            .1
            .try_clone()?;
        lanes.push((lane, branch0, branch1, downstream));
    }

    let shards = shard_items(lanes, config.workers);
    let barrier = Arc::new(Barrier::new(active_shards(&shards) + 1));
    let mut handles = Vec::new();
    for (worker, shard) in shards.into_iter().enumerate() {
        if shard.is_empty() {
            continue;
        }
        let config = config.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            cold_fan_worker(worker, shard, config, barrier)
        }));
    }
    barrier.wait();
    collect_stats("zcfan-readcache-fan-cold", handles)
}

fn recv_worker(
    worker: usize,
    mut streams: Vec<(usize, TcpStream)>,
    config: Config,
    barrier: Arc<Barrier>,
) -> io::Result<WorkerStats> {
    let affinity = maybe_pin_worker(&config, worker);
    let tid = current_tid();
    let mut buf = vec![0u8; config.chunk_bytes];
    let null_sink = if config.client_sink == ClientSink::SpliceNull {
        Some(OpenOptions::new().write(true).open("/dev/null")?)
    } else {
        None
    };
    barrier.wait();
    let start_switches = read_thread_context_switches(tid).unwrap_or_default();
    let start_thread_cpu = thread_cpu_time().unwrap_or_default();
    let start_cpu = current_cpu();
    let started = Instant::now();
    let mut bytes = 0u64;
    let mut checksum = 0u64;
    let mut recv_stats = SpliceDrainStats::default();
    for (_lane, stream) in streams.iter_mut() {
        if config.client_sink == ClientSink::SpliceNull {
            let null_sink = null_sink
                .as_ref()
                .expect("splice-null sink opened before timing");
            let splice_stats = splice_socket_to_fd(
                stream.as_raw_fd(),
                null_sink.as_raw_fd(),
                config.bytes_per_lane,
                config.chunk_bytes,
            )?;
            bytes = bytes.saturating_add(splice_stats.bytes);
            recv_stats.socket_splice_ops = recv_stats
                .socket_splice_ops
                .saturating_add(splice_stats.socket_splice_ops);
            recv_stats.sink_splice_ops = recv_stats
                .sink_splice_ops
                .saturating_add(splice_stats.sink_splice_ops);
            recv_stats.short_socket_splices = recv_stats
                .short_socket_splices
                .saturating_add(splice_stats.short_socket_splices);
            recv_stats.short_sink_splices = recv_stats
                .short_sink_splices
                .saturating_add(splice_stats.short_sink_splices);
            recv_stats.max_socket_splice_bytes = recv_stats
                .max_socket_splice_bytes
                .max(splice_stats.max_socket_splice_bytes);
            recv_stats.max_sink_splice_bytes = recv_stats
                .max_sink_splice_bytes
                .max(splice_stats.max_sink_splice_bytes);
            recv_stats.pipe_capacity_bytes = recv_stats
                .pipe_capacity_bytes
                .max(splice_stats.pipe_capacity_bytes);
            continue;
        }

        let mut remaining = config.bytes_per_lane;
        while remaining != 0 {
            let take = (remaining as usize).min(buf.len());
            let received = match config.client_sink {
                ClientSink::Read => read_exact_counted(stream, &mut buf[..take], &mut recv_stats)?,
                ClientSink::WaitAll => {
                    recv_waitall_counted(stream.as_raw_fd(), &mut buf[..take], &mut recv_stats)?
                }
                ClientSink::SpliceNull => unreachable!("handled before chunked receive loop"),
            };
            if config.verify_pattern {
                checksum = checksum.wrapping_add(sample_checksum(&buf[..received]));
            }
            bytes = bytes.saturating_add(received as u64);
            remaining -= received as u64;
        }
    }
    let mut stats = finish_stats(
        worker,
        streams.len(),
        bytes,
        0,
        0,
        checksum,
        affinity,
        start_cpu,
        start_switches,
        start_thread_cpu,
        started,
    );
    stats.recv_ops = recv_stats.socket_splice_ops;
    stats.send_ops = recv_stats.sink_splice_ops;
    stats.short_recv_ops = recv_stats.short_socket_splices;
    stats.short_send_ops = recv_stats.short_sink_splices;
    stats.max_recv_op_bytes = recv_stats.max_socket_splice_bytes;
    stats.max_send_op_bytes = recv_stats.max_sink_splice_bytes;
    stats.pipe_capacity_bytes = recv_stats.pipe_capacity_bytes;
    Ok(stats)
}

fn hot_send_worker(
    worker: usize,
    streams: Vec<(usize, TcpStream)>,
    config: Config,
    barrier: Arc<Barrier>,
) -> io::Result<WorkerStats> {
    let affinity = maybe_pin_worker(&config, worker);
    let tid = current_tid();
    let cache = create_cache_file(
        &format!("zcfan-hot-worker-{worker}"),
        config.cache_bytes_per_lane,
        config.chunk_bytes,
        worker as u8,
    )?;
    barrier.wait();
    let start_switches = read_thread_context_switches(tid).unwrap_or_default();
    let start_thread_cpu = thread_cpu_time().unwrap_or_default();
    let start_cpu = current_cpu();
    let started = Instant::now();
    let mut bytes = 0u64;
    for (_lane, stream) in &streams {
        bytes = bytes.saturating_add(sendfile_wrapped(
            cache.as_raw_fd(),
            stream.as_raw_fd(),
            config.cache_bytes_per_lane,
            config.bytes_per_lane,
            config.chunk_bytes,
        )?);
    }
    Ok(finish_stats(
        worker,
        streams.len(),
        bytes,
        0,
        0,
        0,
        affinity,
        start_cpu,
        start_switches,
        start_thread_cpu,
        started,
    ))
}

fn leaf_send_worker(
    worker: usize,
    streams: Vec<(usize, TcpStream)>,
    config: Config,
    barrier: Arc<Barrier>,
) -> io::Result<WorkerStats> {
    let affinity = maybe_pin_worker(&config, worker);
    let tid = current_tid();
    let cache = create_cache_file(
        &format!("zcfan-leaf-worker-{worker}"),
        config.cache_bytes_per_lane.min(config.bytes_per_lane),
        config.chunk_bytes,
        (worker as u8).wrapping_add(0x51),
    )?;
    barrier.wait();
    let start_switches = read_thread_context_switches(tid).unwrap_or_default();
    let start_thread_cpu = thread_cpu_time().unwrap_or_default();
    let start_cpu = current_cpu();
    let started = Instant::now();
    let mut bytes = 0u64;
    for (_lane, stream) in &streams {
        bytes = bytes.saturating_add(sendfile_wrapped(
            cache.as_raw_fd(),
            stream.as_raw_fd(),
            config.cache_bytes_per_lane.min(config.bytes_per_lane),
            config.bytes_per_lane,
            config.chunk_bytes,
        )?);
    }
    Ok(finish_stats(
        worker,
        streams.len(),
        bytes,
        0,
        0,
        0,
        affinity,
        start_cpu,
        start_switches,
        start_thread_cpu,
        started,
    ))
}

fn cold_fan_worker(
    worker: usize,
    lanes: Vec<(usize, TcpStream, TcpStream, TcpStream)>,
    config: Config,
    barrier: Arc<Barrier>,
) -> io::Result<WorkerStats> {
    let affinity = maybe_pin_worker(&config, worker);
    let tid = current_tid();
    barrier.wait();
    let start_switches = read_thread_context_switches(tid).unwrap_or_default();
    let start_thread_cpu = thread_cpu_time().unwrap_or_default();
    let start_cpu = current_cpu();
    let started = Instant::now();
    let mut downstream_bytes = 0u64;
    let mut leaf_bytes = 0u64;
    let mut waits = 0u64;
    let mut splice_stats = SpliceDrainStats::default();
    let lane_count = lanes.len();

    if config.cold_mode == ColdMode::UringZc {
        let extent_bytes = usize::try_from(config.extent_bytes)
            .map_err(|_| invalid_input("--extent-bytes exceeds usize"))?;
        let slot_count = uring_zc_slot_count(&config)?;
        let memlock_need = (extent_bytes as u64)
            .saturating_mul((slot_count as u64).saturating_mul(2))
            .saturating_mul(config.workers as u64);
        if let Some(limit) = current_memlock_limit_bytes()? {
            if limit < memlock_need {
                eprintln!(
                    "PERF WARNING: zcfan-readcache uring-zc needs about {} bytes of registered-buffer memlock for workers={} extent_bytes={} fixed_pair_slots_per_worker={} fixed_buffers_per_slot=2, but RLIMIT_MEMLOCK soft limit is {} bytes; use prlimit/ulimit/systemd limits before treating this run as representative",
                    memlock_need, config.workers, extent_bytes, slot_count, limit
                );
            }
        }
        let min_ring_entries = slot_count
            .saturating_mul(4)
            .saturating_add(32)
            .min(u32::MAX as usize) as u32;
        let ring_entries = uring_zc_ring_entries().max(min_ring_entries);
        let mut ring = crate::RawRing::new(ring_entries, ring_entries.saturating_mul(2))?;
        ring.register_napi_from_env(&format!("zcfan-readcache-uring-zc-worker-{worker}"))?;
        let fixed_buffer_count = slot_count
            .checked_mul(2)
            .ok_or_else(|| invalid_input("uring-zc fixed buffer count overflow"))?;
        let fixed_buffers =
            crate::FixedSendBuffers::new(fixed_buffer_count, extent_bytes).map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!(
                        "uring-zc failed to allocate {fixed_buffer_count} worker-local registered buffers of {extent_bytes} bytes: {err}"
                    ),
                )
            })?;
        let mut iovecs = fixed_buffers.iovecs(extent_bytes);
        ring.register_buffers(&mut iovecs).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "uring-zc failed to register fixed buffers; raise memlock/headroom before treating this path as representative: {err}"
                ),
            )
        })?;
        println!(
            "zcfan-readcache-uring-zc-worker-setup: worker={worker} fixed_pair_slots={slot_count} fixed_buffers={fixed_buffer_count} extent_bytes={extent_bytes} ring_entries={ring_entries} memory_policy={}",
            fixed_buffers.memory_policy()
        );
        let mut uring_stats = UringZcStats::default();

        for (lane, branch0, branch1, downstream) in lanes {
            let (moved, read) = uring_zc_stream_batched_lane(
                &mut ring,
                [branch0.as_raw_fd(), branch1.as_raw_fd()],
                downstream.as_raw_fd(),
                &fixed_buffers,
                slot_count,
                extent_bytes,
                config.branch_bytes_per_lane,
                &mut uring_stats,
            )?;
            downstream_bytes = downstream_bytes.saturating_add(moved);
            leaf_bytes = leaf_bytes.saturating_add(read);
            let _ = lane;
        }
        ring.unregister_buffers()?;

        let mut stats = finish_stats(
            worker,
            lane_count,
            downstream_bytes,
            leaf_bytes,
            waits,
            0,
            affinity,
            start_cpu,
            start_switches,
            start_thread_cpu,
            started,
        );
        stats.recv_ops = uring_stats.recv_ops;
        stats.send_ops = uring_stats.send_ops;
        stats.zc_notifications = uring_stats.zc_notifications;
        stats.zc_copied_notifications = uring_stats.zc_copied_notifications;
        if stats.zc_copied_notifications > 0 {
            let message = format!(
                "zcfan-readcache uring-zc copied {}/{} SEND_ZC notifications; this is not a representative zero-copy fan result",
                stats.zc_copied_notifications, stats.zc_notifications
            );
            if env_truthy("URING_PLAY_ZCFAN_REQUIRE_TRUE_ZC") {
                return Err(io::Error::other(message));
            }
            eprintln!("PERF WARNING: {message}");
        }
        return Ok(stats);
    }

    if config.cold_mode == ColdMode::SlabDirect {
        let extent_bytes = usize::try_from(config.extent_bytes)
            .map_err(|_| invalid_input("--extent-bytes exceeds usize"))?;
        for (lane, branch0, branch1, downstream) in lanes {
            let ring0 = Arc::new(SlabRing::new(config.slab_slots, extent_bytes)?);
            let ring1 = Arc::new(SlabRing::new(config.slab_slots, extent_bytes)?);
            let recv0 = spawn_slab_receiver(
                branch0,
                Arc::clone(&ring0),
                config.branch_bytes_per_lane,
                extent_bytes,
                receiver_cpu_for_worker(&config, worker, 0),
                config.wait_mode,
            );
            let recv1 = spawn_slab_receiver(
                branch1,
                Arc::clone(&ring1),
                config.branch_bytes_per_lane,
                extent_bytes,
                receiver_cpu_for_worker(&config, worker, 1),
                config.wait_mode,
            );

            let rings = [&ring0, &ring1];
            let mut branch_offsets = [0u64, 0u64];
            let mut branch_sequences = [0u64, 0u64];
            while branch_offsets[0] < config.branch_bytes_per_lane
                || branch_offsets[1] < config.branch_bytes_per_lane
            {
                for branch in 0..2 {
                    if branch_offsets[branch] >= config.branch_bytes_per_lane {
                        continue;
                    }
                    let moved = rings[branch].drain_to_socket(
                        branch_sequences[branch],
                        downstream.as_raw_fd(),
                        config.wait_mode,
                        &mut waits,
                    )?;
                    branch_offsets[branch] = branch_offsets[branch].saturating_add(moved as u64);
                    branch_sequences[branch] = branch_sequences[branch].saturating_add(1);
                    downstream_bytes = downstream_bytes.saturating_add(moved as u64);
                    leaf_bytes = leaf_bytes.saturating_add(moved as u64);
                }
            }

            let recv0_stats = recv0
                .join()
                .map_err(|_| io::Error::other("slab branch0 receiver panicked"))??;
            let recv1_stats = recv1
                .join()
                .map_err(|_| io::Error::other("slab branch1 receiver panicked"))??;
            if recv0_stats.bytes != config.branch_bytes_per_lane
                || recv1_stats.bytes != config.branch_bytes_per_lane
            {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "slab receiver byte count did not match branch size",
                ));
            }
            waits = waits.saturating_add(recv0_stats.waits);
            waits = waits.saturating_add(recv1_stats.waits);
            let _ = lane;
        }
        return Ok(finish_stats(
            worker,
            lane_count,
            downstream_bytes,
            leaf_bytes,
            waits,
            0,
            affinity,
            start_cpu,
            start_switches,
            start_thread_cpu,
            started,
        ));
    }

    if config.cold_mode == ColdMode::PipeDirect {
        for (lane, branch0, branch1, downstream) in lanes {
            let pipe0 = Pipe::new(config.chunk_bytes)?;
            let pipe1 = Pipe::new(config.chunk_bytes)?;
            let (pipe0_read, pipe0_write) = pipe0.into_split();
            let (pipe1_read, pipe1_write) = pipe1.into_split();
            let recv0 = spawn_pipe_receiver(
                branch0,
                pipe0_write,
                config.branch_bytes_per_lane,
                config.chunk_bytes,
                receiver_cpu_for_worker(&config, worker, 0),
            );
            let recv1 = spawn_pipe_receiver(
                branch1,
                pipe1_write,
                config.branch_bytes_per_lane,
                config.chunk_bytes,
                receiver_cpu_for_worker(&config, worker, 1),
            );

            let mut branch_offsets = [0u64, 0u64];
            while branch_offsets[0] < config.branch_bytes_per_lane
                || branch_offsets[1] < config.branch_bytes_per_lane
            {
                let branches = [pipe0_read.as_raw_fd(), pipe1_read.as_raw_fd()];
                for (branch, pipe_fd) in branches.into_iter().enumerate() {
                    if branch_offsets[branch] >= config.branch_bytes_per_lane {
                        continue;
                    }
                    let want = config
                        .extent_bytes
                        .min(config.branch_bytes_per_lane - branch_offsets[branch])
                        as usize;
                    drain_pipe_to_fd(pipe_fd, downstream.as_raw_fd(), None, want)?;
                    branch_offsets[branch] = branch_offsets[branch].saturating_add(want as u64);
                    downstream_bytes = downstream_bytes.saturating_add(want as u64);
                    leaf_bytes = leaf_bytes.saturating_add(want as u64);
                }
            }

            let recv0_bytes = recv0
                .join()
                .map_err(|_| io::Error::other("pipe branch0 receiver panicked"))??;
            let recv1_bytes = recv1
                .join()
                .map_err(|_| io::Error::other("pipe branch1 receiver panicked"))??;
            if recv0_bytes != config.branch_bytes_per_lane
                || recv1_bytes != config.branch_bytes_per_lane
            {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "pipe receiver byte count did not match branch size",
                ));
            }
            let _ = lane;
        }
        return Ok(finish_stats(
            worker,
            lane_count,
            downstream_bytes,
            leaf_bytes,
            waits,
            0,
            affinity,
            start_cpu,
            start_switches,
            start_thread_cpu,
            started,
        ));
    }

    if matches!(
        config.cold_mode,
        ColdMode::LaneSplice | ColdMode::LaneSpliceBlocking
    ) {
        for (lane, branch0, branch1, downstream) in lanes {
            let nonblocking = config.cold_mode == ColdMode::LaneSplice;
            if nonblocking {
                set_fd_nonblocking(branch0.as_raw_fd(), true)?;
                set_fd_nonblocking(branch1.as_raw_fd(), true)?;
                set_fd_nonblocking(downstream.as_raw_fd(), true)?;
            }
            let pipe0 = Pipe::new(config.chunk_bytes)?;
            let pipe1 = Pipe::new(config.chunk_bytes)?;
            let mut branch_offsets = [0u64, 0u64];
            while branch_offsets[0] < config.branch_bytes_per_lane
                || branch_offsets[1] < config.branch_bytes_per_lane
            {
                let branches = [(branch0.as_raw_fd(), &pipe0), (branch1.as_raw_fd(), &pipe1)];
                for (branch, (stream_fd, pipe)) in branches.into_iter().enumerate() {
                    if branch_offsets[branch] >= config.branch_bytes_per_lane {
                        continue;
                    }
                    let want = config
                        .extent_bytes
                        .min(config.branch_bytes_per_lane - branch_offsets[branch]);
                    let moved = stream_leaf_extent_with_pipe(
                        stream_fd,
                        downstream.as_raw_fd(),
                        pipe,
                        want,
                        config.chunk_bytes,
                        config.wait_mode,
                        &mut waits,
                        &mut splice_stats,
                        nonblocking,
                    )?;
                    branch_offsets[branch] = branch_offsets[branch].saturating_add(moved);
                    downstream_bytes = downstream_bytes.saturating_add(moved);
                    leaf_bytes = leaf_bytes.saturating_add(moved);
                }
            }
            let _ = lane;
        }
        let mut stats = finish_stats(
            worker,
            lane_count,
            downstream_bytes,
            leaf_bytes,
            waits,
            0,
            affinity,
            start_cpu,
            start_switches,
            start_thread_cpu,
            started,
        );
        stats.recv_ops = splice_stats.socket_splice_ops;
        stats.send_ops = splice_stats.sink_splice_ops;
        stats.short_recv_ops = splice_stats.short_socket_splices;
        stats.short_send_ops = splice_stats.short_sink_splices;
        stats.max_recv_op_bytes = splice_stats.max_socket_splice_bytes;
        stats.max_send_op_bytes = splice_stats.max_sink_splice_bytes;
        stats.pipe_capacity_bytes = splice_stats.pipe_capacity_bytes;
        return Ok(stats);
    }

    if config.cold_mode != ColdMode::CacheThenSend {
        for (lane, branch0, branch1, downstream) in lanes {
            let file0 = if config.cold_mode == ColdMode::StreamTee {
                Some(create_sparse_memfd(
                    &format!("zcfan-cold-stream-l{lane}-b0"),
                    config.branch_bytes_per_lane,
                )?)
            } else {
                None
            };
            let file1 = if config.cold_mode == ColdMode::StreamTee {
                Some(create_sparse_memfd(
                    &format!("zcfan-cold-stream-l{lane}-b1"),
                    config.branch_bytes_per_lane,
                )?)
            } else {
                None
            };
            let pipe0 = Pipe::new(config.chunk_bytes)?;
            let pipe1 = Pipe::new(config.chunk_bytes)?;
            let cache_pipe0 = if config.cold_mode == ColdMode::StreamTee {
                Some(Pipe::new(config.chunk_bytes)?)
            } else {
                None
            };
            let cache_pipe1 = if config.cold_mode == ColdMode::StreamTee {
                Some(Pipe::new(config.chunk_bytes)?)
            } else {
                None
            };
            let mut branch_offsets = [0u64, 0u64];
            while branch_offsets[0] < config.branch_bytes_per_lane
                || branch_offsets[1] < config.branch_bytes_per_lane
            {
                for branch in 0..2 {
                    if branch_offsets[branch] >= config.branch_bytes_per_lane {
                        continue;
                    }
                    let (stream_fd, cache_fd, pipe, cache_pipe) = if branch == 0 {
                        (
                            branch0.as_raw_fd(),
                            file0.as_ref().map(|file| file.as_raw_fd()),
                            &pipe0,
                            cache_pipe0.as_ref(),
                        )
                    } else {
                        (
                            branch1.as_raw_fd(),
                            file1.as_ref().map(|file| file.as_raw_fd()),
                            &pipe1,
                            cache_pipe1.as_ref(),
                        )
                    };
                    let want = config
                        .extent_bytes
                        .min(config.branch_bytes_per_lane - branch_offsets[branch]);
                    let moved = stream_leaf_extent_with_reused_pipes(
                        stream_fd,
                        downstream.as_raw_fd(),
                        cache_fd,
                        branch_offsets[branch],
                        want,
                        config.chunk_bytes,
                        pipe,
                        cache_pipe,
                        &mut splice_stats,
                    )?;
                    branch_offsets[branch] = branch_offsets[branch].saturating_add(moved);
                    downstream_bytes = downstream_bytes.saturating_add(moved);
                    leaf_bytes = leaf_bytes.saturating_add(moved);
                }
            }
        }
        let mut stats = finish_stats(
            worker,
            lane_count,
            downstream_bytes,
            leaf_bytes,
            waits,
            0,
            affinity,
            start_cpu,
            start_switches,
            start_thread_cpu,
            started,
        );
        stats.recv_ops = splice_stats.socket_splice_ops;
        stats.send_ops = splice_stats.sink_splice_ops;
        stats.short_recv_ops = splice_stats.short_socket_splices;
        stats.short_send_ops = splice_stats.short_sink_splices;
        stats.max_recv_op_bytes = splice_stats.max_socket_splice_bytes;
        stats.max_send_op_bytes = splice_stats.max_sink_splice_bytes;
        stats.pipe_capacity_bytes = splice_stats.pipe_capacity_bytes;
        return Ok(stats);
    }

    let mut phase_ingress = Duration::ZERO;
    let mut phase_egress = Duration::ZERO;
    for (lane, branch0, branch1, downstream) in lanes {
        let file0 = create_sparse_memfd(
            &format!("zcfan-cold-l{lane}-b0"),
            config.branch_bytes_per_lane,
        )?;
        let file1 = create_sparse_memfd(
            &format!("zcfan-cold-l{lane}-b1"),
            config.branch_bytes_per_lane,
        )?;
        let progress0 = BranchProgress {
            written: Arc::new(AtomicU64::new(0)),
        };
        let progress1 = BranchProgress {
            written: Arc::new(AtomicU64::new(0)),
        };
        let recv0 = spawn_splice_receiver(
            branch0,
            file0.try_clone()?,
            config.branch_bytes_per_lane,
            config.chunk_bytes,
            Arc::clone(&progress0.written),
            receiver_cpu_for_worker(&config, worker, 0),
        );
        let recv1 = spawn_splice_receiver(
            branch1,
            file1.try_clone()?,
            config.branch_bytes_per_lane,
            config.chunk_bytes,
            Arc::clone(&progress1.written),
            receiver_cpu_for_worker(&config, worker, 1),
        );

        let prefill = config.prefill_bytes.min(config.branch_bytes_per_lane);
        if prefill != 0 {
            let phase_started = Instant::now();
            for progress in [&progress0, &progress1] {
                while progress.written.load(Ordering::Acquire) < prefill {
                    waits = waits.saturating_add(1);
                    config.wait_mode.wait_once();
                }
            }
            phase_ingress = phase_ingress.saturating_add(phase_started.elapsed());
        }

        let mut branch_offsets = [0u64, 0u64];
        while branch_offsets[0] < config.branch_bytes_per_lane
            || branch_offsets[1] < config.branch_bytes_per_lane
        {
            for (branch, file, progress) in
                [(0usize, &file0, &progress0), (1usize, &file1, &progress1)]
            {
                if branch_offsets[branch] >= config.branch_bytes_per_lane {
                    continue;
                }
                let want = config
                    .extent_bytes
                    .min(config.branch_bytes_per_lane - branch_offsets[branch]);
                let phase_started = Instant::now();
                while progress.written.load(Ordering::Acquire)
                    < branch_offsets[branch].saturating_add(want)
                {
                    waits = waits.saturating_add(1);
                    config.wait_mode.wait_once();
                }
                phase_ingress = phase_ingress.saturating_add(phase_started.elapsed());
                let phase_started = Instant::now();
                downstream_bytes = downstream_bytes.saturating_add(sendfile_at(
                    file.as_raw_fd(),
                    downstream.as_raw_fd(),
                    branch_offsets[branch],
                    want,
                    config.chunk_bytes,
                )?);
                phase_egress = phase_egress.saturating_add(phase_started.elapsed());
                branch_offsets[branch] = branch_offsets[branch].saturating_add(want);
            }
        }

        leaf_bytes = leaf_bytes.saturating_add(
            recv0
                .join()
                .map_err(|_| io::Error::other("branch0 receiver panicked"))??,
        );
        leaf_bytes = leaf_bytes.saturating_add(
            recv1
                .join()
                .map_err(|_| io::Error::other("branch1 receiver panicked"))??,
        );
        let _ = lane;
    }

    let mut stats = finish_stats(
        worker,
        lane_count,
        downstream_bytes,
        leaf_bytes,
        waits,
        0,
        affinity,
        start_cpu,
        start_switches,
        start_thread_cpu,
        started,
    );
    stats.phase_ingress = phase_ingress;
    stats.phase_egress = phase_egress;
    Ok(stats)
}

fn spawn_splice_receiver(
    stream: TcpStream,
    file: File,
    bytes: u64,
    chunk_bytes: usize,
    progress: Arc<AtomicU64>,
    target_cpu: Option<usize>,
) -> thread::JoinHandle<io::Result<u64>> {
    thread::spawn(move || {
        if let Some(cpu) = target_cpu {
            set_current_thread_affinity(cpu)?;
        }
        let pipe = Pipe::new(chunk_bytes)?;
        let mut copied = 0u64;
        while copied < bytes {
            let want = (bytes - copied).min(chunk_bytes as u64) as usize;
            let from_sock = splice_fd(
                stream.as_raw_fd(),
                None,
                pipe.write_fd(),
                None,
                want,
                libc::SPLICE_F_MOVE,
            )?;
            if from_sock == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "leaf stream ended during socket->pipe splice",
                ));
            }
            let mut drained = 0usize;
            while drained < from_sock {
                let mut off = (copied + drained as u64) as libc::off_t;
                let to_file = splice_fd(
                    pipe.read_fd(),
                    None,
                    file.as_raw_fd(),
                    Some(&mut off),
                    from_sock - drained,
                    libc::SPLICE_F_MOVE,
                )?;
                if to_file == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "pipe->memfd splice wrote zero bytes",
                    ));
                }
                drained += to_file;
            }
            copied += from_sock as u64;
            progress.store(copied, Ordering::Release);
        }
        Ok(copied)
    })
}

fn spawn_pipe_receiver(
    stream: TcpStream,
    pipe_write: OwnedFd,
    bytes: u64,
    chunk_bytes: usize,
    target_cpu: Option<usize>,
) -> thread::JoinHandle<io::Result<u64>> {
    thread::spawn(move || {
        if let Some(cpu) = target_cpu {
            set_current_thread_affinity(cpu)?;
        }
        let mut copied = 0u64;
        while copied < bytes {
            let want = (bytes - copied).min(chunk_bytes as u64) as usize;
            let moved = splice_fd(
                stream.as_raw_fd(),
                None,
                pipe_write.as_raw_fd(),
                None,
                want,
                libc::SPLICE_F_MOVE,
            )?;
            if moved == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "leaf stream ended during socket->handoff-pipe splice",
                ));
            }
            copied += moved as u64;
        }
        Ok(copied)
    })
}

fn spawn_slab_receiver(
    stream: TcpStream,
    ring: Arc<SlabRing>,
    bytes: u64,
    extent_bytes: usize,
    target_cpu: Option<usize>,
    wait_mode: WaitMode,
) -> thread::JoinHandle<io::Result<SlabRecvStats>> {
    thread::spawn(move || {
        if let Some(cpu) = target_cpu {
            set_current_thread_affinity(cpu)?;
        }
        let mut copied = 0u64;
        let mut sequence = 0u64;
        let mut waits = 0u64;
        while copied < bytes {
            let want = (bytes - copied).min(extent_bytes as u64) as usize;
            ring.fill_from_socket(sequence, stream.as_raw_fd(), want, wait_mode, &mut waits)?;
            copied = copied.saturating_add(want as u64);
            sequence = sequence.saturating_add(1);
        }
        Ok(SlabRecvStats {
            bytes: copied,
            waits,
        })
    })
}

fn bind_lane_listeners(bind: &str, base_port: u16, lanes: usize) -> io::Result<Vec<TcpListener>> {
    let mut listeners = Vec::with_capacity(lanes);
    for lane in 0..lanes {
        let port = checked_port(base_port, lane)?;
        listeners.push(bind_listener_reuseaddr(bind, port)?);
    }
    Ok(listeners)
}

fn bind_listener_reuseaddr(bind: &str, port: u16) -> io::Result<TcpListener> {
    let addr = (bind, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| invalid_input(format!("could not resolve listen address {bind}:{port}")))?;
    unsafe {
        let domain = match addr {
            SocketAddr::V4(_) => libc::AF_INET,
            SocketAddr::V6(_) => libc::AF_INET6,
        };
        let fd = libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let owned = OwnedFd::from_raw_fd(fd);
        let yes: libc::c_int = 1;
        let ret = libc::setsockopt(
            owned.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            (&yes as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        bind_socket_addr(owned.as_raw_fd(), addr)?;
        if libc::listen(owned.as_raw_fd(), 1024) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(TcpListener::from(owned))
    }
}

fn connect_lanes(
    peer: &str,
    base_port: u16,
    lanes: usize,
    source_ip: Option<IpAddr>,
) -> io::Result<Vec<(usize, TcpStream)>> {
    let mut out = Vec::with_capacity(lanes);
    for lane in 0..lanes {
        let port = checked_port(base_port, lane)?;
        let stream = connect_bound(peer, port, source_ip)?;
        set_stream_options(&stream)?;
        crate::zc_maybe_warn_route_alignment(
            "zcfan-readcache-connect",
            stream.peer_addr()?,
            stream.local_addr().ok(),
        )?;
        println!(
            "zcfan-readcache-connect: lane={lane} peer={peer}:{port} local={}",
            stream.local_addr()?
        );
        out.push((lane, stream));
    }
    Ok(out)
}

fn connect_bound(peer: &str, port: u16, source_ip: Option<IpAddr>) -> io::Result<TcpStream> {
    let remote = (peer, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| invalid_input(format!("could not resolve {peer}:{port}")))?;
    let Some(source_ip) = source_ip else {
        return TcpStream::connect(remote);
    };
    unsafe {
        let domain = match remote {
            SocketAddr::V4(_) => libc::AF_INET,
            SocketAddr::V6(_) => libc::AF_INET6,
        };
        let fd = libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let owned = OwnedFd::from_raw_fd(fd);
        bind_socket_to_ip(owned.as_raw_fd(), source_ip)?;
        connect_socket(owned.as_raw_fd(), remote)?;
        Ok(TcpStream::from(owned))
    }
}

fn bind_socket_to_ip(fd: RawFd, ip: IpAddr) -> io::Result<()> {
    match ip {
        IpAddr::V4(ip) => {
            let addr = libc::sockaddr_in {
                sin_family: libc::AF_INET as u16,
                sin_port: 0,
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(ip.octets()),
                },
                sin_zero: [0; 8],
            };
            let ret = unsafe {
                libc::bind(
                    fd,
                    (&addr as *const libc::sockaddr_in).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            };
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        IpAddr::V6(ip) => {
            let addr = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as u16,
                sin6_port: 0,
                sin6_flowinfo: 0,
                sin6_addr: libc::in6_addr {
                    s6_addr: ip.octets(),
                },
                sin6_scope_id: 0,
            };
            let ret = unsafe {
                libc::bind(
                    fd,
                    (&addr as *const libc::sockaddr_in6).cast(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            };
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

fn connect_socket(fd: RawFd, remote: SocketAddr) -> io::Result<()> {
    let ret = bind_or_connect_socket_addr(fd, remote, false)?;
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn bind_socket_addr(fd: RawFd, addr: SocketAddr) -> io::Result<()> {
    let ret = bind_or_connect_socket_addr(fd, addr, true)?;
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn bind_or_connect_socket_addr(fd: RawFd, addr: SocketAddr, bind: bool) -> io::Result<i32> {
    let ret = match addr {
        SocketAddr::V4(addr) => {
            let raw = libc::sockaddr_in {
                sin_family: libc::AF_INET as u16,
                sin_port: addr.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(addr.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            unsafe {
                if bind {
                    libc::bind(
                        fd,
                        (&raw as *const libc::sockaddr_in).cast(),
                        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                } else {
                    libc::connect(
                        fd,
                        (&raw as *const libc::sockaddr_in).cast(),
                        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                }
            }
        }
        SocketAddr::V6(addr) => {
            let raw = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as u16,
                sin6_port: addr.port().to_be(),
                sin6_flowinfo: addr.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: addr.ip().octets(),
                },
                sin6_scope_id: addr.scope_id(),
            };
            unsafe {
                if bind {
                    libc::bind(
                        fd,
                        (&raw as *const libc::sockaddr_in6).cast(),
                        std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    )
                } else {
                    libc::connect(
                        fd,
                        (&raw as *const libc::sockaddr_in6).cast(),
                        std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    )
                }
            }
        }
    };
    Ok(ret)
}

fn set_stream_options(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    set_socket_buffer(stream.as_raw_fd(), libc::SO_SNDBUF, 64 * 1024 * 1024)?;
    set_socket_buffer(stream.as_raw_fd(), libc::SO_RCVBUF, 64 * 1024 * 1024)?;
    Ok(())
}

fn set_fd_nonblocking(fd: RawFd, nonblocking: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let next = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    if next == flags {
        return Ok(());
    }
    let ret = unsafe { libc::fcntl(fd, libc::F_SETFL, next) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_socket_buffer(fd: RawFd, opt: i32, bytes: usize) -> io::Result<()> {
    let value = bytes as libc::c_int;
    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            opt,
            (&value as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if ret != 0 {
        eprintln!(
            "PERF WARNING: zcfan-readcache setsockopt opt={opt} bytes={bytes} failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(())
}

fn set_socket_rcvlowat(fd: RawFd, bytes: usize, label: &str) -> io::Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let value = bytes as libc::c_int;
    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVLOWAT,
            (&value as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if ret != 0 {
        eprintln!(
            "PERF WARNING: zcfan-readcache {label} SO_RCVLOWAT requested={} failed: {}",
            bytes,
            io::Error::last_os_error()
        );
        return Ok(());
    }

    let mut actual = 0 as libc::c_int;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let get_ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVLOWAT,
            (&mut actual as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if get_ret != 0 {
        eprintln!(
            "PERF WARNING: zcfan-readcache {label} SO_RCVLOWAT readback failed: {}",
            io::Error::last_os_error()
        );
    } else if actual < value {
        eprintln!(
            "PERF WARNING: zcfan-readcache {label} SO_RCVLOWAT requested={} actual={}; bulk receive may still wake on small fragments",
            bytes, actual
        );
    } else {
        eprintln!("zcfan-readcache-socket: {label} SO_RCVLOWAT={actual}");
    }
    Ok(())
}

fn create_cache_file(label: &str, bytes: u64, chunk_bytes: usize, seed: u8) -> io::Result<File> {
    let file = create_sparse_memfd(label, bytes)?;
    let mut buf = vec![seed; chunk_bytes.min(bytes as usize)];
    for (index, byte) in buf.iter_mut().enumerate() {
        *byte = seed.wrapping_add((index & 0xff) as u8);
    }
    let mut offset = 0u64;
    while offset < bytes {
        let take = (bytes - offset).min(buf.len() as u64) as usize;
        file.write_all_at(&buf[..take], offset)?;
        offset += take as u64;
    }
    Ok(file)
}

fn create_sparse_memfd(label: &str, bytes: u64) -> io::Result<File> {
    let name = std::ffi::CString::new(label)
        .map_err(|_| invalid_input("memfd label contains an interior NUL"))?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let ret = unsafe { libc::ftruncate(file.as_raw_fd(), bytes as libc::off_t) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

fn sendfile_wrapped(
    in_fd: RawFd,
    out_fd: RawFd,
    cache_bytes: u64,
    total_bytes: u64,
    chunk_bytes: usize,
) -> io::Result<u64> {
    let mut sent = 0u64;
    while sent < total_bytes {
        let cache_off = sent % cache_bytes;
        let take = (total_bytes - sent)
            .min(cache_bytes - cache_off)
            .min(chunk_bytes as u64);
        sent += sendfile_at(in_fd, out_fd, cache_off, take, chunk_bytes)?;
    }
    Ok(sent)
}

fn sendfile_at(
    in_fd: RawFd,
    out_fd: RawFd,
    offset: u64,
    total_bytes: u64,
    chunk_bytes: usize,
) -> io::Result<u64> {
    let mut sent = 0u64;
    while sent < total_bytes {
        let mut off = (offset + sent) as libc::off_t;
        let take = (total_bytes - sent).min(chunk_bytes as u64) as usize;
        let ret = unsafe { libc::sendfile(out_fd, in_fd, &mut off, take) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ret == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "sendfile wrote zero bytes",
            ));
        }
        sent += ret as u64;
    }
    Ok(sent)
}

fn read_exact_counted(
    stream: &mut TcpStream,
    buf: &mut [u8],
    stats: &mut SpliceDrainStats,
) -> io::Result<usize> {
    let mut done = 0usize;
    while done < buf.len() {
        let want = buf.len() - done;
        let ret = stream.read(&mut buf[done..])?;
        if ret == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "socket ended during counted receive",
            ));
        }
        stats.record_socket_splice(ret, want);
        done += ret;
    }
    Ok(done)
}

fn recv_waitall_counted(
    fd: RawFd,
    buf: &mut [u8],
    stats: &mut SpliceDrainStats,
) -> io::Result<usize> {
    let mut done = 0usize;
    while done < buf.len() {
        let want = buf.len() - done;
        let ret =
            unsafe { libc::recv(fd, buf[done..].as_mut_ptr().cast(), want, libc::MSG_WAITALL) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ret == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "socket ended during MSG_WAITALL receive",
            ));
        }
        stats.record_socket_splice(ret as usize, want);
        done += ret as usize;
    }
    Ok(done)
}

fn recv_waitall(fd: RawFd, buf: &mut [u8]) -> io::Result<()> {
    let mut stats = SpliceDrainStats::default();
    recv_waitall_counted(fd, buf, &mut stats).map(|_| ())
}

fn splice_socket_to_fd(
    socket_fd: RawFd,
    out_fd: RawFd,
    total_bytes: u64,
    chunk_bytes: usize,
) -> io::Result<SpliceDrainStats> {
    let pipe = Pipe::new(chunk_bytes)?;
    let pipe_capacity = pipe.capacity_bytes()?;
    let mut copied = 0u64;
    let mut stats = SpliceDrainStats {
        pipe_capacity_bytes: pipe_capacity as u64,
        ..SpliceDrainStats::default()
    };
    while copied < total_bytes {
        let want = (total_bytes - copied).min(chunk_bytes as u64) as usize;
        let from_sock = splice_fd(
            socket_fd,
            None,
            pipe.write_fd(),
            None,
            want,
            libc::SPLICE_F_MOVE,
        )?;
        if from_sock == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "socket ended during socket->pipe splice",
            ));
        }
        stats.record_socket_splice(from_sock, want);
        let mut drained = 0usize;
        while drained < from_sock {
            let sink_want = from_sock - drained;
            let to_sink = splice_fd(
                pipe.read_fd(),
                None,
                out_fd,
                None,
                sink_want,
                libc::SPLICE_F_MOVE,
            )?;
            if to_sink == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pipe->sink splice wrote zero bytes",
                ));
            }
            stats.record_sink_splice(to_sink, sink_want);
            drained += to_sink;
        }
        copied += from_sock as u64;
    }
    stats.bytes = copied;
    Ok(stats)
}

fn stream_leaf_extent_with_reused_pipes(
    socket_fd: RawFd,
    downstream_fd: RawFd,
    cache_fd: Option<RawFd>,
    cache_offset: u64,
    total_bytes: u64,
    chunk_bytes: usize,
    pipe: &Pipe,
    cache_pipe: Option<&Pipe>,
    stats: &mut SpliceDrainStats,
) -> io::Result<u64> {
    stats.pipe_capacity_bytes = stats.pipe_capacity_bytes.max(pipe.capacity_bytes()? as u64);
    if let Some(cache_pipe) = cache_pipe {
        stats.pipe_capacity_bytes = stats
            .pipe_capacity_bytes
            .max(cache_pipe.capacity_bytes()? as u64);
    }
    let mut moved = 0u64;
    while moved < total_bytes {
        let want = (total_bytes - moved).min(chunk_bytes as u64) as usize;
        let from_sock = splice_fd(
            socket_fd,
            None,
            pipe.write_fd(),
            None,
            want,
            libc::SPLICE_F_MOVE | libc::SPLICE_F_MORE,
        )?;
        if from_sock == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "leaf stream ended during reused-pipe socket->pipe splice",
            ));
        }
        stats.record_socket_splice(from_sock, want);

        let mut remaining = from_sock;
        while remaining != 0 {
            let take = if let (Some(cache_fd), Some(cache_pipe)) = (cache_fd, cache_pipe) {
                let duplicated = tee_fd(pipe.read_fd(), cache_pipe.write_fd(), remaining, 0)?;
                if duplicated == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "reused-pipe tee duplicated zero bytes",
                    ));
                }
                drain_pipe_to_fd_with_flags_counted(
                    pipe.read_fd(),
                    downstream_fd,
                    None,
                    duplicated,
                    libc::SPLICE_F_MOVE | libc::SPLICE_F_MORE,
                    stats,
                )?;
                let mut off =
                    (cache_offset + moved + (from_sock - remaining) as u64) as libc::off_t;
                drain_pipe_to_fd_with_flags_counted(
                    cache_pipe.read_fd(),
                    cache_fd,
                    Some(&mut off),
                    duplicated,
                    libc::SPLICE_F_MOVE | libc::SPLICE_F_MORE,
                    stats,
                )?;
                duplicated
            } else {
                drain_pipe_to_fd_with_flags_counted(
                    pipe.read_fd(),
                    downstream_fd,
                    None,
                    remaining,
                    libc::SPLICE_F_MOVE | libc::SPLICE_F_MORE,
                    stats,
                )?;
                remaining
            };
            remaining -= take;
        }
        moved += from_sock as u64;
    }
    Ok(moved)
}

fn stream_leaf_extent_with_pipe(
    socket_fd: RawFd,
    downstream_fd: RawFd,
    pipe: &Pipe,
    total_bytes: u64,
    chunk_bytes: usize,
    wait_mode: WaitMode,
    waits: &mut u64,
    stats: &mut SpliceDrainStats,
    nonblocking: bool,
) -> io::Result<u64> {
    stats.pipe_capacity_bytes = stats.pipe_capacity_bytes.max(pipe.capacity_bytes()? as u64);
    let mut moved = 0u64;
    while moved < total_bytes {
        let want = (total_bytes - moved).min(chunk_bytes as u64) as usize;
        let splice_flags = libc::SPLICE_F_MOVE
            | libc::SPLICE_F_MORE
            | if nonblocking {
                libc::SPLICE_F_NONBLOCK
            } else {
                0
            };
        let from_sock = if nonblocking {
            splice_fd_wait(
                socket_fd,
                None,
                pipe.write_fd(),
                None,
                want,
                splice_flags,
                wait_mode,
                waits,
            )?
        } else {
            splice_fd(socket_fd, None, pipe.write_fd(), None, want, splice_flags)?
        };
        if from_sock == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "leaf stream ended during lane-local socket->pipe splice",
            ));
        }
        stats.record_socket_splice(from_sock, want);
        if nonblocking {
            drain_pipe_to_fd_with_wait_counted(
                pipe.read_fd(),
                downstream_fd,
                None,
                from_sock,
                splice_flags,
                wait_mode,
                waits,
                stats,
            )?;
        } else {
            drain_pipe_to_fd_with_flags_counted(
                pipe.read_fd(),
                downstream_fd,
                None,
                from_sock,
                splice_flags,
                stats,
            )?;
        }
        moved += from_sock as u64;
    }
    Ok(moved)
}

fn drain_pipe_to_fd(
    pipe_fd: RawFd,
    out_fd: RawFd,
    mut off_out: Option<&mut libc::off_t>,
    bytes: usize,
) -> io::Result<()> {
    drain_pipe_to_fd_with_flags(
        pipe_fd,
        out_fd,
        off_out.as_deref_mut(),
        bytes,
        libc::SPLICE_F_MOVE,
    )
}

fn drain_pipe_to_fd_with_flags(
    pipe_fd: RawFd,
    out_fd: RawFd,
    mut off_out: Option<&mut libc::off_t>,
    bytes: usize,
    flags: u32,
) -> io::Result<()> {
    let mut drained = 0usize;
    while drained < bytes {
        let moved = splice_fd(
            pipe_fd,
            None,
            out_fd,
            off_out.as_deref_mut(),
            bytes - drained,
            flags,
        )?;
        if moved == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "pipe drain wrote zero bytes",
            ));
        }
        drained += moved;
    }
    Ok(())
}

fn drain_pipe_to_fd_with_flags_counted(
    pipe_fd: RawFd,
    out_fd: RawFd,
    mut off_out: Option<&mut libc::off_t>,
    bytes: usize,
    flags: u32,
    stats: &mut SpliceDrainStats,
) -> io::Result<()> {
    let mut drained = 0usize;
    while drained < bytes {
        let want = bytes - drained;
        let moved = splice_fd(pipe_fd, None, out_fd, off_out.as_deref_mut(), want, flags)?;
        if moved == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "pipe drain wrote zero bytes",
            ));
        }
        stats.record_sink_splice(moved, want);
        drained += moved;
    }
    Ok(())
}

fn drain_pipe_to_fd_with_wait_counted(
    pipe_fd: RawFd,
    out_fd: RawFd,
    mut off_out: Option<&mut libc::off_t>,
    bytes: usize,
    flags: u32,
    wait_mode: WaitMode,
    waits: &mut u64,
    stats: &mut SpliceDrainStats,
) -> io::Result<()> {
    let mut drained = 0usize;
    while drained < bytes {
        let want = bytes - drained;
        let moved = splice_fd_wait(
            pipe_fd,
            None,
            out_fd,
            off_out.as_deref_mut(),
            want,
            flags,
            wait_mode,
            waits,
        )?;
        if moved == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "pipe drain wrote zero bytes",
            ));
        }
        stats.record_sink_splice(moved, want);
        drained += moved;
    }
    Ok(())
}

fn tee_fd(fd_in: RawFd, fd_out: RawFd, len: usize, flags: u32) -> io::Result<usize> {
    loop {
        let ret = unsafe { libc::tee(fd_in, fd_out, len, flags) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if err.kind() == io::ErrorKind::WouldBlock {
                std::hint::spin_loop();
                continue;
            }
            return Err(err);
        }
        return Ok(ret as usize);
    }
}

fn uring_zc_ring_entries() -> u32 {
    env::var("URING_PLAY_ZCFAN_READCACHE_RING_ENTRIES")
        .ok()
        .and_then(|value| parse_count(&value).ok())
        .unwrap_or(256)
        .max(32)
        .min(u32::MAX as usize) as u32
}

fn uring_zc_slot_count(config: &Config) -> io::Result<usize> {
    let slots = config.slab_slots.max(1);
    if slots > (u16::MAX as usize / 2) {
        return Err(invalid_input(format!(
            "uring-zc --slab-slots {slots} needs {} fixed buffers, but io_uring fixed buffer indexes are u16",
            slots.saturating_mul(2)
        )));
    }
    Ok(slots)
}

fn current_memlock_limit_bytes() -> io::Result<Option<u64>> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let ret = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    if limit.rlim_cur == libc::RLIM_INFINITY {
        Ok(None)
    } else {
        Ok(Some(limit.rlim_cur as u64))
    }
}

const URING_ZC_KIND_MASK: u64 = 0xf000_0000_0000_0000;
const URING_ZC_RECV_KIND: u64 = 0xa000_0000_0000_0000;
const URING_ZC_SEND_KIND: u64 = 0xb000_0000_0000_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UringZcCqeKind {
    Recv,
    Send,
}

fn uring_zc_user_data(kind: UringZcCqeKind, slot: usize, branch: usize) -> io::Result<u64> {
    if branch >= 2 {
        return Err(invalid_input(format!("uring-zc invalid branch {branch}")));
    }
    let payload = (slot as u64)
        .checked_shl(1)
        .ok_or_else(|| invalid_input("uring-zc slot id overflow"))?
        | branch as u64;
    if payload & URING_ZC_KIND_MASK != 0 {
        return Err(invalid_input("uring-zc slot id does not fit in user_data"));
    }
    let tag = match kind {
        UringZcCqeKind::Recv => URING_ZC_RECV_KIND,
        UringZcCqeKind::Send => URING_ZC_SEND_KIND,
    };
    Ok(tag | payload)
}

fn uring_zc_decode_user_data(user_data: u64) -> io::Result<(UringZcCqeKind, usize, usize)> {
    let kind = match user_data & URING_ZC_KIND_MASK {
        URING_ZC_RECV_KIND => UringZcCqeKind::Recv,
        URING_ZC_SEND_KIND => UringZcCqeKind::Send,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("uring-zc unexpected CQE user_data=0x{user_data:x}"),
            ));
        }
    };
    let payload = user_data & !URING_ZC_KIND_MASK;
    Ok((kind, (payload >> 1) as usize, (payload & 1) as usize))
}

fn uring_zc_fixed_index(slot: usize, branch: usize) -> io::Result<u16> {
    let index = slot
        .checked_mul(2)
        .and_then(|value| value.checked_add(branch))
        .ok_or_else(|| invalid_input("uring-zc fixed buffer index overflow"))?;
    u16::try_from(index).map_err(|_| invalid_input("uring-zc fixed buffer index exceeds u16"))
}

fn uring_zc_queue_recv_slot(
    ring: &mut crate::RawRing,
    fd: RawFd,
    fixed_buffers: &crate::FixedSendBuffers,
    slot: usize,
    branch: usize,
    offset: usize,
    remaining: usize,
    stats: &mut UringZcStats,
) -> io::Result<()> {
    let fixed_index = uring_zc_fixed_index(slot, branch)? as usize;
    let len = remaining.min(u32::MAX as usize);
    let ptr = unsafe { fixed_buffers.ptr(fixed_index).add(offset) };
    ring.queue_recv(
        fd,
        ptr,
        len as u32,
        0,
        uring_zc_user_data(UringZcCqeKind::Recv, slot, branch)?,
    )?;
    stats.recv_ops = stats.recv_ops.saturating_add(1);
    Ok(())
}

fn uring_zc_queue_send_slot(
    ring: &mut crate::RawRing,
    fd: RawFd,
    fixed_buffers: &crate::FixedSendBuffers,
    slot: usize,
    branch: usize,
    len: usize,
    stats: &mut UringZcStats,
) -> io::Result<()> {
    let fixed_index = uring_zc_fixed_index(slot, branch)?;
    let ptr = fixed_buffers.ptr(fixed_index as usize);
    ring.queue_send_zc(
        fd,
        ptr.cast_const(),
        len as u32,
        libc::MSG_NOSIGNAL as u32,
        Some(fixed_index),
        true,
        uring_zc_user_data(UringZcCqeKind::Send, slot, branch)?,
    )?;
    stats.send_ops = stats.send_ops.saturating_add(1);
    Ok(())
}

fn uring_zc_note_notification(
    cqe: crate::IoUringCqe32,
    slot_count: usize,
    send_more: &[[bool; 2]],
    send_notif_seen: &mut [[bool; 2]],
    pending_notifs: &mut usize,
    stats: &mut UringZcStats,
) -> io::Result<()> {
    stats.zc_notifications = stats.zc_notifications.saturating_add(1);
    if (cqe.res & crate::IORING_NOTIF_USAGE_ZC_COPIED) != 0 {
        stats.zc_copied_notifications = stats.zc_copied_notifications.saturating_add(1);
    }
    let (kind, slot, branch) = uring_zc_decode_user_data(cqe.user_data)?;
    if kind != UringZcCqeKind::Send || slot >= slot_count || branch >= 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "uring-zc notification for invalid send slot: user_data=0x{:x}",
                cqe.user_data
            ),
        ));
    }
    if !send_notif_seen[slot][branch] {
        send_notif_seen[slot][branch] = true;
        if send_more[slot][branch] {
            *pending_notifs = pending_notifs.checked_sub(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "uring-zc notification underflow",
                )
            })?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn uring_zc_stream_batched_lane(
    ring: &mut crate::RawRing,
    branch_fds: [RawFd; 2],
    downstream_fd: RawFd,
    fixed_buffers: &crate::FixedSendBuffers,
    slot_count: usize,
    extent_bytes: usize,
    branch_bytes_per_lane: u64,
    stats: &mut UringZcStats,
) -> io::Result<(u64, u64)> {
    let mut scheduled = [0u64, 0u64];
    let mut downstream_bytes = 0u64;
    let mut leaf_bytes = 0u64;
    let mut lens = vec![[0usize; 2]; slot_count];
    let mut filled = vec![[0usize; 2]; slot_count];
    let mut send_more = vec![[false; 2]; slot_count];
    let mut send_notif_seen = vec![[false; 2]; slot_count];

    while scheduled[0] < branch_bytes_per_lane || scheduled[1] < branch_bytes_per_lane {
        lens.fill([0; 2]);
        filled.fill([0; 2]);
        send_more.fill([false; 2]);
        send_notif_seen.fill([false; 2]);

        let mut batch_slots = 0usize;
        let mut pending_recvs = 0usize;
        for slot in 0..slot_count {
            if scheduled[0] >= branch_bytes_per_lane && scheduled[1] >= branch_bytes_per_lane {
                break;
            }
            for branch in 0..2 {
                let remaining = branch_bytes_per_lane.saturating_sub(scheduled[branch]);
                let want = (extent_bytes as u64).min(remaining) as usize;
                lens[slot][branch] = want;
                scheduled[branch] = scheduled[branch].saturating_add(want as u64);
                if want > 0 {
                    uring_zc_queue_recv_slot(
                        ring,
                        branch_fds[branch],
                        fixed_buffers,
                        slot,
                        branch,
                        0,
                        want,
                        stats,
                    )?;
                    pending_recvs = pending_recvs.saturating_add(1);
                }
            }
            batch_slots += 1;
        }

        while pending_recvs > 0 {
            let cqe = ring.wait_cqe()?;
            if (cqe.flags & crate::IORING_CQE_F_NOTIF) != 0 {
                let mut ignored_pending = 0usize;
                uring_zc_note_notification(
                    cqe,
                    slot_count,
                    &send_more,
                    &mut send_notif_seen,
                    &mut ignored_pending,
                    stats,
                )?;
                continue;
            }
            let (kind, slot, branch) = uring_zc_decode_user_data(cqe.user_data)?;
            if kind != UringZcCqeKind::Recv || slot >= batch_slots || branch >= 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "uring-zc recv got invalid CQE user_data=0x{:x}",
                        cqe.user_data
                    ),
                ));
            }
            pending_recvs -= 1;
            if cqe.res < 0 {
                return Err(io::Error::from_raw_os_error(-cqe.res));
            }
            if cqe.res == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "uring-zc branch stream ended during batched receive",
                ));
            }
            let received = cqe.res as usize;
            let next = filled[slot][branch].checked_add(received).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "uring-zc recv fill overflow")
            })?;
            if next > lens[slot][branch] {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "uring-zc recv overfilled slot={slot} branch={branch}: next={next} lens={}",
                        lens[slot][branch]
                    ),
                ));
            }
            filled[slot][branch] = next;
            if next < lens[slot][branch] {
                uring_zc_queue_recv_slot(
                    ring,
                    branch_fds[branch],
                    fixed_buffers,
                    slot,
                    branch,
                    next,
                    lens[slot][branch] - next,
                    stats,
                )?;
                pending_recvs = pending_recvs.saturating_add(1);
            }
        }

        let mut pending_send_cqes = 0usize;
        let mut pending_notifs = 0usize;
        for slot in 0..batch_slots {
            for branch in 0..2 {
                let len = lens[slot][branch];
                if len == 0 {
                    continue;
                }
                uring_zc_queue_send_slot(
                    ring,
                    downstream_fd,
                    fixed_buffers,
                    slot,
                    branch,
                    len,
                    stats,
                )?;
                pending_send_cqes = pending_send_cqes.saturating_add(1);
            }
        }

        while pending_send_cqes > 0 || pending_notifs > 0 {
            let cqe = ring.wait_cqe()?;
            if (cqe.flags & crate::IORING_CQE_F_NOTIF) != 0 {
                uring_zc_note_notification(
                    cqe,
                    slot_count,
                    &send_more,
                    &mut send_notif_seen,
                    &mut pending_notifs,
                    stats,
                )?;
                continue;
            }
            let (kind, slot, branch) = uring_zc_decode_user_data(cqe.user_data)?;
            if kind != UringZcCqeKind::Send || slot >= batch_slots || branch >= 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "uring-zc send got invalid CQE user_data=0x{:x}",
                        cqe.user_data
                    ),
                ));
            }
            pending_send_cqes -= 1;
            if cqe.res < 0 {
                return Err(io::Error::from_raw_os_error(-cqe.res));
            }
            let sent = cqe.res as usize;
            if sent != lens[slot][branch] {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!(
                        "uring-zc batched send was partial for slot={slot} branch={branch}: sent={sent} expected={}; refusing to reorder WAL stream",
                        lens[slot][branch]
                    ),
                ));
            }
            downstream_bytes = downstream_bytes.saturating_add(sent as u64);
            leaf_bytes = leaf_bytes.saturating_add(sent as u64);
            if (cqe.flags & crate::IORING_CQE_F_MORE) != 0 {
                send_more[slot][branch] = true;
                if !send_notif_seen[slot][branch] {
                    pending_notifs = pending_notifs.saturating_add(1);
                }
            }
        }
    }

    Ok((downstream_bytes, leaf_bytes))
}

struct SlabRing {
    slots: Vec<SlabSlot>,
}

struct SlabSlot {
    state: AtomicU8,
    len: AtomicUsize,
    data: UnsafeCell<Box<[u8]>>,
}

unsafe impl Sync for SlabSlot {}

impl SlabRing {
    fn new(slots: usize, slot_bytes: usize) -> io::Result<Self> {
        if slots == 0 || slot_bytes == 0 {
            return Err(invalid_input("slab slots and bytes must be nonzero"));
        }
        let mut out = Vec::with_capacity(slots);
        for _ in 0..slots {
            out.push(SlabSlot {
                state: AtomicU8::new(0),
                len: AtomicUsize::new(0),
                data: UnsafeCell::new(vec![0u8; slot_bytes].into_boxed_slice()),
            });
        }
        Ok(Self { slots: out })
    }

    fn slot(&self, sequence: u64) -> &SlabSlot {
        &self.slots[sequence as usize % self.slots.len()]
    }

    fn fill_from_socket(
        &self,
        sequence: u64,
        socket_fd: RawFd,
        len: usize,
        wait_mode: WaitMode,
        waits: &mut u64,
    ) -> io::Result<()> {
        let slot = self.slot(sequence);
        while slot.state.load(Ordering::Acquire) != 0 {
            *waits = waits.saturating_add(1);
            wait_mode.wait_once();
        }
        let data = unsafe { &mut *slot.data.get() };
        recv_waitall(socket_fd, &mut data[..len])?;
        slot.len.store(len, Ordering::Release);
        slot.state.store(1, Ordering::Release);
        Ok(())
    }

    fn drain_to_socket(
        &self,
        sequence: u64,
        socket_fd: RawFd,
        wait_mode: WaitMode,
        waits: &mut u64,
    ) -> io::Result<usize> {
        let slot = self.slot(sequence);
        while slot.state.load(Ordering::Acquire) != 1 {
            *waits = waits.saturating_add(1);
            wait_mode.wait_once();
        }
        let len = slot.len.load(Ordering::Acquire);
        let data = unsafe { &*slot.data.get() };
        send_all_fd(socket_fd, &data[..len])?;
        slot.state.store(0, Ordering::Release);
        Ok(len)
    }
}

fn send_all_fd(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let ret = unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), libc::MSG_NOSIGNAL) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ret == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "socket send wrote zero bytes",
            ));
        }
        buf = &buf[ret as usize..];
    }
    Ok(())
}

struct Pipe {
    read: OwnedFd,
    write: OwnedFd,
}

impl Pipe {
    fn new(size: usize) -> io::Result<Self> {
        let mut fds = [0; 2];
        let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        let requested = size.max(1);
        let set_ret = unsafe {
            libc::fcntl(
                write.as_raw_fd(),
                libc::F_SETPIPE_SZ,
                requested as libc::c_int,
            )
        };
        if set_ret < 0 {
            eprintln!(
                "PERF WARNING: zcfan-readcache pipe F_SETPIPE_SZ requested={} failed: {}",
                requested,
                io::Error::last_os_error()
            );
        } else if (set_ret as usize) < requested {
            eprintln!(
                "PERF WARNING: zcfan-readcache pipe requested={} actual={} bytes; splice-null results may be pipe-churn limited",
                requested, set_ret
            );
        }
        Ok(Self { read, write })
    }

    fn capacity_bytes(&self) -> io::Result<usize> {
        let ret = unsafe { libc::fcntl(self.write.as_raw_fd(), libc::F_GETPIPE_SZ) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ret as usize)
    }

    fn read_fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    fn write_fd(&self) -> RawFd {
        self.write.as_raw_fd()
    }

    fn into_split(self) -> (OwnedFd, OwnedFd) {
        (self.read, self.write)
    }
}

fn splice_fd(
    fd_in: RawFd,
    mut off_in: Option<&mut libc::off_t>,
    fd_out: RawFd,
    mut off_out: Option<&mut libc::off_t>,
    len: usize,
    flags: u32,
) -> io::Result<usize> {
    loop {
        let off_in_ptr = match off_in {
            Some(ref mut off) => (&mut **off) as *mut libc::off_t,
            None => std::ptr::null_mut(),
        };
        let off_out_ptr = match off_out {
            Some(ref mut off) => (&mut **off) as *mut libc::off_t,
            None => std::ptr::null_mut(),
        };
        let ret = unsafe { libc::splice(fd_in, off_in_ptr, fd_out, off_out_ptr, len, flags) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(ret as usize);
    }
}

fn splice_fd_wait(
    fd_in: RawFd,
    mut off_in: Option<&mut libc::off_t>,
    fd_out: RawFd,
    mut off_out: Option<&mut libc::off_t>,
    len: usize,
    flags: u32,
    wait_mode: WaitMode,
    waits: &mut u64,
) -> io::Result<usize> {
    loop {
        let off_in_ptr = match off_in {
            Some(ref mut off) => (&mut **off) as *mut libc::off_t,
            None => std::ptr::null_mut(),
        };
        let off_out_ptr = match off_out {
            Some(ref mut off) => (&mut **off) as *mut libc::off_t,
            None => std::ptr::null_mut(),
        };
        let ret = unsafe { libc::splice(fd_in, off_in_ptr, fd_out, off_out_ptr, len, flags) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted || err.kind() == io::ErrorKind::WouldBlock {
                *waits = waits.saturating_add(1);
                wait_mode.wait_once();
                continue;
            }
            return Err(err);
        }
        return Ok(ret as usize);
    }
}

fn shard_items<T>(items: Vec<T>, workers: usize) -> Vec<Vec<T>> {
    let workers = workers.max(1);
    let mut out = (0..workers).map(|_| Vec::new()).collect::<Vec<_>>();
    for (index, item) in items.into_iter().enumerate() {
        out[index % workers].push(item);
    }
    out
}

fn active_shards<T>(shards: &[Vec<T>]) -> usize {
    shards.iter().filter(|shard| !shard.is_empty()).count()
}

fn take_lane_stream(streams: &mut Vec<(usize, TcpStream)>, lane: usize) -> io::Result<TcpStream> {
    let index = streams
        .iter()
        .position(|(stream_lane, _)| *stream_lane == lane)
        .ok_or_else(|| invalid_input(format!("missing lane stream {lane}")))?;
    Ok(streams.swap_remove(index).1)
}

fn collect_stats(
    label: &str,
    handles: Vec<thread::JoinHandle<io::Result<WorkerStats>>>,
) -> io::Result<()> {
    let mut stats = Vec::new();
    for handle in handles {
        stats.push(
            handle
                .join()
                .map_err(|_| io::Error::other(format!("{label} worker panicked")))??,
        );
    }
    print_stats(label, &stats);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finish_stats(
    worker: usize,
    lanes: usize,
    bytes: u64,
    aux_bytes: u64,
    waits: u64,
    checksum: u64,
    affinity: Affinity,
    start_cpu: i32,
    start_switches: ThreadSwitches,
    start_thread_cpu: Duration,
    started: Instant,
) -> WorkerStats {
    let end_cpu = current_cpu();
    let end_switches = read_thread_context_switches(current_tid()).unwrap_or(start_switches);
    let end_thread_cpu = thread_cpu_time().unwrap_or(start_thread_cpu);
    WorkerStats {
        worker,
        lanes,
        bytes,
        aux_bytes,
        waits,
        elapsed: started.elapsed(),
        thread_cpu: end_thread_cpu.saturating_sub(start_thread_cpu),
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
        checksum,
        recv_ops: 0,
        send_ops: 0,
        short_recv_ops: 0,
        short_send_ops: 0,
        max_recv_op_bytes: 0,
        max_send_op_bytes: 0,
        pipe_capacity_bytes: 0,
        zc_notifications: 0,
        zc_copied_notifications: 0,
        phase_ingress: Duration::ZERO,
        phase_egress: Duration::ZERO,
    }
}

fn print_stats(label: &str, stats: &[WorkerStats]) {
    let mut total = WorkerStats::default();
    let mut wall = Duration::ZERO;
    for stat in stats {
        let secs = stat.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        let cpu_secs = stat.thread_cpu.as_secs_f64();
        let ingress_secs = stat.phase_ingress.as_secs_f64();
        let egress_secs = stat.phase_egress.as_secs_f64();
        println!(
            "{label}-worker: worker={} lanes={} bytes={} aux_bytes={} waits={} seconds={secs:.6} Gbitps={:.3} aux_Gbitps={:.3} total_io_Gbitps={:.3} thread_cpu_seconds={cpu_secs:.6} cpu_wall_pct={:.1} phase_ingress_seconds={ingress_secs:.6} phase_egress_seconds={egress_secs:.6} target_cpu={} affinity_applied={} start_cpu={} end_cpu={} voluntary_ctxt_switches={} involuntary_ctxt_switches={} migrations={} checksum=0x{:016x} recv_ops={} send_ops={} short_recv_ops={} short_send_ops={} max_recv_op_bytes={} max_send_op_bytes={} pipe_capacity_bytes={} zc_notifications={} zc_copied_notifications={}",
            stat.worker,
            stat.lanes,
            stat.bytes,
            stat.aux_bytes,
            stat.waits,
            stat.bytes as f64 * 8.0 / 1_000_000_000.0 / secs,
            stat.aux_bytes as f64 * 8.0 / 1_000_000_000.0 / secs,
            stat.bytes.saturating_add(stat.aux_bytes) as f64 * 8.0 / 1_000_000_000.0 / secs,
            cpu_secs * 100.0 / secs,
            stat.target_cpu,
            stat.affinity_applied,
            stat.start_cpu,
            stat.end_cpu,
            stat.voluntary_switches,
            stat.involuntary_switches,
            stat.migrations,
            stat.checksum,
            stat.recv_ops,
            stat.send_ops,
            stat.short_recv_ops,
            stat.short_send_ops,
            stat.max_recv_op_bytes,
            stat.max_send_op_bytes,
            stat.pipe_capacity_bytes,
            stat.zc_notifications,
            stat.zc_copied_notifications
        );
        total.bytes = total.bytes.saturating_add(stat.bytes);
        total.aux_bytes = total.aux_bytes.saturating_add(stat.aux_bytes);
        total.waits = total.waits.saturating_add(stat.waits);
        total.thread_cpu = total.thread_cpu.saturating_add(stat.thread_cpu);
        total.phase_ingress = total.phase_ingress.saturating_add(stat.phase_ingress);
        total.phase_egress = total.phase_egress.saturating_add(stat.phase_egress);
        total.voluntary_switches = total
            .voluntary_switches
            .saturating_add(stat.voluntary_switches);
        total.involuntary_switches = total
            .involuntary_switches
            .saturating_add(stat.involuntary_switches);
        total.migrations = total.migrations.saturating_add(stat.migrations);
        total.checksum = total.checksum.wrapping_add(stat.checksum);
        total.recv_ops = total.recv_ops.saturating_add(stat.recv_ops);
        total.send_ops = total.send_ops.saturating_add(stat.send_ops);
        total.short_recv_ops = total.short_recv_ops.saturating_add(stat.short_recv_ops);
        total.short_send_ops = total.short_send_ops.saturating_add(stat.short_send_ops);
        total.max_recv_op_bytes = total.max_recv_op_bytes.max(stat.max_recv_op_bytes);
        total.max_send_op_bytes = total.max_send_op_bytes.max(stat.max_send_op_bytes);
        total.pipe_capacity_bytes = total.pipe_capacity_bytes.max(stat.pipe_capacity_bytes);
        total.zc_notifications = total.zc_notifications.saturating_add(stat.zc_notifications);
        total.zc_copied_notifications = total
            .zc_copied_notifications
            .saturating_add(stat.zc_copied_notifications);
        wall = wall.max(stat.elapsed);
    }
    let secs = wall.as_secs_f64().max(f64::MIN_POSITIVE);
    let cpu_secs = total.thread_cpu.as_secs_f64();
    let ingress_secs = total.phase_ingress.as_secs_f64();
    let egress_secs = total.phase_egress.as_secs_f64();
    println!(
        "{label}-summary: workers={} bytes={} aux_bytes={} waits={} seconds={secs:.6} Gbitps={:.3} aux_Gbitps={:.3} total_io_Gbitps={:.3} thread_cpu_seconds={cpu_secs:.6} cpu_wall_ratio={:.3} phase_ingress_seconds={ingress_secs:.6} phase_egress_seconds={egress_secs:.6} voluntary_ctxt_switches={} involuntary_ctxt_switches={} migrations={} checksum=0x{:016x} recv_ops={} send_ops={} short_recv_ops={} short_send_ops={} max_recv_op_bytes={} max_send_op_bytes={} pipe_capacity_bytes={} zc_notifications={} zc_copied_notifications={} block_devices=no stripe_in_kernel=no mirror_in_kernel=no",
        stats.len(),
        total.bytes,
        total.aux_bytes,
        total.waits,
        total.bytes as f64 * 8.0 / 1_000_000_000.0 / secs,
        total.aux_bytes as f64 * 8.0 / 1_000_000_000.0 / secs,
        total.bytes.saturating_add(total.aux_bytes) as f64 * 8.0 / 1_000_000_000.0 / secs,
        cpu_secs / secs,
        total.voluntary_switches,
        total.involuntary_switches,
        total.migrations,
        total.checksum,
        total.recv_ops,
        total.send_ops,
        total.short_recv_ops,
        total.short_send_ops,
        total.max_recv_op_bytes,
        total.max_send_op_bytes,
        total.pipe_capacity_bytes,
        total.zc_notifications,
        total.zc_copied_notifications
    );
}

fn print_config(config: &Config) {
    println!(
        "zcfan-readcache-config: role={} bind={} branch_binds={} peer={} source_ip={} base_port={} leaf_base_port={} downstream_base_port={} lanes={} branches={} bytes_per_lane={} branch_bytes_per_lane={} cache_bytes_per_lane={} chunk_bytes={} extent_bytes={} workers={} pin={} cpu_list={} verify={} client_sink={} rcvlowat_bytes={} wait_mode={} cold_mode={} slab_slots={} prefill_bytes={} transport=sendfile/splice/tee/slab/lane-splice/io_uring-sendzc page_cache=memfd block_devices=no kernel_raid=no",
        config.role.label(),
        config.bind,
        format_string_list(&config.branch_binds),
        config.peer,
        config
            .source_ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "-".to_string()),
        config.base_port,
        config.leaf_base_port,
        config.downstream_base_port,
        config.lanes,
        config.branches,
        config.bytes_per_lane,
        config.branch_bytes_per_lane,
        config.cache_bytes_per_lane,
        config.chunk_bytes,
        config.extent_bytes,
        config.workers,
        config.pin,
        format_cpu_list(&config.cpu_list),
        config.verify_pattern,
        config.client_sink.label(),
        config.rcvlowat_bytes,
        config.wait_mode.label(),
        config.cold_mode.label(),
        config.slab_slots,
        config.prefill_bytes
    );
    for worker in 0..config.workers {
        println!(
            "zcfan-readcache-topology: worker={worker} target_cpu={}",
            target_cpu_for_worker(config, worker)
                .map(|cpu| cpu.to_string())
                .unwrap_or_else(|| "unpinned".to_string())
        );
        if config.role == Role::FanCold && config.cold_mode.uses_receiver_threads() {
            for branch in 0..config.branches {
                println!(
                    "zcfan-readcache-receiver-topology: worker={worker} branch={branch} target_cpu={}",
                    receiver_cpu_for_worker(config, worker, branch)
                        .map(|cpu| cpu.to_string())
                        .unwrap_or_else(|| "unpinned".to_string())
                );
            }
        }
    }
}

fn maybe_pin_worker(config: &Config, worker: usize) -> Affinity {
    let Some(cpu) = target_cpu_for_worker(config, worker) else {
        return Affinity {
            target_cpu: -1,
            applied: false,
        };
    };
    match set_current_thread_affinity(cpu) {
        Ok(()) => Affinity {
            target_cpu: cpu as i32,
            applied: true,
        },
        Err(err) => {
            eprintln!(
                "PERF WARNING: zcfan-readcache affinity worker={worker} target_cpu={cpu} failed: {err}"
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

fn receiver_cpu_for_worker(config: &Config, worker: usize, branch: usize) -> Option<usize> {
    if !config.pin || config.cpu_list.is_empty() {
        return None;
    }
    let index = worker.saturating_add((branch + 1).saturating_mul(config.workers));
    config.cpu_list.get(index).copied()
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
    let status = std::fs::read_to_string(format!("/proc/self/task/{tid}/status"))?;
    let mut switches = ThreadSwitches::default();
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("voluntary_ctxt_switches:") {
            switches.voluntary = value.trim().parse::<u64>().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
            switches.involuntary = value.trim().parse::<u64>().unwrap_or(0);
        }
    }
    if let Ok(sched) = std::fs::read_to_string(format!("/proc/self/task/{tid}/sched")) {
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

fn sample_checksum(buf: &[u8]) -> u64 {
    let mut out = 0u64;
    for byte in buf.iter().step_by(4096.max(buf.len() / 16).max(1)) {
        out = out.rotate_left(7).wrapping_add(*byte as u64);
    }
    out
}

fn checked_port(base: u16, offset: usize) -> io::Result<u16> {
    let port = base as usize + offset;
    u16::try_from(port).map_err(|_| invalid_input("port range exceeds u16"))
}

fn parse_ip(value: &str) -> io::Result<IpAddr> {
    value
        .parse::<IpAddr>()
        .map_err(|err| invalid_input(format!("invalid IP {value:?}: {err}")))
}

fn parse_u16(value: &str) -> io::Result<u16> {
    value
        .parse::<u16>()
        .map_err(|err| invalid_input(format!("invalid u16 {value:?}: {err}")))
}

fn parse_count(value: &str) -> io::Result<usize> {
    let parsed = parse_count_u64(value)?;
    usize::try_from(parsed).map_err(|_| invalid_input(format!("{value:?} exceeds usize")))
}

fn parse_count_u64(value: &str) -> io::Result<u64> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid_input("empty numeric value"));
    }
    let (digits, scale) = match value.as_bytes()[value.len() - 1] as char {
        'k' | 'K' => (&value[..value.len() - 1], 1024u64),
        'm' | 'M' => (&value[..value.len() - 1], 1024u64 * 1024),
        'g' | 'G' => (&value[..value.len() - 1], 1024u64 * 1024 * 1024),
        _ => (value, 1u64),
    };
    let base = digits
        .parse::<u64>()
        .map_err(|err| invalid_input(format!("invalid numeric value {value:?}: {err}")))?;
    base.checked_mul(scale)
        .ok_or_else(|| invalid_input(format!("numeric value {value:?} overflows u64")))
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

fn format_cpu_list(cpus: &[usize]) -> String {
    if cpus.is_empty() {
        return "-".to_string();
    }
    cpus.iter()
        .map(|cpu| cpu.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_csv_strings(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn format_string_list(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(",")
    }
}

fn branch_bind_for(config: &Config, branch: usize) -> &str {
    config
        .branch_binds
        .get(branch)
        .map(String::as_str)
        .unwrap_or(&config.bind)
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

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn print_usage() {
    println!(
        "usage: zcfan-readcache-bench <client-recv|fan-hot|fan-cold|leaf-send> [options]\n\
         \n\
         Hot path: client-recv listens; fan-hot sends resident memfd cache pages with sendfile.\n\
         Cold path: client-recv listens; fan-cold accepts two leaf streams, splices leaf sockets\n\
         into branch-local memfd page-cache files, and sendfile-zips extents downstream.\n\
         No block device is used as a mirror or stripe primitive.\n\
         \n\
         Options:\n\
           --bind ADDR                 listen address for client-recv/fan-cold\n\
           --branch-binds A,B          fan-cold per-branch listen addresses\n\
           --peer ADDR                 peer address for fan-hot/leaf-send/fan-cold downstream\n\
           --source-ip ADDR            bind outgoing sockets to data NIC source address\n\
           --base-port N               client ports for client-recv/fan-hot or leaf ports for leaf-send\n\
           --leaf-base-port N          fan-cold branch0 ports; branch1 uses +lanes\n\
           --downstream-base-port N    fan-cold downstream client base port\n\
           --lanes N                   lane/socket count\n\
           --branches N                cold branch count, currently 2\n\
           --bytes-per-lane N          downstream bytes per lane\n\
           --branch-bytes-per-lane N   bytes per leaf lane for cold/leaf-send\n\
           --cache-bytes-per-lane N    resident hot cache window per worker/lane\n\
           --chunk-bytes N             sendfile/splice chunk size\n\
           --extent-bytes N            cold zip extent size\n\
           --workers N                 worker threads, default lanes\n\
           --pin --cpu-list LIST       topology-explicit worker pinning\n\
           --client-sink MODE          client receive path: read, waitall, or splice-null\n\
           --rcvlowat-bytes N          opt-in SO_RCVLOWAT bulk receive threshold\n\
           --wait-mode MODE            fan-cold wait mode: spin or yield\n\
           --cold-mode MODE            cache-then-send, lane-splice, lane-splice-blocking, uring-zc, stream-tee, stream-direct, pipe-direct, or slab-direct\n\
           --slab-slots N              leased slab slots per lane/branch\n\
           --prefill-bytes N           cache-then-send branch runway before downstream zip"
    );
}
