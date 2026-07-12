use std::cell::UnsafeCell;
use std::env;
use std::hint::spin_loop;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Atomic,
    Local,
    Owner,
    Dispatch,
    All,
}

impl Mode {
    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "atomic" | "global-atomic" => Ok(Self::Atomic),
            "local" | "lane-local" => Ok(Self::Local),
            "owner" | "owner-matrix" | "spsc-owner" => Ok(Self::Owner),
            "dispatch" | "ordered-dispatch" | "ordered-owner" => Ok(Self::Dispatch),
            "all" => Ok(Self::All),
            _ => Err(invalid(format!(
                "unknown mode {value:?}; use atomic, local, owner, dispatch, or all"
            ))),
        }
    }
}

#[derive(Clone, Debug)]
struct Config {
    mode: Mode,
    lanes: usize,
    ops_per_lane: u64,
    records: usize,
    hot_percent: u32,
    window: usize,
    ring_entries: usize,
    dispatch_batch: usize,
    pin: bool,
    ingress_cpus: Vec<usize>,
    owner_cpus: Vec<usize>,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OrderDesc {
    record: u64,
    sequence: u64,
    payload_slot: u32,
    generation: u32,
    ingress_lane: u16,
    flags: u16,
    _reserved: u32,
}

const DESC_F_BARRIER: u16 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ReleaseDesc {
    sequence: u64,
    payload_slot: u32,
    generation: u32,
}

#[repr(align(64))]
struct CacheLineAtomicUsize(AtomicUsize);

struct SpscRing<T: Copy + Default> {
    slots: Box<[UnsafeCell<T>]>,
    mask: usize,
    head: CacheLineAtomicUsize,
    tail: CacheLineAtomicUsize,
}

unsafe impl<T: Copy + Default + Send> Send for SpscRing<T> {}
// The matrix construction gives every ring exactly one producer and one consumer.
unsafe impl<T: Copy + Default + Send> Sync for SpscRing<T> {}

impl<T: Copy + Default> SpscRing<T> {
    fn new(entries: usize) -> io::Result<Self> {
        if entries < 2 || !entries.is_power_of_two() {
            return Err(invalid("ring entries must be a power of two >= 2"));
        }
        Ok(Self {
            slots: (0..entries)
                .map(|_| UnsafeCell::new(T::default()))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            mask: entries - 1,
            head: CacheLineAtomicUsize(AtomicUsize::new(0)),
            tail: CacheLineAtomicUsize(AtomicUsize::new(0)),
        })
    }

    fn try_push(&self, value: T) -> Result<(), T> {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= self.slots.len() {
            return Err(value);
        }
        unsafe { *self.slots[head & self.mask].get() = value };
        self.head.0.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    fn try_pop(&self) -> Option<T> {
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let value = unsafe { *self.slots[tail & self.mask].get() };
        self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        Some(value)
    }

    fn try_push_slice(&self, values: &[T]) -> usize {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);
        let used = head.wrapping_sub(tail);
        let count = values.len().min(self.slots.len().saturating_sub(used));
        for (offset, value) in values.iter().copied().take(count).enumerate() {
            unsafe { *self.slots[head.wrapping_add(offset) & self.mask].get() = value };
        }
        if count != 0 {
            self.head
                .0
                .store(head.wrapping_add(count), Ordering::Release);
        }
        count
    }

    fn pop_batch(&self, out: &mut Vec<T>, max: usize) -> usize {
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);
        let count = head.wrapping_sub(tail).min(max);
        out.reserve(count);
        for offset in 0..count {
            out.push(unsafe { *self.slots[tail.wrapping_add(offset) & self.mask].get() });
        }
        if count != 0 {
            self.tail
                .0
                .store(tail.wrapping_add(count), Ordering::Release);
        }
        count
    }
}

struct SequenceSlot<T: Copy + Default> {
    published: AtomicU64,
    value: UnsafeCell<T>,
}

struct SequenceRing<T: Copy + Default> {
    slots: Box<[SequenceSlot<T>]>,
    mask: usize,
}

const SEQUENCE_RESERVED: u64 = u64::MAX;

unsafe impl<T: Copy + Default + Send> Send for SequenceRing<T> {}
// Each sequence has one publisher and the ring has one ordered consumer.
unsafe impl<T: Copy + Default + Send> Sync for SequenceRing<T> {}

impl<T: Copy + Default> SequenceRing<T> {
    fn new(entries: usize) -> io::Result<Self> {
        if entries < 2 || !entries.is_power_of_two() {
            return Err(invalid("sequence ring entries must be a power of two >= 2"));
        }
        Ok(Self {
            slots: (0..entries)
                .map(|_| SequenceSlot {
                    published: AtomicU64::new(0),
                    value: UnsafeCell::new(T::default()),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            mask: entries - 1,
        })
    }

    fn try_publish(&self, sequence: u64, value: T) -> Result<(), T> {
        if sequence == 0 || sequence == SEQUENCE_RESERVED {
            return Err(value);
        }
        let slot = &self.slots[sequence as usize & self.mask];
        if slot
            .published
            .compare_exchange(0, SEQUENCE_RESERVED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(value);
        }
        unsafe { *slot.value.get() = value };
        slot.published.store(sequence, Ordering::Release);
        Ok(())
    }

    fn try_consume(&self, sequence: u64) -> Option<T> {
        let slot = &self.slots[sequence as usize & self.mask];
        if slot.published.load(Ordering::Acquire) != sequence {
            return None;
        }
        let value = unsafe { *slot.value.get() };
        slot.published.store(0, Ordering::Release);
        Some(value)
    }
}

#[derive(Clone, Copy, Default)]
struct ThreadStats {
    ops: u64,
    spins: u64,
    checksum: u64,
    cpu: i32,
    voluntary: u64,
    involuntary: u64,
}

#[derive(Clone, Copy)]
struct RunResult {
    wall: Duration,
    logical_ops: u64,
    spins: u64,
    checksum: u64,
    voluntary: u64,
    involuntary: u64,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn parse_usize(value: &str, label: &str) -> io::Result<usize> {
    value
        .parse::<usize>()
        .map_err(|error| invalid(format!("invalid {label} {value:?}: {error}")))
}

fn parse_u64(value: &str, label: &str) -> io::Result<u64> {
    value
        .parse::<u64>()
        .map_err(|error| invalid(format!("invalid {label} {value:?}: {error}")))
}

fn parse_cpu_list(value: &str) -> io::Result<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((start, end)) = part.split_once('-') {
            let start = parse_usize(start, "CPU range start")?;
            let end = parse_usize(end, "CPU range end")?;
            if end < start {
                return Err(invalid("CPU range end is below its start"));
            }
            for cpu in start..=end {
                if !cpus.contains(&cpu) {
                    cpus.push(cpu);
                }
            }
        } else {
            let cpu = parse_usize(part, "CPU")?;
            if !cpus.contains(&cpu) {
                cpus.push(cpu);
            }
        }
    }
    Ok(cpus)
}

fn default_cpu_maps(lanes: usize) -> (Vec<usize>, Vec<usize>) {
    let ingress = (0..lanes).map(|lane| lane * 4).collect::<Vec<_>>();
    let owners = (0..lanes).map(|lane| lane * 4 + 1).collect::<Vec<_>>();
    (ingress, owners)
}

fn parse_config() -> io::Result<Config> {
    let mut mode = Mode::All;
    let mut lanes = 4usize;
    let mut ops_per_lane = 5_000_000u64;
    let mut records = 262_144usize;
    let mut hot_percent = 0u32;
    let mut window = 4_096usize;
    let mut ring_entries = 4_096usize;
    let mut dispatch_batch = 256usize;
    let mut pin = true;
    let mut ingress_cpus = None;
    let mut owner_cpus = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = |args: &mut std::iter::Skip<env::Args>, name: &str| {
            args.next()
                .ok_or_else(|| invalid(format!("{name} needs a value")))
        };
        match arg.as_str() {
            "--mode" => mode = Mode::parse(&value(&mut args, "--mode")?)?,
            "--lanes" => lanes = parse_usize(&value(&mut args, "--lanes")?, "lanes")?,
            "--ops-per-lane" => {
                ops_per_lane = parse_u64(&value(&mut args, "--ops-per-lane")?, "ops")?
            }
            "--records" => records = parse_usize(&value(&mut args, "--records")?, "records")?,
            "--hot-percent" => {
                hot_percent = value(&mut args, "--hot-percent")?
                    .parse::<u32>()
                    .map_err(|error| invalid(format!("invalid hot percent: {error}")))?
            }
            "--window" => window = parse_usize(&value(&mut args, "--window")?, "window")?,
            "--ring-entries" => {
                ring_entries = parse_usize(&value(&mut args, "--ring-entries")?, "ring entries")?
            }
            "--dispatch-batch" => {
                dispatch_batch =
                    parse_usize(&value(&mut args, "--dispatch-batch")?, "dispatch batch")?
            }
            "--ingress-cpus" => {
                ingress_cpus = Some(parse_cpu_list(&value(&mut args, "--ingress-cpus")?)?)
            }
            "--owner-cpus" => {
                owner_cpus = Some(parse_cpu_list(&value(&mut args, "--owner-cpus")?)?)
            }
            "--pin" => pin = true,
            "--no-pin" => pin = false,
            "-h" | "--help" => {
                println!(
                    "usage: zcorder-bench [--mode atomic|local|owner|dispatch|all] [--lanes N] \
                     [--ops-per-lane N] [--records N] [--hot-percent 0..100] [--window N] [--ring-entries N] [--dispatch-batch N] \
                     [--ingress-cpus LIST] [--owner-cpus LIST] [--pin|--no-pin]"
                );
                std::process::exit(0);
            }
            _ => return Err(invalid(format!("unknown argument {arg:?}"))),
        }
    }
    if lanes == 0 || lanes > u16::MAX as usize {
        return Err(invalid("lanes must be in 1..=65535"));
    }
    if ops_per_lane == 0 || records == 0 || window == 0 {
        return Err(invalid("ops, records, and window must be positive"));
    }
    if hot_percent > 100 {
        return Err(invalid("hot percent must be in 0..=100"));
    }
    if ring_entries < window || !ring_entries.is_power_of_two() {
        return Err(invalid("ring entries must be a power of two >= window"));
    }
    if dispatch_batch == 0 || dispatch_batch > ring_entries {
        return Err(invalid("dispatch batch must be in 1..=ring-entries"));
    }
    let (default_ingress, default_owners) = default_cpu_maps(lanes);
    let ingress_cpus = ingress_cpus.unwrap_or(default_ingress);
    let owner_cpus = owner_cpus.unwrap_or(default_owners);
    if pin && (ingress_cpus.len() != lanes || owner_cpus.len() != lanes) {
        return Err(invalid(
            "pinned runs need one ingress and owner CPU per lane",
        ));
    }
    Ok(Config {
        mode,
        lanes,
        ops_per_lane,
        records,
        hot_percent,
        window,
        ring_entries,
        dispatch_batch,
        pin,
        ingress_cpus,
        owner_cpus,
    })
}

fn pin_current(cpu: usize) -> io::Result<()> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
    }
    let result = unsafe {
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set as *const _)
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn context_switches() -> (u64, u64) {
    let Ok(status) = std::fs::read_to_string("/proc/thread-self/status") else {
        return (0, 0);
    };
    let mut voluntary = 0;
    let mut involuntary = 0;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("voluntary_ctxt_switches:") {
            voluntary = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
            involuntary = value.trim().parse().unwrap_or(0);
        }
    }
    (voluntary, involuntary)
}

fn current_cpu() -> i32 {
    unsafe { libc::sched_getcpu() }
}

fn next_random(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    value
}

fn next_record(state: &mut u64, records: usize, hot_percent: u32) -> usize {
    if hot_percent == 0 {
        return next_random(state) as usize % records;
    }
    let selector = next_random(state);
    if selector % 100 < u64::from(hot_percent) {
        0
    } else {
        next_random(state) as usize % records
    }
}

fn print_topology(config: &Config, mode: &str) {
    println!(
        "zcorder-bench-plan: mode={mode} lanes={} ops_per_lane={} records={} hot_percent={} window={} ring_entries={} dispatch_batch={} pin={}",
        config.lanes,
        config.ops_per_lane,
        config.records,
        config.hot_percent,
        config.window,
        config.ring_entries,
        config.dispatch_batch,
        config.pin
    );
    for lane in 0..config.lanes {
        println!(
            "zcorder-bench-topology: lane={lane} ingress_cpu={} owner_cpu={} owner_rule=record_mod_lane payload=descriptor-reference release=exact-slot-generation",
            config.ingress_cpus.get(lane).map_or(-1, |cpu| *cpu as i32),
            config.owner_cpus.get(lane).map_or(-1, |cpu| *cpu as i32),
        );
    }
}

fn finish_result(label: &str, result: RunResult) {
    let seconds = result.wall.as_secs_f64().max(f64::MIN_POSITIVE);
    let (payload_slots, ordering) = match label {
        "global-atomic" => ("not-modeled", "global-submit+per-record-atomic"),
        "lane-local" => ("not-modeled", "lane-private-ceiling"),
        "owner-matrix" => ("exact", "stable-owner-arrival-unlinked-concurrent"),
        "ordered-dispatch" => ("exact", "global-submit-dispatch+stable-owner"),
        _ => ("unknown", "unknown"),
    };
    println!(
        "zcorder-bench-result: mode={label} logical_ops={} wall_seconds={seconds:.6} logical_iops={:.0} descriptor_Gops={:.3} spins={} context_switches={} context_switches_per_1k={:.6} sync_cuts=1 payload_slots_returned={payload_slots} ordering={ordering} checksum={}",
        result.logical_ops,
        result.logical_ops as f64 / seconds,
        result.logical_ops as f64 / seconds / 1e9,
        result.spins,
        result.voluntary + result.involuntary,
        (result.voluntary + result.involuntary) as f64 * 1000.0 / result.logical_ops as f64,
        result.checksum,
    );
}

fn run_ordered_dispatch(config: &Config) -> io::Result<RunResult> {
    print_topology(config, "ordered-dispatch");
    let reorder_entries = config
        .window
        .checked_mul(config.lanes)
        .and_then(usize::checked_next_power_of_two)
        .ok_or_else(|| invalid("ordered-dispatch reorder window overflow"))?;
    let reorder_ring = Arc::new(SequenceRing::<OrderDesc>::new(reorder_entries)?);
    let owner_rings = Arc::new(
        (0..config.lanes)
            .map(|_| SpscRing::<OrderDesc>::new(config.ring_entries))
            .collect::<io::Result<Vec<_>>>()?,
    );
    let releases = Arc::new(
        (0..config.lanes * config.lanes)
            .map(|_| SpscRing::<ReleaseDesc>::new(config.ring_entries))
            .collect::<io::Result<Vec<_>>>()?,
    );
    let next_sequence = Arc::new(AtomicU64::new(1));
    let thread_count = config
        .lanes
        .checked_mul(2)
        .and_then(|count| count.checked_add(2))
        .ok_or_else(|| invalid("ordered-dispatch thread count overflow"))?;
    let barrier = Arc::new(Barrier::new(thread_count));
    let records_per_owner = config.records.div_ceil(config.lanes).max(1);
    let dispatch_batch = config.dispatch_batch;

    let mut owner_handles = Vec::with_capacity(config.lanes);
    for owner in 0..config.lanes {
        let owner_rings = Arc::clone(&owner_rings);
        let releases = Arc::clone(&releases);
        let barrier = Arc::clone(&barrier);
        let lanes = config.lanes;
        let pin = config.pin;
        let cpu = config.owner_cpus[owner];
        owner_handles.push(thread::spawn(move || -> io::Result<ThreadStats> {
            if pin {
                pin_current(cpu)?;
            }
            let before = context_switches();
            let mut latest = vec![0u64; records_per_owner];
            let mut batch = Vec::with_capacity(dispatch_batch);
            let mut stats = ThreadStats::default();
            barrier.wait();
            loop {
                batch.clear();
                if owner_rings[owner].pop_batch(&mut batch, dispatch_batch) == 0 {
                    stats.spins += 1;
                    spin_loop();
                    continue;
                }
                let mut reached_barrier = false;
                for desc in batch.iter().copied() {
                    if desc.flags & DESC_F_BARRIER != 0 {
                        reached_barrier = true;
                        continue;
                    }
                    if reached_barrier || desc.record as usize % lanes != owner {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "ordered-dispatch owner batch topology mismatch",
                        ));
                    }
                    let local_record = desc.record as usize / lanes;
                    let previous = latest[local_record];
                    if previous >= desc.sequence {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "ordered-dispatch regressed per-record sequence",
                        ));
                    }
                    latest[local_record] = desc.sequence;
                    stats.checksum ^= previous.wrapping_add(desc.record);
                    stats.ops += 1;
                    push_wait(
                        &releases[matrix_index(desc.ingress_lane as usize, owner, lanes)],
                        ReleaseDesc {
                            sequence: desc.sequence,
                            payload_slot: desc.payload_slot,
                            generation: desc.generation,
                        },
                        &mut stats.spins,
                    );
                }
                if reached_barrier {
                    break;
                }
            }
            let after = context_switches();
            stats.cpu = current_cpu();
            stats.voluntary = after.0.saturating_sub(before.0);
            stats.involuntary = after.1.saturating_sub(before.1);
            Ok(stats)
        }));
    }

    let dispatcher_ring = Arc::clone(&reorder_ring);
    let dispatcher_owners = Arc::clone(&owner_rings);
    let dispatcher_barrier = Arc::clone(&barrier);
    let lanes = config.lanes;
    let total_ops = config
        .ops_per_lane
        .checked_mul(lanes as u64)
        .ok_or_else(|| invalid("ordered-dispatch operation count overflow"))?;
    let dispatcher_cpu = config
        .owner_cpus
        .iter()
        .chain(config.ingress_cpus.iter())
        .copied()
        .max()
        .and_then(|cpu| cpu.checked_add(1));
    let dispatcher_pin = config.pin;
    let dispatcher_handle = thread::spawn(move || -> io::Result<ThreadStats> {
        if dispatcher_pin {
            pin_current(dispatcher_cpu.ok_or_else(|| invalid("dispatcher CPU overflow"))?)?;
        }
        let before = context_switches();
        let mut stats = ThreadStats::default();
        let mut owner_batches = (0..lanes)
            .map(|_| Vec::<OrderDesc>::with_capacity(dispatch_batch))
            .collect::<Vec<_>>();
        dispatcher_barrier.wait();
        let mut expected = 1u64;
        while expected <= total_ops {
            for batch in &mut owner_batches {
                batch.clear();
            }
            let batch_end = total_ops.min(
                expected
                    .saturating_add(dispatch_batch as u64)
                    .saturating_sub(1),
            );
            while expected <= batch_end {
                let desc = loop {
                    if let Some(desc) = dispatcher_ring.try_consume(expected) {
                        break desc;
                    }
                    stats.spins += 1;
                    spin_loop();
                };
                let owner = desc.record as usize % lanes;
                owner_batches[owner].push(desc);
                stats.ops += 1;
                stats.checksum ^= desc.sequence.rotate_left(owner as u32);
                expected += 1;
            }
            for (owner, batch) in owner_batches.iter().enumerate() {
                if !batch.is_empty() {
                    push_slice_wait(&dispatcher_owners[owner], batch, &mut stats.spins);
                }
            }
        }
        for owner in 0..lanes {
            push_wait(
                &dispatcher_owners[owner],
                OrderDesc {
                    flags: DESC_F_BARRIER,
                    ..OrderDesc::default()
                },
                &mut stats.spins,
            );
        }
        let after = context_switches();
        stats.cpu = current_cpu();
        stats.voluntary = after.0.saturating_sub(before.0);
        stats.involuntary = after.1.saturating_sub(before.1);
        Ok(stats)
    });

    let mut ingress_handles = Vec::with_capacity(config.lanes);
    for ingress in 0..config.lanes {
        let reorder_ring = Arc::clone(&reorder_ring);
        let releases = Arc::clone(&releases);
        let next_sequence = Arc::clone(&next_sequence);
        let barrier = Arc::clone(&barrier);
        let lanes = config.lanes;
        let ops = config.ops_per_lane;
        let records = config.records;
        let hot_percent = config.hot_percent;
        let window = config.window;
        let pin = config.pin;
        let cpu = config.ingress_cpus[ingress];
        ingress_handles.push(thread::spawn(move || -> io::Result<ThreadStats> {
            if pin {
                pin_current(cpu)?;
            }
            let before = context_switches();
            let mut free_slots = (0..window as u32).rev().collect::<Vec<_>>();
            let mut generations = vec![1u32; window];
            let mut random = 0xe703_7ed1_a0b4_28db ^ ingress as u64;
            let mut released = 0u64;
            let mut release_cursor = 0usize;
            let mut stats = ThreadStats::default();
            barrier.wait();
            for _ in 0..ops {
                while free_slots.is_empty() {
                    let owner = release_cursor;
                    release_cursor = (release_cursor + 1) % lanes;
                    if let Some(release) = releases[matrix_index(ingress, owner, lanes)].try_pop() {
                        let slot = release.payload_slot as usize;
                        if slot >= window || generations[slot] != release.generation {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "ordered-dispatch stale payload release",
                            ));
                        }
                        generations[slot] = generations[slot].wrapping_add(1).max(1);
                        free_slots.push(release.payload_slot);
                        released += 1;
                    } else {
                        stats.spins += 1;
                        spin_loop();
                    }
                }
                let slot = free_slots.pop().expect("free slot checked");
                let record = next_record(&mut random, records, hot_percent);
                let sequence = next_sequence.fetch_add(1, Ordering::Relaxed);
                publish_wait(
                    &reorder_ring,
                    sequence,
                    OrderDesc {
                        record: record as u64,
                        sequence,
                        payload_slot: slot,
                        generation: generations[slot as usize],
                        ingress_lane: ingress as u16,
                        flags: 0,
                        _reserved: 0,
                    },
                    &mut stats.spins,
                );
            }
            while released != ops {
                let owner = release_cursor;
                release_cursor = (release_cursor + 1) % lanes;
                if let Some(release) = releases[matrix_index(ingress, owner, lanes)].try_pop() {
                    let slot = release.payload_slot as usize;
                    if slot >= window || generations[slot] != release.generation {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "ordered-dispatch stale terminal payload release",
                        ));
                    }
                    generations[slot] = generations[slot].wrapping_add(1).max(1);
                    free_slots.push(release.payload_slot);
                    released += 1;
                    stats.checksum ^= release.sequence.rotate_left(owner as u32);
                } else {
                    stats.spins += 1;
                    spin_loop();
                }
            }
            if free_slots.len() != window {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ordered-dispatch leaked payload slots",
                ));
            }
            let after = context_switches();
            stats.ops = ops;
            stats.cpu = current_cpu();
            stats.voluntary = after.0.saturating_sub(before.0);
            stats.involuntary = after.1.saturating_sub(before.1);
            Ok(stats)
        }));
    }

    barrier.wait();
    let started = Instant::now();
    let mut aggregate = ThreadStats::default();
    for (lane, handle) in ingress_handles.into_iter().enumerate() {
        let stats = handle
            .join()
            .map_err(|_| io::Error::other("ordered-dispatch ingress panicked"))??;
        println!(
            "zcorder-bench-worker: mode=ordered-ingress lane={lane} cpu={} ops={} spins={} context_switches={}",
            stats.cpu,
            stats.ops,
            stats.spins,
            stats.voluntary + stats.involuntary
        );
        aggregate.ops += stats.ops;
        aggregate.spins += stats.spins;
        aggregate.checksum ^= stats.checksum;
        aggregate.voluntary += stats.voluntary;
        aggregate.involuntary += stats.involuntary;
    }
    let dispatcher = dispatcher_handle
        .join()
        .map_err(|_| io::Error::other("ordered dispatcher panicked"))??;
    if dispatcher.ops != aggregate.ops {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "ordered dispatcher processed {} operations, expected {}",
                dispatcher.ops, aggregate.ops
            ),
        ));
    }
    println!(
        "zcorder-bench-worker: mode=ordered-dispatch cpu={} ops={} spins={} context_switches={}",
        dispatcher.cpu,
        dispatcher.ops,
        dispatcher.spins,
        dispatcher.voluntary + dispatcher.involuntary
    );
    aggregate.spins += dispatcher.spins;
    aggregate.checksum ^= dispatcher.checksum;
    aggregate.voluntary += dispatcher.voluntary;
    aggregate.involuntary += dispatcher.involuntary;
    let mut owner_ops = 0u64;
    for (lane, handle) in owner_handles.into_iter().enumerate() {
        let stats = handle
            .join()
            .map_err(|_| io::Error::other("ordered-dispatch owner panicked"))??;
        println!(
            "zcorder-bench-worker: mode=ordered-owner lane={lane} cpu={} ops={} spins={} context_switches={}",
            stats.cpu,
            stats.ops,
            stats.spins,
            stats.voluntary + stats.involuntary
        );
        owner_ops += stats.ops;
        aggregate.spins += stats.spins;
        aggregate.checksum ^= stats.checksum;
        aggregate.voluntary += stats.voluntary;
        aggregate.involuntary += stats.involuntary;
    }
    if owner_ops != aggregate.ops {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "ordered owners processed {owner_ops} operations, expected {}",
                aggregate.ops
            ),
        ));
    }
    Ok(RunResult {
        wall: started.elapsed(),
        logical_ops: aggregate.ops,
        spins: aggregate.spins,
        checksum: aggregate.checksum,
        voluntary: aggregate.voluntary,
        involuntary: aggregate.involuntary,
    })
}

fn run_atomic(config: &Config) -> io::Result<RunResult> {
    print_topology(config, "global-atomic");
    let table = Arc::new(
        (0..config.records)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>(),
    );
    let next_sequence = Arc::new(AtomicU64::new(1));
    let barrier = Arc::new(Barrier::new(config.lanes + 1));
    let started = Instant::now();
    let mut handles = Vec::with_capacity(config.lanes);
    for lane in 0..config.lanes {
        let table = Arc::clone(&table);
        let next_sequence = Arc::clone(&next_sequence);
        let barrier = Arc::clone(&barrier);
        let ops = config.ops_per_lane;
        let records = config.records;
        let hot_percent = config.hot_percent;
        let pin = config.pin;
        let cpu = config.ingress_cpus[lane];
        handles.push(thread::spawn(move || -> io::Result<ThreadStats> {
            if pin {
                pin_current(cpu)?;
            }
            let before = context_switches();
            let mut random = 0x9e37_79b9_7f4a_7c15 ^ lane as u64;
            let mut checksum = 0u64;
            barrier.wait();
            for _ in 0..ops {
                let record = next_record(&mut random, records, hot_percent);
                let sequence = next_sequence.fetch_add(1, Ordering::Relaxed);
                let predecessor = table[record].swap(sequence, Ordering::AcqRel);
                checksum ^= predecessor.wrapping_add(record as u64);
            }
            let after = context_switches();
            Ok(ThreadStats {
                ops,
                checksum,
                cpu: current_cpu(),
                voluntary: after.0.saturating_sub(before.0),
                involuntary: after.1.saturating_sub(before.1),
                ..ThreadStats::default()
            })
        }));
    }
    barrier.wait();
    let run_started = Instant::now();
    let mut aggregate = ThreadStats::default();
    for (lane, handle) in handles.into_iter().enumerate() {
        let stats = handle
            .join()
            .map_err(|_| io::Error::other("atomic worker panicked"))??;
        println!(
            "zcorder-bench-worker: mode=global-atomic lane={lane} cpu={} ops={} context_switches={}",
            stats.cpu,
            stats.ops,
            stats.voluntary + stats.involuntary
        );
        aggregate.ops += stats.ops;
        aggregate.checksum ^= stats.checksum;
        aggregate.voluntary += stats.voluntary;
        aggregate.involuntary += stats.involuntary;
    }
    let wall = run_started.elapsed();
    let _total_setup = started.elapsed();
    Ok(RunResult {
        wall,
        logical_ops: aggregate.ops,
        spins: 0,
        checksum: aggregate.checksum,
        voluntary: aggregate.voluntary,
        involuntary: aggregate.involuntary,
    })
}

fn run_local(config: &Config) -> io::Result<RunResult> {
    print_topology(config, "lane-local");
    let barrier = Arc::new(Barrier::new(config.lanes + 1));
    let mut handles = Vec::with_capacity(config.lanes);
    let records_per_lane = config.records.div_ceil(config.lanes).max(1);
    for lane in 0..config.lanes {
        let barrier = Arc::clone(&barrier);
        let ops = config.ops_per_lane;
        let pin = config.pin;
        let cpu = config.ingress_cpus[lane];
        handles.push(thread::spawn(move || -> io::Result<ThreadStats> {
            if pin {
                pin_current(cpu)?;
            }
            let before = context_switches();
            let mut latest = vec![0u64; records_per_lane];
            let mut random = 0xd1b5_4a32_d192_ed03 ^ lane as u64;
            let mut checksum = 0u64;
            barrier.wait();
            for sequence in 1..=ops {
                let record = next_random(&mut random) as usize % records_per_lane;
                let previous = latest[record];
                latest[record] = sequence;
                checksum ^= previous.wrapping_add(record as u64);
            }
            let after = context_switches();
            Ok(ThreadStats {
                ops,
                checksum,
                cpu: current_cpu(),
                voluntary: after.0.saturating_sub(before.0),
                involuntary: after.1.saturating_sub(before.1),
                ..ThreadStats::default()
            })
        }));
    }
    barrier.wait();
    let started = Instant::now();
    let mut aggregate = ThreadStats::default();
    for (lane, handle) in handles.into_iter().enumerate() {
        let stats = handle
            .join()
            .map_err(|_| io::Error::other("lane-local worker panicked"))??;
        println!(
            "zcorder-bench-worker: mode=lane-local lane={lane} cpu={} ops={} context_switches={}",
            stats.cpu,
            stats.ops,
            stats.voluntary + stats.involuntary
        );
        aggregate.ops += stats.ops;
        aggregate.checksum ^= stats.checksum;
        aggregate.voluntary += stats.voluntary;
        aggregate.involuntary += stats.involuntary;
    }
    Ok(RunResult {
        wall: started.elapsed(),
        logical_ops: aggregate.ops,
        spins: 0,
        checksum: aggregate.checksum,
        voluntary: aggregate.voluntary,
        involuntary: aggregate.involuntary,
    })
}

fn matrix_index(ingress: usize, owner: usize, lanes: usize) -> usize {
    ingress * lanes + owner
}

fn push_wait<T: Copy + Default>(ring: &SpscRing<T>, mut value: T, spins: &mut u64) {
    loop {
        match ring.try_push(value) {
            Ok(()) => return,
            Err(returned) => value = returned,
        }
        *spins += 1;
        spin_loop();
    }
}

fn push_slice_wait<T: Copy + Default>(ring: &SpscRing<T>, values: &[T], spins: &mut u64) {
    let mut offset = 0usize;
    while offset < values.len() {
        let pushed = ring.try_push_slice(&values[offset..]);
        if pushed == 0 {
            *spins += 1;
            spin_loop();
        } else {
            offset += pushed;
        }
    }
}

fn publish_wait<T: Copy + Default>(
    ring: &SequenceRing<T>,
    sequence: u64,
    mut value: T,
    spins: &mut u64,
) {
    loop {
        match ring.try_publish(sequence, value) {
            Ok(()) => return,
            Err(returned) => value = returned,
        }
        *spins += 1;
        spin_loop();
    }
}

fn run_owner(config: &Config) -> io::Result<RunResult> {
    print_topology(config, "owner-matrix");
    let requests = Arc::new(
        (0..config.lanes * config.lanes)
            .map(|_| SpscRing::<OrderDesc>::new(config.ring_entries))
            .collect::<io::Result<Vec<_>>>()?,
    );
    let releases = Arc::new(
        (0..config.lanes * config.lanes)
            .map(|_| SpscRing::<ReleaseDesc>::new(config.ring_entries))
            .collect::<io::Result<Vec<_>>>()?,
    );
    let barrier = Arc::new(Barrier::new(config.lanes * 2 + 1));
    let records_per_owner = config.records.div_ceil(config.lanes).max(1);
    let mut owner_handles = Vec::with_capacity(config.lanes);
    for owner in 0..config.lanes {
        let requests = Arc::clone(&requests);
        let releases = Arc::clone(&releases);
        let barrier = Arc::clone(&barrier);
        let lanes = config.lanes;
        let pin = config.pin;
        let cpu = config.owner_cpus[owner];
        owner_handles.push(thread::spawn(move || -> io::Result<ThreadStats> {
            if pin {
                pin_current(cpu)?;
            }
            let before = context_switches();
            let mut latest = vec![0u64; records_per_owner];
            let mut barriers = vec![false; lanes];
            let mut barrier_count = 0usize;
            let mut cursor = 0usize;
            let mut stats = ThreadStats::default();
            barrier.wait();
            while barrier_count != lanes {
                let ingress = cursor;
                cursor = (cursor + 1) % lanes;
                let ring = &requests[matrix_index(ingress, owner, lanes)];
                let Some(desc) = ring.try_pop() else {
                    stats.spins += 1;
                    spin_loop();
                    continue;
                };
                if desc.flags & DESC_F_BARRIER != 0 {
                    if !barriers[ingress] {
                        barriers[ingress] = true;
                        barrier_count += 1;
                    }
                    continue;
                }
                if desc.record as usize % lanes != owner || desc.ingress_lane as usize != ingress {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "owner-matrix descriptor topology mismatch",
                    ));
                }
                let local_record = desc.record as usize / lanes;
                let previous = latest[local_record];
                latest[local_record] = desc.sequence;
                stats.checksum ^= previous.wrapping_add(desc.record);
                stats.ops += 1;
                push_wait(
                    &releases[matrix_index(ingress, owner, lanes)],
                    ReleaseDesc {
                        sequence: desc.sequence,
                        payload_slot: desc.payload_slot,
                        generation: desc.generation,
                    },
                    &mut stats.spins,
                );
            }
            let after = context_switches();
            stats.cpu = current_cpu();
            stats.voluntary = after.0.saturating_sub(before.0);
            stats.involuntary = after.1.saturating_sub(before.1);
            Ok(stats)
        }));
    }

    let mut ingress_handles = Vec::with_capacity(config.lanes);
    for ingress in 0..config.lanes {
        let requests = Arc::clone(&requests);
        let releases = Arc::clone(&releases);
        let barrier = Arc::clone(&barrier);
        let lanes = config.lanes;
        let ops = config.ops_per_lane;
        let records = config.records;
        let hot_percent = config.hot_percent;
        let window = config.window;
        let pin = config.pin;
        let cpu = config.ingress_cpus[ingress];
        ingress_handles.push(thread::spawn(move || -> io::Result<ThreadStats> {
            if pin {
                pin_current(cpu)?;
            }
            let before = context_switches();
            let mut free_slots = (0..window as u32).rev().collect::<Vec<_>>();
            let mut generations = vec![1u32; window];
            let mut random = 0xa076_1d64_78bd_642f ^ ingress as u64;
            let mut released = 0u64;
            let mut release_cursor = 0usize;
            let mut stats = ThreadStats::default();
            barrier.wait();
            for sequence in 1..=ops {
                while free_slots.is_empty() {
                    let owner = release_cursor;
                    release_cursor = (release_cursor + 1) % lanes;
                    if let Some(release) = releases[matrix_index(ingress, owner, lanes)].try_pop() {
                        let slot = release.payload_slot as usize;
                        if slot >= window || generations[slot] != release.generation {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "owner-matrix stale payload release",
                            ));
                        }
                        generations[slot] = generations[slot].wrapping_add(1).max(1);
                        free_slots.push(release.payload_slot);
                        released += 1;
                    } else {
                        stats.spins += 1;
                        spin_loop();
                    }
                }
                let slot = free_slots.pop().expect("free slot checked");
                let record = next_record(&mut random, records, hot_percent);
                let owner = record % lanes;
                push_wait(
                    &requests[matrix_index(ingress, owner, lanes)],
                    OrderDesc {
                        record: record as u64,
                        sequence,
                        payload_slot: slot,
                        generation: generations[slot as usize],
                        ingress_lane: ingress as u16,
                        flags: 0,
                        _reserved: 0,
                    },
                    &mut stats.spins,
                );
            }
            for owner in 0..lanes {
                push_wait(
                    &requests[matrix_index(ingress, owner, lanes)],
                    OrderDesc {
                        ingress_lane: ingress as u16,
                        flags: DESC_F_BARRIER,
                        ..OrderDesc::default()
                    },
                    &mut stats.spins,
                );
            }
            while released != ops {
                let owner = release_cursor;
                release_cursor = (release_cursor + 1) % lanes;
                if let Some(release) = releases[matrix_index(ingress, owner, lanes)].try_pop() {
                    let slot = release.payload_slot as usize;
                    if slot >= window || generations[slot] != release.generation {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "owner-matrix stale terminal payload release",
                        ));
                    }
                    generations[slot] = generations[slot].wrapping_add(1).max(1);
                    free_slots.push(release.payload_slot);
                    released += 1;
                    stats.checksum ^= release.sequence.rotate_left(owner as u32);
                } else {
                    stats.spins += 1;
                    spin_loop();
                }
            }
            if free_slots.len() != window {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "owner-matrix leaked payload slots",
                ));
            }
            let after = context_switches();
            stats.ops = ops;
            stats.cpu = current_cpu();
            stats.voluntary = after.0.saturating_sub(before.0);
            stats.involuntary = after.1.saturating_sub(before.1);
            Ok(stats)
        }));
    }

    barrier.wait();
    let started = Instant::now();
    let mut aggregate = ThreadStats::default();
    for (lane, handle) in ingress_handles.into_iter().enumerate() {
        let stats = handle
            .join()
            .map_err(|_| io::Error::other("owner ingress worker panicked"))??;
        println!(
            "zcorder-bench-worker: mode=owner-ingress lane={lane} cpu={} ops={} spins={} context_switches={}",
            stats.cpu,
            stats.ops,
            stats.spins,
            stats.voluntary + stats.involuntary
        );
        aggregate.ops += stats.ops;
        aggregate.spins += stats.spins;
        aggregate.checksum ^= stats.checksum;
        aggregate.voluntary += stats.voluntary;
        aggregate.involuntary += stats.involuntary;
    }
    let logical_ops = aggregate.ops;
    let mut owner_ops = 0u64;
    for (lane, handle) in owner_handles.into_iter().enumerate() {
        let stats = handle
            .join()
            .map_err(|_| io::Error::other("owner ordering worker panicked"))??;
        println!(
            "zcorder-bench-worker: mode=owner-order lane={lane} cpu={} ops={} spins={} context_switches={}",
            stats.cpu,
            stats.ops,
            stats.spins,
            stats.voluntary + stats.involuntary
        );
        owner_ops = owner_ops
            .checked_add(stats.ops)
            .ok_or_else(|| io::Error::other("owner operation count overflow"))?;
        aggregate.spins += stats.spins;
        aggregate.checksum ^= stats.checksum;
        aggregate.voluntary += stats.voluntary;
        aggregate.involuntary += stats.involuntary;
    }
    if owner_ops != logical_ops {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("owner-matrix processed {owner_ops} operations, expected {logical_ops}"),
        ));
    }
    Ok(RunResult {
        wall: started.elapsed(),
        logical_ops: aggregate.ops,
        spins: aggregate.spins,
        checksum: aggregate.checksum,
        voluntary: aggregate.voluntary,
        involuntary: aggregate.involuntary,
    })
}

fn main() -> io::Result<()> {
    let config = parse_config()?;
    let run = |label: &str, result: io::Result<RunResult>| -> io::Result<()> {
        finish_result(label, result?);
        Ok(())
    };
    match config.mode {
        Mode::Atomic => run("global-atomic", run_atomic(&config)),
        Mode::Local => run("lane-local", run_local(&config)),
        Mode::Owner => run("owner-matrix", run_owner(&config)),
        Mode::Dispatch => run("ordered-dispatch", run_ordered_dispatch(&config)),
        Mode::All => {
            run("global-atomic", run_atomic(&config))?;
            run("lane-local", run_local(&config))?;
            run("owner-matrix", run_owner(&config))?;
            run("ordered-dispatch", run_ordered_dispatch(&config))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spsc_ring_wraps_without_reordering() {
        let ring = SpscRing::<u64>::new(4).unwrap();
        for cycle in 0..32u64 {
            for offset in 0..4u64 {
                ring.try_push(cycle * 4 + offset).unwrap();
            }
            assert!(ring.try_push(u64::MAX).is_err());
            for offset in 0..4u64 {
                assert_eq!(ring.try_pop(), Some(cycle * 4 + offset));
            }
            assert_eq!(ring.try_pop(), None);
        }
    }

    #[test]
    fn sequence_ring_reorders_publishers_and_wraps() {
        let ring = SequenceRing::<u64>::new(4).unwrap();
        assert!(ring.try_publish(2, 20).is_ok());
        assert_eq!(ring.try_consume(1), None);
        assert!(ring.try_publish(1, 10).is_ok());
        assert_eq!(ring.try_consume(1), Some(10));
        assert_eq!(ring.try_consume(2), Some(20));
        assert!(ring.try_publish(5, 50).is_ok());
        assert_eq!(ring.try_consume(5), Some(50));
    }
}
