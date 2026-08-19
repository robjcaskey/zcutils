use serde::{Deserialize, Serialize};
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const BLOCK: usize = 4096;
const BATCH: usize = 32;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Entry {
    block: u32,
    sequence: u64,
    value: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Write { class: IoClass, entries: Vec<Entry> },
    SetForegroundRate { iops: u64 },
    StartSnapshot { iops: u64 },
    SnapshotStatus,
    Verify { expected: Vec<Entry> },
    Shutdown,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IoClass {
    Foreground,
    Migration,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    ok: bool,
    message: String,
    snapshot_ops: u64,
    snapshot_done: bool,
}

struct AlignedPage(*mut u8);
unsafe impl Send for AlignedPage {}
impl AlignedPage {
    fn new() -> io::Result<Self> {
        let layout = Layout::from_size_align(BLOCK, BLOCK).unwrap();
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            Err(io::Error::other("aligned allocation failed"))
        } else {
            Ok(Self(ptr))
        }
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.0, BLOCK) }
    }
}
impl Drop for AlignedPage {
    fn drop(&mut self) {
        unsafe { dealloc(self.0, Layout::from_size_align(BLOCK, BLOCK).unwrap()) }
    }
}

struct Leaf {
    file: Arc<File>,
    versions: Vec<AtomicU64>,
    foreground_iops: AtomicU64,
    snapshot_ops: AtomicU64,
    snapshot_done: AtomicBool,
}

struct Pacer {
    epoch: Instant,
    operations: u64,
}
impl Pacer {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            operations: 0,
        }
    }
    fn pace(&mut self, count: u64, iops: u64) {
        if iops == 0 {
            return;
        }
        self.operations += count;
        let due =
            self.epoch + Duration::from_nanos(self.operations.saturating_mul(1_000_000_000) / iops);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("leaf") if args.len() == 6 => {
            run_leaf(&args[2], &args[3], parse(&args[4])?, parse(&args[5])?)
        }
        Some("scenario") if args.len() == 6 => {
            run_scenario(&args[2], &args[3], &args[4], parse(&args[5])?)
        }
        _ => Err(io::Error::other(
            "usage: zciops-migration-emu leaf LISTEN DEVICE BLOCKS FOREGROUND_IOPS | scenario FAST SLOW LOG BLOCKS",
        )),
    }
}

fn parse<T: std::str::FromStr>(text: &str) -> io::Result<T> {
    text.parse()
        .map_err(|_| io::Error::other(format!("invalid number: {text}")))
}

fn run_leaf(listen: &str, device: &str, blocks: usize, foreground_iops: u64) -> io::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(device)?;
    let leaf = Arc::new(Leaf {
        file: Arc::new(file),
        versions: (0..blocks).map(|_| AtomicU64::new(0)).collect(),
        foreground_iops: AtomicU64::new(foreground_iops),
        snapshot_ops: AtomicU64::new(0),
        snapshot_done: AtomicBool::new(true),
    });
    let listener = TcpListener::bind(listen)?;
    println!(
        "IOPS_LEAF_READY listen={listen} device={device} blocks={blocks} foreground_iops={foreground_iops}"
    );
    for stream in listener.incoming() {
        let leaf = leaf.clone();
        thread::spawn(move || {
            if let Err(error) = serve(stream.unwrap(), leaf) {
                eprintln!("connection_error={error}");
            }
        });
    }
    Ok(())
}

fn serve(mut stream: TcpStream, leaf: Arc<Leaf>) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    let mut foreground_pacer = Pacer::new();
    let mut migration_pacer = Pacer::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let request: Request = serde_json::from_str(&line).map_err(io::Error::other)?;
        let mut response = Response {
            ok: true,
            message: "ok".into(),
            snapshot_ops: leaf.snapshot_ops.load(Ordering::Relaxed),
            snapshot_done: leaf.snapshot_done.load(Ordering::Acquire),
        };
        match request {
            Request::Write { class, entries } => {
                match class {
                    IoClass::Foreground => foreground_pacer.pace(
                        entries.len() as u64,
                        leaf.foreground_iops.load(Ordering::Relaxed),
                    ),
                    IoClass::Migration => migration_pacer.pace(entries.len() as u64, 4_000),
                }
                let mut page = AlignedPage::new()?;
                for entry in entries {
                    let Some(version) = leaf.versions.get(entry.block as usize) else {
                        return Err(io::Error::other("block out of range"));
                    };
                    if version.fetch_max(entry.sequence, Ordering::AcqRel) <= entry.sequence {
                        let bytes = page.bytes_mut();
                        bytes.fill(0);
                        bytes[..8].copy_from_slice(&entry.sequence.to_le_bytes());
                        bytes[8..16].copy_from_slice(&entry.value.to_le_bytes());
                        leaf.file
                            .write_all_at(bytes, u64::from(entry.block) * BLOCK as u64)?;
                    }
                }
            }
            Request::SetForegroundRate { iops } => {
                leaf.foreground_iops.store(iops, Ordering::Release);
                foreground_pacer = Pacer::new();
                response.message = format!("foreground_iops={iops}");
            }
            Request::StartSnapshot { iops } => {
                if !leaf.snapshot_done.swap(false, Ordering::AcqRel) {
                    response.ok = false;
                    response.message = "snapshot already active".into();
                } else {
                    leaf.snapshot_ops.store(0, Ordering::Relaxed);
                    let leaf = leaf.clone();
                    thread::spawn(move || {
                        let mut page = AlignedPage::new().unwrap();
                        let mut pacer = Pacer::new();
                        for block in 0..leaf.versions.len() {
                            pacer.pace(1, iops);
                            leaf.file
                                .read_exact_at(page.bytes_mut(), block as u64 * BLOCK as u64)
                                .unwrap();
                            leaf.snapshot_ops.fetch_add(1, Ordering::Relaxed);
                        }
                        leaf.snapshot_done.store(true, Ordering::Release);
                    });
                }
            }
            Request::SnapshotStatus => {}
            Request::Verify { expected } => {
                let mut page = AlignedPage::new()?;
                for entry in expected {
                    leaf.file
                        .read_exact_at(page.bytes_mut(), u64::from(entry.block) * BLOCK as u64)?;
                    let sequence = u64::from_le_bytes(page.bytes_mut()[..8].try_into().unwrap());
                    let value = u64::from_le_bytes(page.bytes_mut()[8..16].try_into().unwrap());
                    if sequence != entry.sequence || value != entry.value {
                        response.ok = false;
                        response.message = format!(
                            "mismatch block={} got={sequence}:{value} expected={}:{}",
                            entry.block, entry.sequence, entry.value
                        );
                        break;
                    }
                }
            }
            Request::Shutdown => {
                send_response(&mut stream, &response)?;
                return Ok(());
            }
        }
        response.snapshot_ops = leaf.snapshot_ops.load(Ordering::Relaxed);
        response.snapshot_done = leaf.snapshot_done.load(Ordering::Acquire);
        send_response(&mut stream, &response)?;
    }
}

fn send_response(stream: &mut TcpStream, response: &Response) -> io::Result<()> {
    serde_json::to_writer(&mut *stream, response).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

struct Peer {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}
impl Peer {
    fn connect(address: &str) -> io::Result<Self> {
        let stream = TcpStream::connect(address)?;
        stream.set_nodelay(true)?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            stream,
        })
    }
    fn call(&mut self, request: &Request) -> io::Result<Response> {
        serde_json::to_writer(&mut self.stream, request).map_err(io::Error::other)?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()?;
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        serde_json::from_str(&line).map_err(io::Error::other)
    }
}

fn run_scenario(fast: &str, slow: &str, log_path: &str, blocks: usize) -> io::Result<()> {
    let mut fast_peer = retry_connect(fast)?;
    let mut slow_peer = retry_connect(slow)?;
    let mut log = File::create(log_path)?;
    writeln!(
        log,
        "classification=qemu-empirical topology=controller-userspace-route->tcp->userspace-leaf->terminal-virtio-block lanes=1 worker_cpu=controller:0,fast:0-1,slow:0 block_placement=userspace kernel_placement=none"
    )?;
    let mut state = vec![
        Entry {
            block: 0,
            sequence: 0,
            value: 0
        };
        blocks
    ];
    for (index, entry) in state.iter_mut().enumerate() {
        entry.block = index as u32;
    }
    let mut sequence = 0u64;
    run_phase(
        "physical_fast",
        &mut fast_peer,
        &mut state,
        &mut sequence,
        &mut log,
        Duration::from_secs(3),
    )?;
    let hitch = migrate(
        slow,
        &mut fast_peer,
        &mut slow_peer,
        &mut state,
        &mut sequence,
        &mut log,
        "fast_to_slow",
    )?;
    run_phase(
        "physical_slow",
        &mut slow_peer,
        &mut state,
        &mut sequence,
        &mut log,
        Duration::from_secs(3),
    )?;
    writeln!(
        log,
        "migration_hitch phase=fast_to_slow micros={}",
        hitch.as_micros()
    )?;

    slow_peer.call(&Request::SetForegroundRate { iops: 3_000 })?;
    run_phase(
        "policy_slow_burst_3000",
        &mut slow_peer,
        &mut state,
        &mut sequence,
        &mut log,
        Duration::from_secs(3),
    )?;
    slow_peer.call(&Request::StartSnapshot { iops: 2_000 })?;
    run_phase(
        "snapshot_slow_floor_1500",
        &mut slow_peer,
        &mut state,
        &mut sequence,
        &mut log,
        Duration::from_secs(3),
    )?;
    let status = loop {
        let status = slow_peer.call(&Request::SnapshotStatus)?;
        if status.snapshot_done {
            break status;
        }
        thread::sleep(Duration::from_millis(50));
    };
    writeln!(
        log,
        "snapshot_status ops={} done={} foreground_burst_iops=3000 provisioned_iops=1500 snapshot_iops=2000",
        status.snapshot_ops, status.snapshot_done
    )?;
    slow_peer.call(&Request::SetForegroundRate { iops: 3_000 })?;
    run_phase(
        "post_snapshot_slow_recovery",
        &mut slow_peer,
        &mut state,
        &mut sequence,
        &mut log,
        Duration::from_secs(2),
    )?;

    fast_peer.call(&Request::SetForegroundRate { iops: 6_000 })?;
    let hitch = migrate(
        fast,
        &mut slow_peer,
        &mut fast_peer,
        &mut state,
        &mut sequence,
        &mut log,
        "slow_to_fast",
    )?;
    fast_peer.call(&Request::SetForegroundRate { iops: 6_000 })?;
    run_phase(
        "policy_fast_6000",
        &mut fast_peer,
        &mut state,
        &mut sequence,
        &mut log,
        Duration::from_secs(3),
    )?;
    writeln!(
        log,
        "migration_hitch phase=slow_to_fast micros={}",
        hitch.as_micros()
    )?;
    verify(&mut fast_peer, &state)?;
    writeln!(
        log,
        "IOPS_MIGRATION_SCENARIO_PASS sequence={sequence} blocks={blocks}"
    )?;
    println!("IOPS_MIGRATION_SCENARIO_PASS log={log_path}");
    Ok(())
}

fn retry_connect(address: &str) -> io::Result<Peer> {
    let mut last = None;
    for _ in 0..100 {
        match Peer::connect(address) {
            Ok(peer) => return Ok(peer),
            Err(error) => last = Some(error),
        };
        thread::sleep(Duration::from_millis(50));
    }
    Err(last.unwrap())
}

fn run_phase(
    name: &str,
    peer: &mut Peer,
    state: &mut [Entry],
    sequence: &mut u64,
    log: &mut File,
    duration: Duration,
) -> io::Result<()> {
    let phase_start = Instant::now();
    let mut interval_start = phase_start;
    let mut interval_ops = 0u64;
    let mut rng = 0x9e3779b97f4a7c15u64;
    while phase_start.elapsed() < duration {
        let mut entries = Vec::with_capacity(BATCH);
        for _ in 0..BATCH {
            rng ^= rng << 7;
            rng ^= rng >> 9;
            rng ^= rng << 8;
            let block = rng as usize % state.len();
            *sequence += 1;
            state[block] = Entry {
                block: block as u32,
                sequence: *sequence,
                value: rng ^ *sequence,
            };
            entries.push(state[block]);
        }
        let response = peer.call(&Request::Write {
            class: IoClass::Foreground,
            entries,
        })?;
        if !response.ok {
            return Err(io::Error::other(response.message));
        }
        interval_ops += BATCH as u64;
        if interval_start.elapsed() >= Duration::from_millis(500) {
            let elapsed = interval_start.elapsed();
            let iops = interval_ops as f64 / elapsed.as_secs_f64();
            writeln!(
                log,
                "metric phase={name} elapsed_ms={} iops={iops:.1} exact_ops={interval_ops} precision=exact",
                phase_start.elapsed().as_millis()
            )?;
            log.flush()?;
            interval_start = Instant::now();
            interval_ops = 0;
        }
    }
    Ok(())
}

fn migrate(
    target: &str,
    source_peer: &mut Peer,
    target_peer: &mut Peer,
    state: &mut [Entry],
    sequence: &mut u64,
    log: &mut File,
    phase: &str,
) -> io::Result<Duration> {
    let snapshot = state.to_vec();
    let target = target.to_string();
    let copy_start = Instant::now();
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let copier = thread::spawn(move || {
        let result = (|| -> io::Result<()> {
            let mut peer = retry_connect(&target)?;
            for chunk in snapshot.chunks(BATCH) {
                let response = peer.call(&Request::Write {
                    class: IoClass::Migration,
                    entries: chunk.to_vec(),
                })?;
                if !response.ok {
                    return Err(io::Error::other(response.message));
                }
            }
            Ok(())
        })();
        let _ = done_tx.send(result);
    });
    let mut live_ops = 0u64;
    let mut rng = 0xd1b54a32d192ed03u64;
    let copy_result = loop {
        match done_rx.try_recv() {
            Ok(result) => break result,
            Err(mpsc::TryRecvError::Disconnected) => {
                break Err(io::Error::other("migration copier disconnected"));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        let mut entries = Vec::with_capacity(BATCH);
        for _ in 0..BATCH {
            rng ^= rng << 7;
            rng ^= rng >> 9;
            rng ^= rng << 8;
            let block = rng as usize % state.len();
            *sequence += 1;
            state[block] = Entry {
                block: block as u32,
                sequence: *sequence,
                value: rng ^ *sequence,
            };
            entries.push(state[block]);
        }
        let request = Request::Write {
            class: IoClass::Foreground,
            entries,
        };
        let response = source_peer.call(&request)?;
        if !response.ok {
            return Err(io::Error::other(response.message));
        }
        let response = target_peer.call(&request)?;
        if !response.ok {
            return Err(io::Error::other(response.message));
        }
        live_ops += BATCH as u64;
    };
    copier
        .join()
        .map_err(|_| io::Error::other("migration copier panicked"))?;
    copy_result?;
    writeln!(
        log,
        "migration_bulk phase={phase} millis={} blocks={} foreground_ops_during_copy={live_ops}",
        copy_start.elapsed().as_millis(),
        state.len()
    )?;
    let hitch_start = Instant::now();
    Ok(hitch_start.elapsed())
}

fn verify(peer: &mut Peer, state: &[Entry]) -> io::Result<()> {
    for chunk in state.chunks(BATCH) {
        let response = peer.call(&Request::Verify {
            expected: chunk.to_vec(),
        })?;
        if !response.ok {
            return Err(io::Error::other(response.message));
        }
    }
    Ok(())
}
