//! Opt-in local custody for the existing userspace RAID mirror sender.
//! No block-edge placement, new network proxy, or per-I/O global state.

use crate::client_wal_commitment::{Commitment, DurableReceipt, Policy};
use crate::client_wal_journal::{Journal, MAX_BATCH_RECORDS, Record};
use crate::persistent_wal::BackingIoMode;
use crate::*;
use serde::Deserialize;

pub(super) const CONFIG_ENV: &str = "URING_PLAY_RAID_MIRROR_CLIENT_WAL_CONFIG";
pub(super) const REQUIRE_DURABLE: u32 = 1 << 1;
const DURABLE_ACK: u32 = 1;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub policy: Policy,
    pub journal_directory: PathBuf,
    pub capacity_bytes_per_lane: u64,
    #[serde(default = "default_direct")]
    pub direct: bool,
    /// First connection carries data/middle receipts; second carries only
    /// final-replica receipts from the same userspace serial hop.
    #[serde(default)]
    pub serial_hop: bool,
    /// Cold, fenced repair adapter. The initial adapter owns one volume lane;
    /// multi-lane recovery needs a coordinated volume fence, not independent
    /// snapshots racing writes on the other lanes.
    #[serde(default)]
    pub repair: Option<client_wal_repair::Config>,
    #[serde(default)]
    pub rdma: Option<rdma_custody::Config>,
}

fn default_direct() -> bool {
    true
}

/// Admission-time checks only; never consulted for individual requests.
pub(super) fn topology_preflight(
    label: &str,
    lanes: usize,
    workers: usize,
    arena_bytes: usize,
) -> io::Result<()> {
    zcwal_extent_perf_warnings(label, lanes, 1, workers)?;
    if !env_enabled_or("URING_PLAY_HUGETLB", false) {
        zc_topology_issue(
            label,
            "URING_PLAY_HUGETLB=1 is required for representative custody arena measurements",
        )?;
    }
    let (total, free, size) = hugepage_meminfo()?;
    let pages = arena_bytes.div_ceil(size.max(1));
    if total == 0 || free < pages {
        zc_topology_issue(
            label,
            format!("insufficient hugetlb headroom: free={free} needed={pages} total={total}"),
        )?;
    }
    if let Some(limit) = memlock_rlimit_bytes()? {
        if limit < arena_bytes as u64 {
            zc_topology_issue(
                label,
                format!("insufficient memlock: limit={limit} arena_bytes={arena_bytes}"),
            )?;
        }
    }
    println!(
        "{label}-execution-contract: placement_owner=userspace-raid kernel_placement=false terminal_io=borrowed-aligned-wal-writev-and-sync io_uring=false block_hctx=not-applicable TCP_payload_kernel_copy=not-eliminated"
    );
    for worker in 0..workers {
        println!(
            "{label}-worker-map: worker={worker} lane={} cpu={} role={}",
            worker % lanes.max(1),
            affinity_target_cpu(worker)
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unpinned".into()),
            if worker < lanes {
                "ingress-or-source"
            } else {
                "forward"
            }
        );
    }
    Ok(())
}

impl Config {
    pub fn from_env() -> io::Result<Option<Self>> {
        let Some(path) = env::var_os(CONFIG_ENV) else {
            return Ok(None);
        };
        let config: Self = serde_json::from_slice(&fs::read(path)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        Commitment::new(config.policy)?;
        if !config.policy.count_client_local_wal {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "client WAL configuration must explicitly opt in; otherwise omit the configuration",
            ));
        }
        Ok(Some(config))
    }

    fn open_lane(&self, lane: usize) -> io::Result<(Journal, Commitment)> {
        fs::create_dir_all(&self.journal_directory)?;
        let mut policy = self.policy;
        policy.scope.lane =
            u32::try_from(lane).map_err(|_| io::Error::other("lane exceeds u32"))?;
        let path = self.journal_directory.join(format!(
            "volume-{}-log-{}-epoch-{}-lane-{lane}.wal",
            policy.scope.volume, policy.scope.log, policy.scope.writer_epoch
        ));
        let journal = Journal::open(
            &path,
            self.capacity_bytes_per_lane,
            policy.scope,
            policy.local,
            if self.direct {
                BackingIoMode::Direct
            } else {
                BackingIoMode::Buffered
            },
        )?;
        if journal.submitted_hwm() != 0 {
            return Err(io::Error::other(
                "retained client WAL exists: repair/resume its sequence space; refusing a benchmark reset",
            ));
        }
        Ok((journal, Commitment::new(policy)?))
    }
}

/// Caller invokes this only after terminal.sync() succeeds. Older receivers
/// leave this reserved field zero, which the opt-in client refuses to count.
pub(super) fn encode_durable_ack(
    header: ZcWalExtentHeader,
) -> io::Result<[u8; ZC_WAL_ACK_HEADER_LEN]> {
    let mut ack = zcraid_mirror_ack_for_header(header)?.encode();
    if header.flags & REQUIRE_DURABLE != 0 {
        zcwal_put_u32(&mut ack, 0x0c, DURABLE_ACK);
    }
    Ok(ack)
}

struct Replies {
    bytes: [[u8; ZC_WAL_ACK_HEADER_LEN]; 2],
    used: [usize; 2],
    next: [usize; 2],
}

impl Replies {
    fn new() -> Self {
        Self {
            bytes: [[0; ZC_WAL_ACK_HEADER_LEN]; 2],
            used: [0; 2],
            next: [0; 2],
        }
    }

    /// Fair, incremental reads: a trickled/slow ACK on one leg never stops
    /// another leg's complete durable receipt from advancing the zipper.
    fn progress(
        &mut self,
        streams: &mut [(usize, TcpStream)],
        tracker: &mut Commitment,
        lane: usize,
        lane_count: usize,
        extent_bytes: usize,
        wait: bool,
    ) -> io::Result<bool> {
        let mut advanced = false;
        for slot in 0..2 {
            if self.next[slot] as u64 == tracker.progress().submitted {
                continue;
            }
            let stream = &streams[slot].1;
            // Per-call nonblocking receive leaves batched TCP writes blocking.
            let rc = unsafe {
                libc::recv(
                    stream.as_raw_fd(),
                    self.bytes[slot][self.used[slot]..].as_mut_ptr().cast(),
                    ZC_WAL_ACK_HEADER_LEN - self.used[slot],
                    libc::MSG_DONTWAIT,
                )
            };
            if rc < 0 {
                let error = io::Error::last_os_error();
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    continue;
                }
                return Err(error);
            }
            if rc == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "mirror replica lost before durable ACK",
                ));
            }
            self.used[slot] += rc as usize;
            if self.used[slot] != ZC_WAL_ACK_HEADER_LEN {
                continue;
            }
            if zcwal_read_u32(&self.bytes[slot], 0x0c) != DURABLE_ACK {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "replica did not attest a persistent terminal drain; volatile/legacy ACK cannot count",
                ));
            }
            let ack = ZcWalAckHeader::decode(&self.bytes[slot])?;
            zcraid_mirror_verify_ack(
                ack,
                streams[slot].0,
                lane,
                lane_count,
                self.next[slot],
                extent_bytes,
                ZcRaidZlaneCoordMode::LaneOwner,
            )?;
            self.next[slot] += 1;
            self.used[slot] = 0;
            let policy = tracker.policy();
            let replica = policy.remote[slot];
            tracker.durable(DurableReceipt {
                scope: policy.scope,
                replica: replica.id,
                incarnation: replica.incarnation,
                through: self.next[slot] as u64,
            })?;
            advanced = true;
        }
        if !advanced && wait {
            let mut fds = [libc::pollfd {
                fd: -1,
                events: libc::POLLIN,
                revents: 0,
            }; 2];
            for (slot, (_, stream)) in streams.iter().enumerate() {
                if self.next[slot] as u64 != tracker.progress().submitted {
                    fds[slot].fd = stream.as_raw_fd();
                }
            }
            if fds.iter().all(|fd| fd.fd < 0) {
                return Ok(false);
            }
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, 1000) };
            if rc < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(advanced)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn tcp_send(
    worker: usize,
    lanes: Vec<usize>,
    branches: Arc<Vec<ZcRaidMirrorBranchTarget>>,
    lane_count: usize,
    extents_per_lane: usize,
    extent_bytes: usize,
    ack_window: usize,
    config: Config,
) -> io::Result<ZcWalExtentStats> {
    tcp_send_with_pattern(
        worker,
        lanes,
        branches,
        lane_count,
        extents_per_lane,
        extent_bytes,
        ack_window,
        config,
        SendPayloadPattern::from_env(extent_bytes)?,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn tcp_send_with_pattern(
    worker: usize,
    lanes: Vec<usize>,
    branches: Arc<Vec<ZcRaidMirrorBranchTarget>>,
    lane_count: usize,
    extents_per_lane: usize,
    extent_bytes: usize,
    ack_window: usize,
    config: Config,
    pattern: SendPayloadPattern,
) -> io::Result<ZcWalExtentStats> {
    if branches.len() != 2 {
        return Err(io::Error::other(
            "local custody currently requires exactly two remote branches",
        ));
    }
    if config.repair.is_some() && (!config.serial_hop || lane_count != 1 || lanes != [0]) {
        return Err(io::Error::other(
            "serial repair requires one fenced volume lane",
        ));
    }
    if config.rdma.is_some() && !config.serial_hop {
        return Err(io::Error::other(
            "RDMA client custody requires the serial userspace hop",
        ));
    }
    let affinity = maybe_pin_current_thread("zcraid-mirror-client-wal", worker);
    let tid = current_tid();
    let cpu_start = thread_cpu_time().unwrap_or_default();
    let switches = read_thread_context_switches(tid).unwrap_or_default();
    let start_cpu = current_cpu();
    let lane_total = lanes.len();
    let mut latency = LatencyHistogram::new();
    let huge = env_enabled_or("URING_PLAY_HUGETLB", false);
    let max_window = ack_window.max(1).min(extents_per_lane.max(1));
    let distinct_payloads = pattern.is_offset_dependent();
    let arena_bytes = extent_bytes
        .checked_mul(if distinct_payloads { max_window } else { 1 })
        .ok_or_else(|| io::Error::other("source arena size overflow"))?;
    let arena = if huge {
        FixedSendBuffers::new_hugetlb(1, arena_bytes)?
    } else {
        FixedSendBuffers::new(1, arena_bytes)?
    };
    let payload = unsafe { slice::from_raw_parts_mut(arena.ptr(0), arena_bytes) };
    pattern.fill(payload);
    let mut operation_wall = Duration::ZERO;
    let mut final_drain = Duration::ZERO;
    let mut early_windows = 0usize;
    let mut peak_retained = 0u64;
    let reservation = max_window
        .checked_mul(extent_bytes)
        .and_then(|bytes| bytes.checked_add(max_window.div_ceil(MAX_BATCH_RECORDS) * 4096))
        .and_then(|bytes| bytes.checked_mul(2))
        .ok_or_else(|| io::Error::other("client WAL window reservation overflow"))?
        as u64;
    if reservation > config.capacity_bytes_per_lane {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "client WAL capacity must cover a complete QD window plus wrap/metadata headroom",
        ));
    }
    for lane in lanes {
        let (mut journal, mut tracker) = config.open_lane(lane)?;
        let mut streams = Vec::with_capacity(2);
        for branch in branches.iter() {
            let port = tcp_bench_port(branch.base_port, lane)?;
            let stream =
                tcp_bench_connect_with_source_ip(&branch.addr, port, None, branch.source_ip)?;
            set_tcp_nodelay_from_env(&stream)?;
            set_tcp_bench_buffers(&stream);
            stream.set_write_timeout(Some(Duration::from_secs(30)))?;
            println!(
                "zcraid-mirror-client-wal-lane: lane={lane} worker={worker} cpu={} branch={} peer={} source_ip={:?} arena={} completion=local-persistent-plus-one-remote-persistent-or-both-remotes",
                affinity.target_cpu,
                branch.plan.branch,
                stream.peer_addr()?,
                branch.source_ip,
                arena.memory_policy
            );
            streams.push((branch.plan.branch, stream));
        }
        // This endpoint is dropped before the caller-owned source arena.
        let mut rdma = config
            .rdma
            .as_ref()
            .map(|rdma| unsafe {
                rdma_custody::Sender::connect(
                    &mut streams[0].1,
                    rdma,
                    lane,
                    max_window,
                    extent_bytes,
                    payload.as_ptr(),
                    payload.len(),
                    None,
                )
            })
            .transpose()?;
        let result = (|| -> io::Result<()> {
            let mut replies = Replies::new();
            let mut records = Vec::with_capacity(MAX_BATCH_RECORDS);
            let mut headers = vec![[0u8; ZC_WAL_EXTENT_HEADER_LEN]; max_window];
            let started = Instant::now();
            let mut seq = 0usize;
            let window = ack_window.max(1);
            while seq < extents_per_lane {
                let batch = window.min(extents_per_lane - seq);
                // Verification mode changes the original application arena only
                // after its previous synchronous file writes and NIC TX drain.
                // The benchmark fill mode keeps the original payload immutable.
                if distinct_payloads {
                    for (slot, data) in payload
                        .chunks_exact_mut(extent_bytes)
                        .take(batch)
                        .enumerate()
                    {
                        let (_, _, _, offset) = zcraid_mirror_extent_layout(
                            lane,
                            lane_count,
                            seq + slot,
                            extent_bytes,
                            ZcRaidZlaneCoordMode::LaneOwner,
                        )?;
                        pattern.fill_for_zcnblk(data, lane, offset);
                    }
                }
                let issued = Instant::now();
                let credit_deadline = Instant::now() + Duration::from_secs(30);
                // Remote payload credits and early durability ACKs have
                // different lifetimes. Never overwrite a NIC receive slot
                // still borrowed by the middle terminal or downstream NIC.
                if let Some(rdma) = rdma.as_ref() {
                    let pool_depth = rdma.remote_slots * max_window;
                    let needed = (seq + batch).saturating_sub(pool_depth) as u64;
                    while tracker.progress().remote_redundant < needed {
                        if Instant::now() >= credit_deadline {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "RDMA remote pool credit exhausted",
                            ));
                        }
                        replies.progress(
                            &mut streams,
                            &mut tracker,
                            lane,
                            lane_count,
                            extent_bytes,
                            true,
                        )?;
                    }
                }
                while journal.used_bytes() + reservation > journal.capacity_bytes() {
                    if Instant::now() >= credit_deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "client WAL full awaiting remote redundancy; retained data was not evicted",
                        ));
                    }
                    replies.progress(
                        &mut streams,
                        &mut tracker,
                        lane,
                        lane_count,
                        extent_bytes,
                        true,
                    )?;
                    let release = tracker
                        .progress()
                        .remote_redundant
                        .min(journal.durable_hwm());
                    if release > journal.released_hwm() {
                        journal.release_remote_prefix(&tracker.redundant_prefix())?;
                        if config.repair.is_some() {
                            eprintln!(
                                "client-wal-custody-progress: lane={lane} released_hwm={} acknowledged={} remote_redundant={}",
                                journal.released_hwm(),
                                tracker.progress().acknowledged,
                                tracker.progress().remote_redundant
                            );
                        }
                    }
                }
                // Metadata is batched; every payload IoSlice borrows the original
                // aligned arena. No second payload allocation or per-branch copy.
                for first in (seq..seq + batch).step_by(MAX_BATCH_RECORDS) {
                    let count = MAX_BATCH_RECORDS.min(seq + batch - first);
                    records.clear();
                    for index in first..first + count {
                        let (_, _, _, offset) = zcraid_mirror_extent_layout(
                            lane,
                            lane_count,
                            index,
                            extent_bytes,
                            ZcRaidZlaneCoordMode::LaneOwner,
                        )?;
                        records.push(Record {
                            logical_offset: offset,
                            length: extent_bytes as u64,
                        });
                    }
                    let mut payloads = [IoSlice::new(&[]); MAX_BATCH_RECORDS];
                    for (slot, part) in payloads[..count].iter_mut().enumerate() {
                        let offset = if distinct_payloads {
                            (first - seq + slot) * extent_bytes
                        } else {
                            0
                        };
                        *part = IoSlice::new(&payload[offset..offset + extent_bytes]);
                    }
                    tracker.submitted(journal.append(&records, &payloads[..count])?)?;
                }
                for (slot, (branch, stream)) in streams.iter_mut().enumerate() {
                    if config.serial_hop && slot != 0 {
                        continue;
                    }
                    if let Some(rdma) = rdma.as_mut() {
                        unsafe {
                            rdma.write_window(seq, batch, payload.as_ptr(), !distinct_payloads)?;
                        }
                    }
                    for (slot, bytes) in headers[..batch].iter_mut().enumerate() {
                        let mut header = zcraid_mirror_extent_header(
                            lane,
                            lane_count,
                            *branch,
                            seq + slot,
                            extent_bytes,
                            ZcRaidZlaneCoordMode::LaneOwner,
                        )?;
                        header.flags |= REQUIRE_DURABLE;
                        *bytes = header.encode();
                    }
                    for (chunk_index, chunk) in headers[..batch].chunks(512).enumerate() {
                        let mut iov = [IoSlice::new(&[]); 1024];
                        let mut used = 0;
                        for (slot, header) in chunk.iter().enumerate() {
                            iov[used] = IoSlice::new(header);
                            used += 1;
                            if rdma.is_none() {
                                let offset = if distinct_payloads {
                                    (chunk_index * 512 + slot) * extent_bytes
                                } else {
                                    0
                                };
                                iov[used] = IoSlice::new(&payload[offset..offset + extent_bytes]);
                                used += 1;
                            }
                        }
                        tcp_write_all_vectored(
                            stream,
                            &mut iov[..used],
                            "client WAL mirror window",
                        )?;
                    }
                }
                tracker.durable(journal.commit()?)?;
                peak_retained = peak_retained.max(journal.used_bytes());
                let deadline = Instant::now() + Duration::from_secs(30);
                while tracker.progress().acknowledged < (seq + batch) as u64 {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "no eligible mirror commitment",
                        ));
                    }
                    replies.progress(
                        &mut streams,
                        &mut tracker,
                        lane,
                        lane_count,
                        extent_bytes,
                        true,
                    )?;
                }
                if tracker.progress().remote_redundant < (seq + batch) as u64 {
                    early_windows += 1;
                }
                // One batch-completion sample, NOT batch fabricated independent
                // per-I/O observations or a clock read for every I/O.
                latency.record_duration(issued.elapsed());
                // Release/checkpoint in batches, not one disk drain per ACK.
                if journal.used_bytes() > journal.capacity_bytes() / 2 {
                    let release = tracker
                        .progress()
                        .remote_redundant
                        .min(journal.durable_hwm());
                    if release > journal.released_hwm() {
                        journal.release_remote_prefix(&tracker.redundant_prefix())?;
                        if config.repair.is_some() {
                            eprintln!(
                                "client-wal-custody-progress: lane={lane} released_hwm={} acknowledged={} remote_redundant={}",
                                journal.released_hwm(),
                                tracker.progress().acknowledged,
                                tracker.progress().remote_redundant
                            );
                        }
                    }
                }
                seq += batch;
            }
            operation_wall += started.elapsed();
            let drain = Instant::now();
            while tracker.progress().remote_redundant < extents_per_lane as u64 {
                if drain.elapsed() > Duration::from_secs(30) {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "remote redundancy drain timed out; client WAL retained",
                    ));
                }
                replies.progress(
                    &mut streams,
                    &mut tracker,
                    lane,
                    lane_count,
                    extent_bytes,
                    true,
                )?;
            }
            journal.release_remote_prefix(&tracker.redundant_prefix())?;
            final_drain += drain.elapsed();
            Ok(())
        })();
        for (_, stream) in &streams {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Err(error) = result {
            if let Some(repair) = &config.repair {
                // The peer may have seen an admitted write whose send failed
                // partway through. Fence and persist ALL admitted records so
                // recovery never replays an older suffix over newer data.
                tracker.durable(journal.commit()?)?;
                client_wal_repair::rebuild(repair, &mut journal, &mut tracker)?;
                return Err(io::Error::other(format!(
                    "mirror rebuilt after {error}; interrupted run is not a performance result"
                )));
            }
            return Err(error);
        }
    }
    eprintln!(
        "zcraid-mirror-client-wal-summary: worker={worker} early_windows={early_windows} peak_retained_bytes={peak_retained} payload_userspace_copy_bytes=0 disk_io={} early_ack_seconds={:.6} final_redundancy_drain_seconds={:.6} local_custody_release=both-remote-durable only_transport_delivery_is_not_durability=true",
        if config.direct {
            "direct-borrowed-arena"
        } else {
            "buffered-kernel-pagecache-copy"
        },
        operation_wall.as_secs_f64(),
        final_drain.as_secs_f64()
    );
    let end_switches = read_thread_context_switches(tid).unwrap_or(switches);
    let extents = extents_per_lane * lane_total;
    Ok(ZcWalExtentStats {
        worker,
        streams: 2 * lane_total,
        payload_bytes: extents * extent_bytes,
        wire_bytes: extents
            * (if config.serial_hop { 1 } else { 2 })
            * (ZC_WAL_EXTENT_HEADER_LEN + extent_bytes),
        extents,
        records: extents * (extent_bytes / ZC_WAL_RECORD_SIZE),
        acks: extents,
        wall: operation_wall,
        cpu: thread_cpu_time()
            .unwrap_or(cpu_start)
            .saturating_sub(cpu_start),
        target_cpu: affinity.target_cpu,
        affinity_applied: affinity.applied,
        start_cpu,
        end_cpu: current_cpu(),
        voluntary_switches: end_switches.voluntary.saturating_sub(switches.voluntary),
        involuntary_switches: end_switches
            .involuntary
            .saturating_sub(switches.involuntary),
        migrations: end_switches.migrations.saturating_sub(switches.migrations),
        ack_latency: latency,
        uring_recv: UringRecvStats::default(),
        virtual_volume_ops: Vec::new(),
        virtual_volume_hwms: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_wal_commitment::{CommitScope, Replica};

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let base = env::var_os("ZC_CLIENT_WAL_TEST_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/var/tmp"));
            let path = base.join(format!(
                "zc-client-custody-tcp-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn config(&self) -> Config {
            Config {
                policy: Policy {
                    scope: CommitScope {
                        volume: 1,
                        log: 2,
                        writer_epoch: 3,
                        lane: 0,
                    },
                    count_client_local_wal: true,
                    local: Replica {
                        id: 1,
                        incarnation: 1,
                        failure_domain: 1,
                    },
                    remote: [
                        Replica {
                            id: 2,
                            incarnation: 1,
                            failure_domain: 2,
                        },
                        Replica {
                            id: 3,
                            incarnation: 1,
                            failure_domain: 3,
                        },
                    ],
                },
                journal_directory: self.0.join("client"),
                capacity_bytes_per_lane: 1024 * 1024,
                direct: true,
                serial_hop: false,
                repair: None,
                rdma: None,
            }
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn targets(listeners: &[TcpListener; 2]) -> Arc<Vec<ZcRaidMirrorBranchTarget>> {
        Arc::new(
            listeners
                .iter()
                .enumerate()
                .map(|(branch, listener)| ZcRaidMirrorBranchTarget {
                    plan: ZcRaidMirrorBranchPlan {
                        branch,
                        fabric_domain: None,
                        lanes: vec![0],
                        leader_cpus: vec![0],
                        workers: vec![0],
                    },
                    addr: "127.0.0.1".into(),
                    base_port: listener.local_addr().unwrap().port(),
                    source_ip: None,
                })
                .collect(),
        )
    }

    fn receive(
        listener: TcpListener,
        path: PathBuf,
        config: Config,
        slot: usize,
        frames: usize,
        release: Option<mpsc::Receiver<()>>,
        reached: Option<mpsc::Sender<()>>,
        qualify: bool,
    ) -> io::Result<()> {
        let (mut stream, _) = listener.accept()?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut journal = Journal::open(
            &path,
            1024 * 1024,
            config.policy.scope,
            config.policy.remote[slot],
            BackingIoMode::Direct,
        )?;
        let arena = FixedSendBuffers::new(1, 4096)?;
        let payload = unsafe { slice::from_raw_parts_mut(arena.ptr(0), 4096) };
        if let Some(release) = release {
            release
                .recv_timeout(Duration::from_secs(5))
                .map_err(io::Error::other)?;
        }
        for seq in 0..frames {
            let mut bytes = [0; ZC_WAL_EXTENT_HEADER_LEN];
            stream.read_exact(&mut bytes)?;
            let header = ZcWalExtentHeader::decode(&bytes)?;
            zcraid_mirror_verify_extent_header(
                header,
                0,
                1,
                slot,
                seq,
                4096,
                ZcRaidZlaneCoordMode::LaneOwner,
            )?;
            stream.read_exact(payload)?;
            journal.append(
                &[Record {
                    logical_offset: header.base_wal_offset,
                    length: 4096,
                }],
                &[IoSlice::new(payload)],
            )?;
            journal.commit()?;
            let ack = if qualify {
                encode_durable_ack(header)?
            } else {
                zcraid_mirror_ack_for_header(header)?.encode()
            };
            stream.write_all(&ack)?;
        }
        if let Some(reached) = reached {
            reached.send(()).map_err(io::Error::other)?;
        }
        Ok(())
    }

    #[test]
    fn tcp_client_continues_while_second_durable_replica_is_stalled() {
        let directory = TestDirectory::new();
        let config = directory.config();
        let listeners = [
            TcpListener::bind("127.0.0.1:0").unwrap(),
            TcpListener::bind("127.0.0.1:0").unwrap(),
        ];
        let branches = targets(&listeners);
        let [first, second] = listeners;
        let (release_tx, release_rx) = mpsc::channel();
        let (reached_tx, reached_rx) = mpsc::channel();
        let cfg = config.clone();
        let path = directory.0.join("first.wal");
        let first =
            thread::spawn(move || receive(first, path, cfg, 0, 8, None, Some(reached_tx), true));
        let cfg = config.clone();
        let path = directory.0.join("second.wal");
        let second =
            thread::spawn(move || receive(second, path, cfg, 1, 8, Some(release_rx), None, true));
        let client = thread::spawn(move || tcp_send(0, vec![0], branches, 1, 8, 4096, 1, config));
        // First leg can receive all eight QD1 windows while the second leg has
        // not received/committed even the first window. This proves the sender
        // actually returns early and pipelines on, not just different labels.
        let reached = reached_rx.recv_timeout(Duration::from_secs(3));
        let _ = release_tx.send(());
        let result = client.join().unwrap();
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        assert!(
            reached.is_ok(),
            "sender waited for second replica before continuing"
        );
        assert_eq!(result.unwrap().acks, 8);
        for (slot, name) in ["first.wal", "second.wal"].iter().enumerate() {
            let config = directory.config();
            let journal = Journal::open(
                &directory.0.join(name),
                1024 * 1024,
                config.policy.scope,
                config.policy.remote[slot],
                BackingIoMode::Direct,
            )
            .unwrap();
            assert_eq!(journal.durable_hwm(), 8);
        }
    }

    #[test]
    fn tcp_legacy_or_volatile_ack_is_not_a_durable_witness() {
        let directory = TestDirectory::new();
        let config = directory.config();
        let listeners = [
            TcpListener::bind("127.0.0.1:0").unwrap(),
            TcpListener::bind("127.0.0.1:0").unwrap(),
        ];
        let branches = targets(&listeners);
        let mut servers = Vec::new();
        for (slot, listener) in listeners.into_iter().enumerate() {
            let cfg = config.clone();
            let path = directory.0.join(format!("remote-{slot}.wal"));
            servers.push(thread::spawn(move || {
                receive(listener, path, cfg, slot, 1, None, None, false)
            }));
        }
        let error = tcp_send(0, vec![0], branches, 1, 1, 4096, 1, config.clone())
            .err()
            .unwrap();
        assert!(error.to_string().contains("did not attest"));
        for server in servers {
            let _ = server.join().unwrap();
        }
        // A rejected ACK does not reclaim the client's only proven copy.
        let path = config
            .journal_directory
            .join("volume-1-log-2-epoch-3-lane-0.wal");
        let journal = Journal::open(
            &path,
            config.capacity_bytes_per_lane,
            config.policy.scope,
            config.policy.local,
            BackingIoMode::Direct,
        )
        .unwrap();
        assert_eq!(journal.durable_hwm(), 1);
        assert_eq!(journal.released_hwm(), 0);
        assert_eq!(journal.replay(1, 1).unwrap().count(), 1);
    }
}
