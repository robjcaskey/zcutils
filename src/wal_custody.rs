//! Opt-in live client-local persistent custody at the existing WAL volume
//! onramp. The kernel edge is unchanged: mirror placement, receipts, retained
//! reads, reconnection and reconstruction belong entirely to this userspace
//! stage. Normal direct TCP/RDMA fast paths do not enter this adapter.
//!
//! Initial scope: one writer, one lane, 4K-aligned requests, trusted private TCP.
//! This exercises the live contract, not a replacement for the multi-lane EFA
//! saturation path. It deliberately refuses unimplemented capability flags.

use crate::client_wal_commitment::{Commitment, DurableReceipt, Policy};
use crate::client_wal_journal::{Journal, Record, ReplayRecord};
use crate::persistent_wal::BackingIoMode;
use crate::raid_mirror_serial::Arena;
use crate::topology::{DurabilityObligation, TopologyCommand};
use crate::topology_controller::EvolutionController;
use crate::wal_custody_peer::{self as peer, Header, Peer};
use crate::*;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::os::fd::AsRawFd;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    policy: Policy,
    listen: SocketAddr,
    journal_directory: PathBuf,
    journal_bytes: u64,
    logical_bytes: u64,
    middle: Peer,
    tail: Peer,
    replacement: Peer,
    #[serde(default)]
    allow_plaintext_private_network: bool,
    #[serde(default = "default_timeout")]
    failure_timeout_ms: u64,
}
fn default_timeout() -> u64 {
    1000
}

struct Replies {
    stream: TcpStream,
    bytes: [u8; peer::HEADER_BYTES],
    used: usize,
}
impl Replies {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            bytes: [0; peer::HEADER_BYTES],
            used: 0,
        }
    }
    /// Nonblocking per-call receive, preserving partial headers. Never change
    /// the shared socket's blocking flag or mistake a TCP fragment for an ACK.
    fn next(&mut self, scope: client_wal_commitment::CommitScope) -> io::Result<Option<Header>> {
        loop {
            let result = unsafe {
                libc::recv(
                    self.stream.as_raw_fd(),
                    self.bytes[self.used..].as_mut_ptr().cast(),
                    self.bytes.len() - self.used,
                    libc::MSG_DONTWAIT,
                )
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if result == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.used += result as usize;
            if self.used == self.bytes.len() {
                self.used = 0;
                return Header::decode(&self.bytes, scope).map(Some);
            }
        }
    }
}

fn wait_readable(stream: &TcpStream, millis: i32) -> io::Result<bool> {
    let mut fd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut fd, 1, millis) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(result != 0)
}

struct Rebuild {
    copied: mpsc::Receiver<io::Result<()>>,
    start_hwm: u64,
    handoff: String,
}

struct Client {
    config: Config,
    journal: Journal,
    tracker: Commitment,
    route: Replies,
    read_tail: TcpStream,
    generation: u64,
    direct_tail: bool,
    rebuilt: bool,
    rebuild: Option<Rebuild>,
    controller: EvolutionController,
    overlay: BTreeMap<u64, (ReplayRecord, u64)>,
    early_middle: u64,
    early_tail: u64,
    overlay_reads: u64,
    remote_reads: u64,
    writes_during_copy: u64,
}

impl Client {
    fn open(config: Config) -> io::Result<Self> {
        if !config.allow_plaintext_private_network
            || !config.policy.count_client_local_wal
            || config.policy.scope.lane != 0
            || config.logical_bytes == 0
            || config.logical_bytes % 4096 != 0
            || config.middle.owner != config.policy.remote[0]
            || config.tail.owner != config.policy.remote[1]
            || !(50..=30_000).contains(&config.failure_timeout_ms)
        {
            return Err(io::Error::other(
                "custody client requires opt-in policy, trusted TCP, one lane and matching peer identities",
            ));
        }
        let tracker = Commitment::new(config.policy)?;
        let mut validate_replacement = Commitment::new(config.policy)?;
        validate_replacement.replace_remote(0, config.replacement.owner)?;
        peer::durable_directory(&config.journal_directory)?;
        let journal = Journal::open(
            &config.journal_directory.join("client.wal"),
            config.journal_bytes,
            config.policy.scope,
            config.policy.local,
            BackingIoMode::Direct,
        )?;
        if journal.submitted_hwm() != 0 {
            return Err(io::Error::other(
                "existing client custody needs recovery reconciliation, never reset it",
            ));
        }
        let timeout = Duration::from_millis(config.failure_timeout_ms);
        let mut route = peer::connect(config.middle.address, timeout)?;
        let middle = peer::hello(&mut route, config.policy.scope, &config.middle, 1)?;
        let mut read_tail = peer::connect(config.tail.address, timeout)?;
        let tail = peer::hello_read(&mut read_tail, config.policy.scope, &config.tail, 1)?;
        if middle.sequence != 0 || tail.sequence != 0 {
            return Err(io::Error::other(
                "new client session cannot silently adopt a nonempty volume",
            ));
        }
        let controller =
            EvolutionController::open(config.journal_directory.join("topology.ndjson"), 1)?;
        if controller.state().applied_index != 0 {
            return Err(io::Error::other(
                "existing topology requires reconciliation, not bootstrap",
            ));
        }
        let placement = |replica| client_wal_repair::placement(config.policy.scope, replica);
        let old = placement(config.middle.owner);
        controller.bootstrap(
            "live-custody-bootstrap",
            DurabilityObligation {
                obligation_id: "live-full-replicas".into(),
                group_id: old.group_id.clone(),
                required_copies: 2,
                distinct: BTreeMap::from([("failure.host".into(), 2)]),
                required_roles: BTreeMap::from([("full-replica".into(), 2)]),
            },
            &[
                (old, BTreeMap::from([(0, 0)])),
                (placement(config.tail.owner), BTreeMap::from([(0, 0)])),
            ],
        )?;
        Ok(Self {
            config,
            journal,
            tracker,
            route: Replies::new(route),
            read_tail,
            generation: 1,
            direct_tail: false,
            rebuilt: false,
            rebuild: None,
            controller,
            overlay: BTreeMap::new(),
            early_middle: 0,
            early_tail: 0,
            overlay_reads: 0,
            remote_reads: 0,
            writes_during_copy: 0,
        })
    }
    fn record_receipt(&mut self, ack: Header) -> io::Result<()> {
        let policy = self.tracker.policy();
        let owner = policy
            .remote
            .iter()
            .find(|p| p.id == ack.replica)
            .ok_or_else(|| io::Error::other("receipt not from an admitted remote"))?;
        ack.validate_ack(*owner, self.generation)?;
        if ack.length != 0 {
            return Err(io::Error::other("unexpected payload on receipt stream"));
        }
        self.tracker.durable(DurableReceipt {
            scope: policy.scope,
            replica: owner.id,
            incarnation: owner.incarnation,
            through: ack.sequence,
        })?;
        Ok(())
    }
    fn collect(&mut self) -> io::Result<()> {
        loop {
            match self.route.next(self.config.policy.scope) {
                Ok(Some(ack)) => self.record_receipt(ack)?,
                Ok(None) => break,
                Err(error) if !self.direct_tail => {
                    self.lose_middle(error)?;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        self.reclaim()
    }
    fn reclaim(&mut self) -> io::Result<()> {
        let through = self
            .tracker
            .progress()
            .remote_redundant
            .min(self.journal.durable_hwm());
        // Replacement resets that replica's credit, not our historical release.
        if through > self.journal.released_hwm() {
            self.journal
                .release_remote_prefix(&self.tracker.redundant_prefix())?;
            self.overlay
                .retain(|_, (record, _)| record.sequence > through);
        }
        Ok(())
    }
    fn await_commit(&mut self, through: u64) -> io::Result<()> {
        let mut started = Instant::now();
        while self.tracker.progress().acknowledged < through {
            self.collect()?;
            if self.tracker.progress().acknowledged >= through {
                break;
            }
            if started.elapsed() >= Duration::from_millis(self.config.failure_timeout_ms) {
                if self.direct_tail {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "surviving durable copy unavailable; fail closed",
                    ));
                }
                self.lose_middle(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "middle stopped producing durable receipts",
                ))?;
                started = Instant::now();
            }
            wait_readable(&self.route.stream, 10)?;
        }
        Ok(())
    }
    fn write(&mut self, offset: u64, payload: &[u8]) -> io::Result<()> {
        self.collect()?;
        self.finish_copy()?;
        let through = loop {
            match self.journal.append(
                &[Record {
                    logical_offset: offset,
                    length: payload.len() as u64,
                }],
                &[IoSlice::new(payload)],
            ) {
                Ok(hwm) => break hwm,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.collect()?;
                    self.finish_copy()?;
                    wait_readable(&self.route.stream, 10)?;
                }
                Err(error) => return Err(error),
            }
        };
        self.tracker.submitted(through)?;
        let request = Header {
            offset,
            length: payload.len() as u64,
            ..Header::request(
                self.config.policy.scope,
                peer::WRITE,
                self.generation,
                through,
            )
        };
        // Socket TX and local pwritev borrow the same final RX allocation.
        // Durable local credit is published ONLY after its actual fdatasync.
        let sent = request.write(&mut self.route.stream, payload);
        self.tracker.durable(self.journal.commit()?)?;
        let record = self
            .journal
            .replay(through, through)?
            .next()
            .ok_or_else(|| io::Error::other("missing committed overlay extent"))?;
        for within in (0..payload.len()).step_by(4096) {
            self.overlay
                .insert(offset + within as u64, (record, within as u64));
        }
        if let Err(error) = sent {
            if self.direct_tail {
                return Err(error);
            }
            self.lose_middle(error)?;
        }
        self.await_commit(through)?;
        let progress = self.tracker.progress();
        if progress.remote_durable[0] >= through && progress.remote_durable[1] < through {
            self.early_middle += 1;
        }
        if progress.remote_durable[1] >= through && progress.remote_durable[0] < through {
            self.early_tail += 1;
        }
        if (self.early_middle == 1 && progress.remote_durable[1] < through)
            || (self.early_tail == 1 && progress.remote_durable[0] < through)
            || (through % 16 == 0
                && (progress.remote_durable[0] < through || progress.remote_durable[1] < through))
        {
            println!(
                "custody-live-race: generation={} winner={} through={through} local={} middle={} third={} lagging_sync_pending=true",
                self.generation,
                if progress.remote_durable[0] < through {
                    "third"
                } else {
                    "middle"
                },
                progress.local_durable,
                progress.remote_durable[0],
                progress.remote_durable[1]
            );
        }
        if self.rebuild.is_some() {
            self.writes_during_copy += 1;
        }
        if through <= 4 || through % 16 == 0 {
            println!(
                "custody-live-progress: submitted={through} acknowledged={} local={} middle={} third={} released={} early_middle={} early_third={} rebuilding={} writes_during_copy={} client_reconnect=false",
                progress.acknowledged,
                progress.local_durable,
                progress.remote_durable[0],
                progress.remote_durable[1],
                self.journal.released_hwm(),
                self.early_middle,
                self.early_tail,
                self.rebuild.is_some(),
                self.writes_during_copy
            );
        }
        self.reclaim()
    }
    fn read(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.collect()?;
        self.finish_copy()?;
        for (index, page) in out.chunks_exact_mut(4096).enumerate() {
            let page_offset = offset + (index * 4096) as u64;
            if let Some(&(record, within)) = self.overlay.get(&page_offset) {
                self.journal.read_retained(record, within, page)?;
                self.overlay_reads += 1;
            } else {
                let request = Header {
                    offset: page_offset,
                    length: 4096,
                    ..Header::request(self.config.policy.scope, peer::READ, self.generation, 0)
                };
                request.write(&mut self.read_tail, &[])?;
                let result = Header::read(&mut self.read_tail, self.config.policy.scope)?
                    .validate_ack(self.config.tail.owner, self.generation)?;
                if result.offset != page_offset
                    || result.length != 4096
                    || result.sequence < self.journal.released_hwm()
                {
                    return Err(io::Error::other(
                        "remote read is behind reclaimed custody or has wrong range",
                    ));
                }
                self.read_tail.read_exact(page)?;
                self.remote_reads += 1;
            }
        }
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        self.tracker.durable(self.journal.commit()?)?;
        self.await_commit(self.journal.durable_hwm())?;
        self.collect()?;
        self.finish_copy()
    }
    fn lose_middle(&mut self, error: io::Error) -> io::Result<()> {
        if self.direct_tail {
            return Err(error);
        }
        let progress = self.tracker.progress();
        let transition_started = Instant::now();
        if progress.local_durable != progress.submitted {
            return Err(io::Error::other(
                "failover requires locally drained admission",
            ));
        }
        let next_generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("generation exhausted"))?;
        println!(
            "custody-live-middle-lost: acknowledged={} retained_from={} local_hwm={} cause={error}",
            progress.acknowledged,
            self.journal.released_hwm() + 1,
            progress.local_durable
        );
        // A confirmed, persisted fence on S precedes any replay/direct writes.
        // Closing M's socket alone is NOT fencing a partitioned old forwarder.
        let mut direct = peer::connect(self.config.tail.address, Duration::from_secs(5))?;
        Header::request(self.config.policy.scope, peer::FENCE, next_generation, 0)
            .write(&mut direct, &[])?;
        let ack = Header::read(&mut direct, self.config.policy.scope)?
            .validate_ack(self.config.tail.owner, next_generation)?;
        if ack.sequence < self.journal.released_hwm() || ack.sequence > progress.submitted {
            return Err(io::Error::other(
                "surviving image cannot cover reclaimed custody",
            ));
        }
        let _ = self.route.stream.shutdown(std::net::Shutdown::Both);
        self.generation = next_generation;
        self.direct_tail = true;
        self.route = Replies::new(direct);
        self.read_tail = peer::connect(self.config.tail.address, Duration::from_secs(5))?;
        peer::hello_read(
            &mut self.read_tail,
            self.config.policy.scope,
            &self.config.tail,
            self.generation,
        )?;
        let old =
            client_wal_repair::placement(self.config.policy.scope, self.tracker.policy().remote[0]);
        self.controller.set_available(
            &format!("middle-lost-{next_generation}"),
            &old.node.node_id,
            false,
        )?;
        // Reset dead credit immediately, preserving historical client ACKs.
        let mut replacement = self.config.replacement.owner;
        if self.rebuilt {
            replacement.incarnation = self.tracker.policy().remote[0].incarnation + 1;
        }
        self.tracker.replace_remote(0, replacement)?;
        self.record_receipt(ack)?;
        for record in self
            .journal
            .replay(ack.sequence + 1, progress.local_durable)?
        {
            send_retained(
                &mut self.route.stream,
                &self.journal,
                record,
                self.generation,
            )?;
            let receipt = Header::read(&mut self.route.stream, self.config.policy.scope)?
                .validate_ack(self.config.tail.owner, self.generation)?;
            self.tracker.durable(DurableReceipt {
                scope: self.config.policy.scope,
                replica: receipt.replica,
                incarnation: receipt.incarnation,
                through: receipt.sequence,
            })?;
        }
        println!(
            "custody-live-direct-survivor: generation={} durable_hwm={} local_plus_third=true frontend_reconnect=false replayed_records={} transition_us={} detection_timeout_ms={}",
            self.generation,
            self.tracker.progress().remote_durable[1],
            progress.local_durable - ack.sequence,
            transition_started.elapsed().as_micros(),
            self.config.failure_timeout_ms
        );
        if self.rebuilt {
            eprintln!(
                "custody-live-needs-capacity: no further preapproved replacement; retain local WAL and backpressure when full"
            );
            return Ok(());
        }
        let survivor =
            client_wal_repair::placement(self.config.policy.scope, self.config.tail.owner);
        let replacement =
            client_wal_repair::placement(self.config.policy.scope, self.config.replacement.owner);
        let handoff = self.controller.stage_replica(
            "live-stage-replacement",
            &survivor.replica_id,
            &replacement,
            BTreeMap::from([(0, progress.local_durable)]),
        )?;
        let config = self.config.clone();
        let generation = self.generation;
        let start_hwm = progress.local_durable;
        let (complete, copied) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = (|| {
                let deadline = Instant::now() + Duration::from_secs(90);
                let mut stream = loop {
                    match peer::connect(config.replacement.address, Duration::from_millis(300)) {
                        Ok(stream) => break stream,
                        Err(_) if Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(100))
                        }
                        Err(error) => return Err(error),
                    }
                };
                stream.set_read_timeout(Some(Duration::from_secs(60)))?;
                Header::request(config.policy.scope, peer::COPY, generation, start_hwm)
                    .write(&mut stream, &[])?;
                let ack = Header::read(&mut stream, config.policy.scope)?
                    .validate_ack(config.replacement.owner, generation)?;
                if ack.sequence != start_hwm {
                    return Err(io::Error::other("replacement base frontier mismatch"));
                }
                Ok(())
            })();
            let _ = complete.send(result);
        });
        self.rebuild = Some(Rebuild {
            copied,
            start_hwm,
            handoff,
        });
        println!(
            "custody-live-replacement-staged: required_hwm={start_hwm} replica={} incarnation={} counted=false base_copy=survivor-to-replacement foreground=client-to-survivor control=committed-state-machine raft_quorum_test=false",
            self.config.replacement.owner.id, self.config.replacement.owner.incarnation
        );
        Ok(())
    }
    fn finish_copy(&mut self) -> io::Result<()> {
        let Some(rebuild) = &self.rebuild else {
            return Ok(());
        };
        match rebuild.copied.try_recv() {
            Ok(result) => result?,
            Err(mpsc::TryRecvError::Empty) => return Ok(()),
            Err(error) => return Err(io::Error::other(error)),
        }
        let rebuild = self.rebuild.take().unwrap();
        let fence = self.journal.durable_hwm();
        self.await_commit(fence)?;
        let started = Instant::now();
        let (replay_bytes, compact_bytes) = compact_replay(
            &self.journal,
            &self.config.replacement,
            self.generation,
            rebuild.start_hwm,
            fence,
        )?;
        let mut destination =
            peer::connect(self.config.replacement.address, Duration::from_secs(5))?;
        Header::request(
            self.config.policy.scope,
            peer::ACTIVATE,
            self.generation,
            fence,
        )
        .write(&mut destination, &[])?;
        let activated = Header::read(&mut destination, self.config.policy.scope)?
            .validate_ack(self.config.replacement.owner, self.generation)?;
        if activated.sequence != fence {
            return Err(io::Error::other(
                "replacement activation changed its durable prefix",
            ));
        }
        // Even a zero-length suffix needs an explicit durable copy receipt.
        self.controller.activate_copied_replica(
            "live-activate-replacement",
            &rebuild.handoff,
            BTreeMap::from([(0, fence)]),
        )?;
        let survivor =
            client_wal_repair::placement(self.config.policy.scope, self.config.tail.owner);
        let custody = self.controller.state().custody[&survivor.replica_id].clone();
        self.controller.commit(
            "live-survivor-current",
            vec![TopologyCommand::AdvanceCustodyHwm {
                replica_id: survivor.replica_id,
                term: custody.term,
                lane_hwms: BTreeMap::from([(0, fence)]),
            }],
        )?;
        self.controller
            .state()
            .verify_coverage("live-full-replicas", &BTreeMap::from([(0, fence)]))?;
        self.tracker.durable(DurableReceipt {
            scope: self.config.policy.scope,
            replica: self.config.replacement.owner.id,
            incarnation: self.config.replacement.owner.incarnation,
            through: fence,
        })?;
        self.route = Replies::new(destination);
        self.direct_tail = false;
        self.rebuilt = true;
        self.reclaim()?;
        println!(
            "custody-live-rebuilt: through={fence} replay_bytes={replay_bytes} compact_bytes={compact_bytes} copy_payload_rebuffer_bytes=0 writes_during_copy={} catchup_us={} route=client-to-new-middle-to-third client_reconnect=false replacement_counted_after_copy=true",
            self.writes_during_copy,
            started.elapsed().as_micros()
        );
        Ok(())
    }
}

/// The replacement is not readable/countable until this state-image patch
/// commits. Therefore overwritten versions can be reduced to the LAST retained
/// version of each page at the fenced HWM. This is full-image reconstruction,
/// not PITR/history compaction. Borrowing the journal pins all sendfile extents.
fn compact_tail(
    journal: &Journal,
    from: u64,
    through: u64,
) -> io::Result<(u64, BTreeMap<u64, u64>)> {
    let mut pages = BTreeMap::new();
    let mut original = 0;
    for record in journal.replay(from + 1, through)? {
        original += record.length;
        for within in (0..record.length).step_by(4096) {
            pages.insert(
                (record.logical_offset + within) / 4096,
                record.file_offset + within,
            );
        }
    }
    Ok((original, pages))
}

fn compact_replay(
    journal: &Journal,
    destination: &Peer,
    generation: u64,
    from: u64,
    through: u64,
) -> io::Result<(u64, u64)> {
    let (original, pages) = compact_tail(journal, from, through)?;
    let compact = pages.len() as u64 * 4096;
    let mut stream = peer::connect(destination.address, Duration::from_secs(5))?;
    let begin = Header {
        offset: from,
        ..Header::request(journal.scope(), peer::PATCH, generation, through)
    };
    begin.write(&mut stream, &[])?;
    let accepted =
        Header::read(&mut stream, journal.scope())?.validate_ack(destination.owner, generation)?;
    if accepted.sequence != from {
        return Err(io::Error::other("state patch base changed before catchup"));
    }
    let entries = pages.into_iter().collect::<Vec<_>>();
    for batch in entries.chunks(peer::MAX_PAYLOAD / 4096) {
        let header = Header {
            op: peer::PATCH_PAGES,
            length: batch.len() as u64 * 4096,
            ..begin
        };
        let table = batch
            .iter()
            .flat_map(|(page, _)| page.to_le_bytes())
            .collect::<Vec<_>>();
        header.write(&mut stream, &table)?;
        for (_, offset) in batch {
            send_file_range(&mut stream, journal, *offset, 4096)?;
        }
        Header::read(&mut stream, journal.scope())?.validate_ack(destination.owner, generation)?;
    }
    Header {
        op: peer::PATCH_DONE,
        ..begin
    }
    .write(&mut stream, &[])?;
    let durable =
        Header::read(&mut stream, journal.scope())?.validate_ack(destination.owner, generation)?;
    if durable.sequence != through {
        return Err(io::Error::other(
            "state patch did not commit its fenced prefix",
        ));
    }
    Ok((original, compact))
}

fn send_retained(
    stream: &mut TcpStream,
    journal: &Journal,
    record: ReplayRecord,
    generation: u64,
) -> io::Result<()> {
    let header = Header {
        offset: record.logical_offset,
        length: record.length,
        ..Header::request(journal.scope(), peer::WRITE, generation, record.sequence)
    };
    header.write(stream, &[])?;
    send_file_range(stream, journal, record.file_offset, record.length)
}

fn send_file_range(
    stream: &mut TcpStream,
    journal: &Journal,
    offset: u64,
    length: u64,
) -> io::Result<()> {
    let mut offset = offset as libc::off_t;
    let mut left = length;
    while left != 0 {
        let sent = unsafe {
            libc::sendfile(
                stream.as_raw_fd(),
                journal.file().as_raw_fd(),
                &mut offset,
                left as usize,
            )
        };
        if sent < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if sent == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        left -= sent as u64;
    }
    Ok(())
}

fn check_frontend(frame: ZcnblkFanWalFrame, logical_bytes: u64) -> io::Result<()> {
    if frame.lane_id != 0 || frame.lane_count != 1 {
        return Err(io::Error::other("custody adapter supports one writer lane"));
    }
    if matches!(
        frame.op,
        ZCNBLK_FAN_WAL_OP_WRITE_DESC | ZCNBLK_FAN_WAL_OP_READ_DESC
    ) {
        if frame.payload_len == 0
            || frame.payload_len as usize > peer::MAX_PAYLOAD
            || frame.payload_len % 4096 != 0
            || frame.leaf_offset % 4096 != 0
            || frame
                .leaf_offset
                .checked_add(frame.payload_len as u64)
                .is_none_or(|end| end > logical_bytes)
        {
            return Err(io::Error::other(
                "frontend must use bounded aligned requests inside the volume",
            ));
        }
    }
    Ok(())
}

fn execute_frontend(
    client: &mut Client,
    frame: ZcnblkFanWalFrame,
    payload: &[u8],
    out: &mut [u8],
    features: u32,
) -> io::Result<ZcnblkFanWalFrame> {
    check_frontend(frame, client.config.logical_bytes)?;
    let mut result = ZcnblkFanWalFrame {
        op: ZCNBLK_FAN_WAL_OP_RESULT,
        payload_len: 0,
        status: 0,
        ..frame
    };
    match frame.op {
        ZCNBLK_FAN_WAL_OP_WRITE_DESC => {
            frame.validate_io_contract(features, true)?;
            if payload.len() != frame.payload_len as usize {
                return Err(io::Error::other("custody write payload length mismatch"));
            }
            client.write(frame.leaf_offset, payload)?;
        }
        ZCNBLK_FAN_WAL_OP_READ_DESC => {
            frame.validate_io_contract(features, true)?;
            if out.len() < frame.payload_len as usize {
                return Err(io::Error::other("custody read destination too small"));
            }
            client.read(frame.leaf_offset, &mut out[..frame.payload_len as usize])?;
            result.payload_len = frame.payload_len;
        }
        ZCNBLK_FAN_WAL_OP_SYNC => client.sync()?,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unnegotiated custody operation",
            ));
        }
    }
    Ok(result)
}

const FRONTEND_FEATURES: u32 = ZCNBLK_WAL_FEATURE_FUA
    | ZCNBLK_WAL_FEATURE_BATCH_SUBMISSION
    | ZCNBLK_WAL_FEATURE_REGISTERED_LEASE
    | ZCNBLK_WAL_FEATURE_IO_PRIORITY;

/// A separate userspace placement/custody stage, co-located with the SHM lane
/// owner. Control headers retain the existing edge handshake. Data MUST enter
/// through `execute`: borrowed shared-arena pages, never a loopback socket or a
/// serialized payload bounce. No kernel placement decisions are introduced.
pub(super) struct LocalEndpoint {
    client: Client,
    control: Option<([u8; ZCNBLK_FAN_WAL_HEADER_LEN], usize)>,
    features: Option<u32>,
    closed: bool,
    completed: std::collections::VecDeque<(u64, u64, u64, usize)>,
}

impl LocalEndpoint {
    pub(super) fn open(path: &str) -> io::Result<Self> {
        let config = serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
        let client = Client::open(config)?;
        println!(
            "custody-live-ready: frontend=shared-arena-direct single_writer=true lane=0 worker=0 placement=userspace client_payload_rebuffer_bytes=0 local_socket_hops=0 representative_performance=false sync=local-persistent-plus-either-remote"
        );
        Ok(Self {
            client,
            control: None,
            features: None,
            closed: false,
            completed: std::collections::VecDeque::with_capacity(128),
        })
    }

    pub(super) fn ready_for_batch(&self) -> io::Result<()> {
        if self.closed
            || self.control.is_some()
            || self.features.is_none()
            || self.completed.len() >= 128
        {
            return Err(io::Error::other(
                "custody batch unavailable or completion queue full",
            ));
        }
        Ok(())
    }

    pub(super) fn execute(
        &mut self,
        frame: ZcnblkFanWalFrame,
        payload: &[u8],
        out: &mut [u8],
    ) -> io::Result<()> {
        self.ready_for_batch()?;
        execute_frontend(
            &mut self.client,
            frame,
            payload,
            out,
            self.features.unwrap(),
        )?;
        Ok(())
    }

    pub(super) fn completed_batch(&mut self, key: (u64, u64, u64, usize)) -> io::Result<()> {
        self.ready_for_batch()?;
        if key.3 == 0 {
            return Err(io::Error::other("empty custody completion batch"));
        }
        self.completed.push_back(key);
        Ok(())
    }

    pub(super) fn consume_batch(&mut self, key: (u64, u64, u64, usize)) -> io::Result<()> {
        if self.completed.front() != Some(&key) {
            return Err(io::Error::other(
                "custody shared-arena completion identity mismatch",
            ));
        }
        self.completed.pop_front();
        Ok(())
    }
}

impl Read for LocalEndpoint {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let (bytes, used) = self.control.as_mut().ok_or_else(|| {
            io::Error::other(
                "custody has no pending control response; payloads require direct arena API",
            )
        })?;
        let size = out.len().min(bytes.len() - *used);
        out[..size].copy_from_slice(&bytes[*used..*used + size]);
        *used += size;
        if *used == bytes.len() {
            self.control = None;
        }
        Ok(size)
    }
}

impl Write for LocalEndpoint {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if self.closed || self.control.is_some() || input.len() != ZCNBLK_FAN_WAL_HEADER_LEN {
            return Err(io::Error::other(
                "custody requires one control header, no payload serialization",
            ));
        }
        let bytes: &[u8; ZCNBLK_FAN_WAL_HEADER_LEN] = input.try_into().unwrap();
        let frame = ZcnblkFanWalFrame::decode(bytes)?;
        check_frontend(frame, self.client.config.logical_bytes)?;
        if frame.payload_len != 0 || !self.completed.is_empty() {
            return Err(io::Error::other(
                "custody control before pending batch completions drained",
            ));
        }
        let response = match frame.op {
            ZCNBLK_FAN_WAL_OP_HELLO if self.features.is_none() => {
                let features = frame.hello_features()?.unwrap_or(0) & FRONTEND_FEATURES;
                self.features = Some(features);
                ZcnblkFanWalFrame {
                    op: ZCNBLK_FAN_WAL_OP_HELLO_ACK,
                    flags: frame.flags & ZCNBLK_FAN_WAL_FLAG_RESULT_RANGE_BATCH,
                    ..frame
                }
                .with_hello_features(features)?
            }
            ZCNBLK_FAN_WAL_OP_SYNC if self.features.is_some() => {
                self.client.sync()?;
                ZcnblkFanWalFrame {
                    op: ZCNBLK_FAN_WAL_OP_RESULT,
                    status: 0,
                    ..frame
                }
            }
            ZCNBLK_FAN_WAL_OP_EOF if self.features.is_some() => {
                finish_client(&mut self.client)?;
                self.closed = true;
                return Ok(input.len());
            }
            _ => {
                return Err(io::Error::other(
                    "custody payloads require the shared-arena API",
                ));
            }
        };
        self.control = Some((response.encode(), 0));
        Ok(input.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serve_frontend(config: Config) -> io::Result<()> {
    let listener = TcpListener::bind(config.listen)?;
    let mut client = Client::open(config)?;
    maybe_pin_current_thread("custody-client", 0);
    println!(
        "custody-live-ready: listen={} frontend=existing-fanwal single_writer=true lane=0 worker=0 placement=userspace normal_fastpaths_changed=false representative_performance=false sync=local-persistent-plus-either-remote readahead=retained-wal-overlay",
        client.config.listen
    );
    let (mut upstream, _) = listener.accept()?;
    upstream.set_nodelay(true)?;
    upstream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let hello = wal_failover::read_frame_header(&mut upstream)?;
    check_frontend(hello, client.config.logical_bytes)?;
    if hello.op != ZCNBLK_FAN_WAL_OP_HELLO {
        return Err(io::Error::other("frontend lacks HELLO"));
    }
    // The edge keeps registered source leases until our result and the result
    // echoes their identity. Priority is advisory within this ordered lane,
    // just as it is for the existing persistent terminal backend.
    let negotiated = hello.hello_features()?.unwrap_or(0) & FRONTEND_FEATURES;
    let result_ranges = hello.flags & ZCNBLK_FAN_WAL_FLAG_RESULT_RANGE_BATCH != 0;
    let ack = ZcnblkFanWalFrame {
        op: ZCNBLK_FAN_WAL_OP_HELLO_ACK,
        flags: hello.flags & ZCNBLK_FAN_WAL_FLAG_RESULT_RANGE_BATCH,
        payload_len: 0,
        ..hello
    }
    .with_hello_features(negotiated)?;
    wal_failover::write_frame(&mut upstream, ack, &[])?;
    let mut arena = Arena::new(peer::MAX_PAYLOAD)?;
    let mut read_arena = Arena::new(peer::MAX_PAYLOAD)?;
    loop {
        if !wait_readable(&upstream, 100)? {
            client.collect()?;
            client.finish_copy()?;
            continue;
        }
        let frame = match wal_failover::read_frame_header(&mut upstream) {
            Ok(frame) => frame,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        };
        check_frontend(frame, client.config.logical_bytes)?;
        if frame.op == ZCNBLK_FAN_WAL_OP_EOF {
            break;
        }
        if matches!(
            frame.op,
            ZCNBLK_FAN_WAL_OP_REQUEST_BATCH | ZCNBLK_FAN_WAL_OP_WRITE_BATCH
        ) {
            let count = frame.segment_count as usize;
            if count == 0
                || count > 256
                || frame.payload_len as usize > 256 * ZCNBLK_FAN_WAL_HEADER_LEN + peer::MAX_PAYLOAD
            {
                return Err(io::Error::other(
                    "custody frontend batch exceeds negotiated bounds",
                ));
            }
            let mut descriptors = Vec::with_capacity(count);
            let mut writes = 0usize;
            let mut reads = 0usize;
            for _ in 0..count {
                let descriptor = wal_failover::read_frame_header(&mut upstream)?;
                check_frontend(descriptor, client.config.logical_bytes)?;
                match descriptor.op {
                    ZCNBLK_FAN_WAL_OP_WRITE_DESC => writes += descriptor.payload_len as usize,
                    ZCNBLK_FAN_WAL_OP_READ_DESC => reads += descriptor.payload_len as usize,
                    _ => return Err(io::Error::other("invalid batch descriptor")),
                }
                descriptors.push(descriptor);
            }
            if writes > peer::MAX_PAYLOAD
                || reads > peer::MAX_PAYLOAD
                || frame.payload_len as usize != count * ZCNBLK_FAN_WAL_HEADER_LEN + writes
            {
                return Err(io::Error::other(
                    "custody batch payload/descriptor geometry mismatch",
                ));
            }
            // Receive control separately so every payload starts 4K aligned;
            // this avoids an alignment bounce before O_DIRECT journal writes.
            upstream.read_exact(&mut arena.bytes_mut()[..writes])?;
            let mut results = Vec::with_capacity(count * ZCNBLK_FAN_WAL_HEADER_LEN);
            let mut write_at = 0;
            let mut read_at = 0;
            let mut lease_hwm = 0;
            for descriptor in descriptors {
                lease_hwm = lease_hwm.max(descriptor.io_contract()?.lease_id);
                let size = descriptor.payload_len as usize;
                let payload = if descriptor.op == ZCNBLK_FAN_WAL_OP_WRITE_DESC {
                    let data = &arena.bytes()[write_at..write_at + size];
                    write_at += size;
                    data
                } else {
                    &[]
                };
                let result = execute_frontend(
                    &mut client,
                    descriptor,
                    payload,
                    &mut read_arena.bytes_mut()[read_at..],
                    negotiated,
                )?;
                read_at += result.payload_len as usize;
                results.extend_from_slice(&result.encode());
            }
            let range_only = read_at == 0 && result_ranges;
            let result = ZcnblkFanWalFrame {
                op: if range_only {
                    ZCNBLK_FAN_WAL_OP_RESULT_RANGE_BATCH
                } else {
                    ZCNBLK_FAN_WAL_OP_RESULT_BATCH
                },
                payload_len: if range_only {
                    0
                } else {
                    (results.len() + read_at) as u32
                },
                status: 0,
                sync_epoch: lease_hwm,
                ..frame
            }
            .encode();
            let mut iov = [
                IoSlice::new(&result),
                IoSlice::new(if range_only { &[] } else { &results }),
                IoSlice::new(&read_arena.bytes()[..read_at]),
            ];
            tcp_write_all_vectored(&mut upstream, &mut iov, "custody final read arena")?;
        } else {
            let writes = if frame.op == ZCNBLK_FAN_WAL_OP_WRITE_DESC {
                frame.payload_len as usize
            } else {
                0
            };
            upstream.read_exact(&mut arena.bytes_mut()[..writes])?;
            let result = execute_frontend(
                &mut client,
                frame,
                &arena.bytes()[..writes],
                read_arena.bytes_mut(),
                negotiated,
            )?;
            wal_failover::write_frame(
                &mut upstream,
                result,
                &read_arena.bytes()[..result.payload_len as usize],
            )?;
        }
    }
    finish_client(&mut client)
}

fn finish_client(client: &mut Client) -> io::Result<()> {
    client.sync()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while client.tracker.progress().remote_redundant < client.journal.durable_hwm()
        || client.rebuild.is_some()
    {
        client.collect()?;
        client.finish_copy()?;
        if Instant::now() >= deadline {
            return Err(io::Error::other("final mirror redundancy drain timed out"));
        }
        wait_readable(&client.route.stream, 10)?;
    }
    println!(
        "custody-live-complete: writes={} early_middle={} early_third={} retained_reads={} remote_reads={} rebuilt={} writes_during_copy={} released={} frontend_sessions=1 frontend_reconnects=0",
        client.journal.durable_hwm(),
        client.early_middle,
        client.early_tail,
        client.overlay_reads,
        client.remote_reads,
        client.rebuilt,
        client.writes_during_copy,
        client.journal.released_hwm()
    );
    Ok(())
}

pub(super) fn cli(mut args: impl Iterator<Item = String>) -> io::Result<()> {
    let usage = || io::Error::other("usage: zcnblk-wal-custody client|peer|inspect CONFIG.json");
    let role = args.next().ok_or_else(usage)?;
    let path = args.next().ok_or_else(usage)?;
    if args.next().is_some() {
        return Err(usage());
    }
    let bytes = fs::read(path)?;
    match role.as_str() {
        "client" => serve_frontend(serde_json::from_slice(&bytes).map_err(io::Error::other)?),
        "peer" => peer::serve(serde_json::from_slice(&bytes).map_err(io::Error::other)?),
        "inspect" => peer::inspect(serde_json::from_slice(&bytes).map_err(io::Error::other)?),
        _ => Err(usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_arena_frontend_refuses_partial_pages_overflow_and_foreign_lanes() {
        let valid = ZcnblkFanWalFrame {
            op: ZCNBLK_FAN_WAL_OP_WRITE_DESC,
            lane_id: 0,
            lane_count: 1,
            payload_len: 4096,
            leaf_offset: 0,
            ..ZcnblkFanWalFrame::default()
        };
        check_frontend(valid, 8192).unwrap();
        for bad in [
            ZcnblkFanWalFrame {
                payload_len: 1,
                ..valid
            },
            ZcnblkFanWalFrame {
                payload_len: 0,
                ..valid
            },
            ZcnblkFanWalFrame {
                payload_len: 128 * 1024,
                ..valid
            },
            ZcnblkFanWalFrame {
                leaf_offset: 1,
                ..valid
            },
            ZcnblkFanWalFrame {
                leaf_offset: 8192,
                ..valid
            },
            ZcnblkFanWalFrame {
                leaf_offset: u64::MAX - 4095,
                ..valid
            },
            ZcnblkFanWalFrame {
                lane_count: 2,
                ..valid
            },
            ZcnblkFanWalFrame {
                lane_id: 1,
                ..valid
            },
        ] {
            assert!(check_frontend(bad, 8192).is_err());
            assert!(
                check_frontend(
                    ZcnblkFanWalFrame {
                        op: ZCNBLK_FAN_WAL_OP_READ_DESC,
                        ..bad
                    },
                    8192
                )
                .is_err()
            );
        }
    }

    #[test]
    fn fragmented_receipts_do_not_block_the_other_race_winner() {
        let config: Config = serde_json::from_str(include_str!(
            "../tests/fixtures/client-wal/live-client.json"
        ))
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut sender, _) = listener.accept().unwrap();
        let mut replies = Replies::new(stream);
        let owner = config.tail.owner;
        let ack = Header {
            replica: owner.id,
            incarnation: owner.incarnation,
            ..Header::request(config.policy.scope, peer::ACK, 1, 1)
        }
        .encode();
        sender.write_all(&ack[..17]).unwrap();
        assert!(wait_readable(&replies.stream, 500).unwrap());
        assert!(replies.next(config.policy.scope).unwrap().is_none());
        assert_eq!(replies.used, 17);
        sender.write_all(&ack[17..]).unwrap();
        assert!(wait_readable(&replies.stream, 500).unwrap());
        let received = replies.next(config.policy.scope).unwrap().unwrap();
        received.validate_ack(owner, 1).unwrap();
        let mut commitment = Commitment::new(config.policy).unwrap();
        commitment.submitted(1).unwrap();
        commitment
            .durable(DurableReceipt {
                scope: config.policy.scope,
                replica: config.policy.local.id,
                incarnation: config.policy.local.incarnation,
                through: 1,
            })
            .unwrap();
        commitment
            .durable(DurableReceipt {
                scope: config.policy.scope,
                replica: received.replica,
                incarnation: received.incarnation,
                through: received.sequence,
            })
            .unwrap();
        assert_eq!(commitment.progress().acknowledged, 1);
        assert_eq!(commitment.progress().remote_durable, [0, 1]);
        assert_eq!(commitment.redundant_prefix().through(), 0);
    }

    #[test]
    fn compaction_preserves_last_overwrite_and_never_releases_early_ack_custody() {
        use std::os::unix::fs::FileExt;
        let config: Config = serde_json::from_str(include_str!(
            "../tests/fixtures/client-wal/live-client.json"
        ))
        .unwrap();
        let base = env::var_os("ZC_CLIENT_WAL_TEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(env::temp_dir);
        let path = base.join(format!("zc-live-compact-{}.wal", std::process::id()));
        let mut journal = Journal::open(
            &path,
            1024 * 1024,
            config.policy.scope,
            config.policy.local,
            BackingIoMode::Direct,
        )
        .unwrap();
        let mut data = Arena::new(64 * 1024).unwrap();
        let mut tracker = Commitment::new(config.policy).unwrap();
        for (offset, length, value) in [(0, 65536, 0x11), (4096, 8192, 0x22), (8192, 4096, 0x33)] {
            data.bytes_mut().fill(value);
            let seq = journal
                .append(
                    &[Record {
                        logical_offset: offset,
                        length,
                    }],
                    &[IoSlice::new(&data.bytes()[..length as usize])],
                )
                .unwrap();
            tracker.submitted(seq).unwrap();
            tracker.durable(journal.commit().unwrap()).unwrap();
        }
        let (bytes, pages) = compact_tail(&journal, 0, 3).unwrap();
        assert_eq!(bytes, 65536 + 8192 + 4096);
        assert_eq!(pages.len(), 16);
        let saved = journal.replay(2, 2).unwrap().next().unwrap();
        let file = std::fs::File::from(journal.file().try_clone_to_owned().unwrap());
        for (page, physical) in pages {
            file.read_exact_at(&mut data.bytes_mut()[..4096], physical)
                .unwrap();
            let expected = match page {
                1 => 0x22,
                2 => 0x33,
                _ => 0x11,
            };
            assert!(data.bytes()[..4096].iter().all(|b| *b == expected));
        }
        tracker
            .durable(DurableReceipt {
                scope: config.policy.scope,
                replica: config.tail.owner.id,
                incarnation: config.tail.owner.incarnation,
                through: 3,
            })
            .unwrap();
        assert_eq!(tracker.progress().acknowledged, 3);
        journal
            .release_remote_prefix(&tracker.redundant_prefix())
            .unwrap();
        assert_eq!(journal.released_hwm(), 0);
        journal
            .read_retained(saved, 0, &mut data.bytes_mut()[..4096])
            .unwrap();
        tracker
            .durable(DurableReceipt {
                scope: config.policy.scope,
                replica: config.middle.owner.id,
                incarnation: config.middle.owner.incarnation,
                through: 3,
            })
            .unwrap();
        journal
            .release_remote_prefix(&tracker.redundant_prefix())
            .unwrap();
        assert_eq!(journal.released_hwm(), 3);
        assert!(
            journal
                .read_retained(saved, 0, &mut data.bytes_mut()[..4096])
                .is_err()
        );
        assert!(compact_tail(&journal, 0, 3).is_err());
        drop(file);
        drop(journal);
        fs::remove_file(path).unwrap();
    }
}
