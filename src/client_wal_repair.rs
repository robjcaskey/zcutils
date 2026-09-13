//! Cold serial-mirror recovery after losing the middle host.
//!
//! The live source fences its old connections and durably drains every admitted
//! local record before entering here. A retained suffix is NOT a full replica:
//! the replacement pulls the sealed surviving image directly from the survivor,
//! then the source sends its retained suffix to both terminals with sendfile.
//! The survivor's known ACK is a LOWER bound, not an inferred on-disk sequence.
//! Replaying the entire suffix through the fenced source HWM is intentional: the
//! survivor may contain newer writes whose ACK was lost, including overwrites.
//!
//! The small TCP adapter is for private, authority-bound recovery connections.
//! It is not an authenticated public listener. Production callers must supply
//! authenticated/fenced connections; scope checks do not authenticate a peer.

use crate::client_wal_commitment::{CommitScope, Commitment, DurableReceipt, Replica};
use crate::client_wal_journal::Journal;
use crate::topology::{CustodyState, DurabilityObligation};
use crate::topology_controller::{EvolutionController, NodePlacement, ReplicaPlacement};
use crate::*;
use serde::Deserialize;
use std::collections::BTreeMap;

const MAGIC: &[u8; 8] = b"ZCREPR01";
const HEADER: usize = 80;
const CHUNK: usize = 1024 * 1024;
const SEAL: u64 = 1;
const COPY: u64 = 2;
const WRITE: u64 = 3;
const COMMIT: u64 = 4;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub survivor_control: String,
    pub replacement_control: String,
    pub replacement: Replica,
    pub controller_directory: PathBuf,
    pub logical_bytes: u64,
    /// Cold provisioning can outlive a normal I/O timeout; bounded separately.
    #[serde(default = "default_replacement_wait")]
    pub replacement_wait_seconds: u64,
}

fn default_replacement_wait() -> u64 {
    60
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TerminalConfig {
    pub scope: CommitScope,
    pub owner: Replica,
    pub bind: String,
    pub copy_endpoint: String,
    pub survivor: bool,
    pub target: String,
    pub logical_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct Header {
    scope: CommitScope,
    op: u64,
    first: u64,
    through: u64,
    offset: u64,
    length: u64,
}

impl Header {
    fn write(self, stream: &mut TcpStream) -> io::Result<()> {
        let mut bytes = [0u8; HEADER];
        bytes[..8].copy_from_slice(MAGIC);
        let fields = [
            self.scope.volume,
            self.scope.log,
            self.scope.writer_epoch,
            u64::from(self.scope.lane),
            self.op,
            self.first,
            self.through,
            self.offset,
            self.length,
        ];
        for (index, field) in fields.iter().enumerate() {
            bytes[8 + index * 8..16 + index * 8].copy_from_slice(&field.to_le_bytes());
        }
        stream.write_all(&bytes)
    }

    fn read(stream: &mut TcpStream, scope: CommitScope) -> io::Result<Self> {
        let mut bytes = [0u8; HEADER];
        stream.read_exact(&mut bytes)?;
        let get = |i: usize| u64::from_le_bytes(bytes[8 + i * 8..16 + i * 8].try_into().unwrap());
        if &bytes[..8] != MAGIC
            || get(0) != scope.volume
            || get(1) != scope.log
            || get(2) != scope.writer_epoch
            || get(3) != u64::from(scope.lane)
        {
            return Err(io::Error::other("foreign recovery scope or wire version"));
        }
        Ok(Self {
            scope,
            op: get(4),
            first: get(5),
            through: get(6),
            offset: get(7),
            length: get(8),
        })
    }
}

fn socket(address: &str) -> io::Result<TcpStream> {
    socket_wait(address, 60)
}

fn socket_wait(address: &str, seconds: u64) -> io::Result<TcpStream> {
    if !(1..=1800).contains(&seconds) {
        return Err(io::Error::other(
            "repair connect deadline must be 1..1800 seconds",
        ));
    }
    // Numeric addresses: no unbounded DNS while holding custody.
    let addr: SocketAddr = address.parse().map_err(io::Error::other)?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
            Ok(stream) => {
                configure(&stream)?;
                return Ok(stream);
            }
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn configure(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))
}

fn receipt(stream: &mut TcpStream, request: Header, owner: Replica) -> io::Result<()> {
    let reply = Header::read(stream, request.scope)?;
    if reply.op != request.op
        || reply.first != owner.id
        || reply.offset != owner.incarnation
        || reply.through != request.through
        || reply.length != request.length
    {
        return Err(io::Error::other(
            "recovery receipt does not match the bound incarnation/fence",
        ));
    }
    Ok(())
}

fn reply(stream: &mut TcpStream, request: Header, owner: Replica) -> io::Result<()> {
    Header {
        first: owner.id,
        offset: owner.incarnation,
        ..request
    }
    .write(stream)
}

pub(super) fn placement(scope: CommitScope, replica: Replica) -> ReplicaPlacement {
    ReplicaPlacement {
        replica_id: format!("replica-{}-incarnation-{}", replica.id, replica.incarnation),
        node: NodePlacement {
            node_id: format!(
                "domain-{}-replica-{}-incarnation-{}",
                replica.failure_domain, replica.id, replica.incarnation
            ),
            region: "serial-test-region".into(),
            az: "serial-test-az".into(),
            tier: "persistent-terminal".into(),
            cost_class: 0,
            durability_role: "full-replica".into(),
            available: true,
        },
        group_id: format!("volume-{}", scope.volume),
        log_id: format!("log-{}-epoch-{}", scope.log, scope.writer_epoch),
    }
}

/// Called only with the source lane fenced. Borrowing the journal for the
/// transfer prevents its ring being reclaimed/reused while sendfile is active.
pub(super) fn rebuild(
    config: &Config,
    journal: &mut Journal,
    tracker: &mut Commitment,
) -> io::Result<()> {
    let policy = tracker.policy();
    let before = tracker.progress();
    if journal.scope() != policy.scope
        || journal.durable_hwm() != before.submitted
        || before.local_durable != before.submitted
        || config.logical_bytes == 0
        || config.logical_bytes % 4096 != 0
    {
        return Err(io::Error::other(
            "repair requires a fully drained, fenced source lane",
        ));
    }
    let floor = before.remote_durable[1];
    let through = before.local_durable;
    if floor < journal.released_hwm() || floor > through {
        return Err(io::Error::other(
            "surviving full image does not cover reclaimed client prefix",
        ));
    }
    // Validate the entire replay schedule before any destructive preparation.
    for record in journal.replay(floor + 1, through)? {
        if record
            .logical_offset
            .checked_add(record.length)
            .is_none_or(|end| end > config.logical_bytes)
        {
            return Err(io::Error::other(
                "retained replay exceeds replacement volume",
            ));
        }
    }
    fs::create_dir_all(&config.controller_directory)?;
    let controller =
        EvolutionController::open(config.controller_directory.join("topology.ndjson"), 1)?;
    if controller.state().applied_index != 0 {
        return Err(io::Error::other(
            "existing repair transaction requires reconciliation, not reset",
        ));
    }
    let old = placement(policy.scope, policy.remote[0]);
    let survivor = placement(policy.scope, policy.remote[1]);
    let replacement = placement(policy.scope, config.replacement);
    let obligation = DurabilityObligation {
        obligation_id: "serial-two-full-replicas".into(),
        group_id: old.group_id.clone(),
        required_copies: 2,
        distinct: BTreeMap::from([("failure.host".into(), 2)]),
        required_roles: BTreeMap::from([("full-replica".into(), 2)]),
    };
    let lane = policy.scope.lane;
    controller.bootstrap(
        "serial-bootstrap",
        obligation,
        &[
            (
                old.clone(),
                BTreeMap::from([(lane, before.remote_durable[0])]),
            ),
            (survivor.clone(), BTreeMap::from([(lane, floor)])),
        ],
    )?;
    controller.set_available("middle-lost", &old.node.node_id, false)?;
    tracker.replace_remote(0, config.replacement)?;
    let handoff = controller.stage_replica(
        "stage-middle-replacement",
        &survivor.replica_id,
        &replacement,
        BTreeMap::from([(lane, through)]),
    )?;
    if controller.state().custody[&replacement.replica_id].state != CustodyState::Staged {
        return Err(io::Error::other("replacement was counted before copying"));
    }
    eprintln!(
        "client-wal-repair-staged: lane={lane} old_incarnation={} new_incarnation={} acknowledged={} reclaimed_prefix={} survivor_known_hwm={floor} fenced_hwm={through} placement_owner=userspace-raid control_store=committed-state-machine raft_quorum_test=false",
        policy.remote[0].incarnation,
        config.replacement.incarnation,
        before.acknowledged,
        journal.released_hwm()
    );
    let mut source = socket(&config.survivor_control)?;
    let seal = Header {
        scope: policy.scope,
        op: SEAL,
        first: floor,
        through,
        offset: 0,
        length: config.logical_bytes,
    };
    seal.write(&mut source)?;
    receipt(&mut source, seal, policy.remote[1])?;
    let mut destination =
        socket_wait(&config.replacement_control, config.replacement_wait_seconds)?;
    let copy = Header { op: COPY, ..seal };
    copy.write(&mut destination)?;
    receipt(&mut destination, copy, config.replacement)?;
    // Newer unacknowledged values in the sealed survivor are okay ONLY because
    // every possible admitted write after `floor` is replayed to BOTH copies.
    let mut replay_bytes = 0u64;
    for record in journal.replay(floor + 1, through)? {
        let command = Header {
            scope: policy.scope,
            op: WRITE,
            first: record.sequence,
            through,
            offset: record.logical_offset,
            length: record.length,
        };
        for stream in [&mut destination, &mut source] {
            command.write(stream)?;
            send_file_extent(stream, journal, record.file_offset, record.length)?;
        }
        replay_bytes += record.length;
    }
    let commit = Header { op: COMMIT, ..seal };
    commit.write(&mut destination)?;
    receipt(&mut destination, commit, config.replacement)?;
    commit.write(&mut source)?;
    receipt(&mut source, commit, policy.remote[1])?;
    controller.activate_copied_replica(
        "activate-middle-after-durable-copy",
        &handoff,
        BTreeMap::from([(lane, through)]),
    )?;
    let survivor_lease = controller.state().custody[&survivor.replica_id].clone();
    controller.commit(
        "survivor-caught-up",
        vec![crate::topology::TopologyCommand::AdvanceCustodyHwm {
            replica_id: survivor.replica_id,
            term: survivor_lease.term,
            lane_hwms: BTreeMap::from([(lane, through)]),
        }],
    )?;
    controller.state().verify_coverage(
        "serial-two-full-replicas",
        &BTreeMap::from([(lane, through)]),
    )?;
    for replica in [config.replacement, policy.remote[1]] {
        tracker.durable(DurableReceipt {
            scope: policy.scope,
            replica: replica.id,
            incarnation: replica.incarnation,
            through,
        })?;
    }
    journal.release_remote_prefix(&tracker.redundant_prefix())?;
    eprintln!(
        "client-wal-repair-complete: lane={lane} base_copy_bytes={} replay_bytes={replay_bytes} replay_copies=2 source_payload_copy_bytes=0 middle_payload_copy_bytes=0 remote_redundant_hwm={} acknowledged_before_failure={} replacement_active=true",
        config.logical_bytes,
        tracker.progress().remote_redundant,
        before.acknowledged
    );
    Ok(())
}

fn send_file_extent(
    stream: &TcpStream,
    journal: &Journal,
    offset: u64,
    bytes: u64,
) -> io::Result<()> {
    let mut cursor = i64::try_from(offset).map_err(io::Error::other)?;
    let mut remaining = bytes;
    while remaining != 0 {
        let sent = unsafe {
            libc::sendfile(
                stream.as_raw_fd(),
                journal.file().as_raw_fd(),
                &mut cursor,
                remaining.min(0x7fff_f000) as usize,
            )
        };
        if sent < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if sent == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        remaining -= sent as u64;
    }
    Ok(())
}

pub(super) fn serve(
    config: TerminalConfig,
    listener: TcpListener,
    terminal: ZcRaidMirrorTerminal,
) -> io::Result<()> {
    terminal.validate_persistent_receipts()?;
    // CLI runs only after the old ingress has stopped. The persistent WAL's
    // exclusive file lock also prevents starting this beside the old writer.
    let copy_listener = if config.survivor {
        Some(TcpListener::bind(&config.copy_endpoint)?)
    } else {
        None
    };
    let (mut control, _) = listener.accept()?;
    configure(&control)?;
    let start = Header::read(&mut control, config.scope)?;
    if start.op != if config.survivor { SEAL } else { COPY }
        || start.first > start.through
        || start.length != config.logical_bytes
        || start.offset != 0
    {
        return Err(io::Error::other("invalid repair seal/copy contract"));
    }
    terminal.sync()?;
    let storage = FixedSendBuffers::new(1, CHUNK)?;
    let buffer = unsafe { slice::from_raw_parts_mut(storage.ptr(0), CHUNK) };
    let mut ring = terminal.ring()?;
    if let Some(listener) = copy_listener {
        reply(&mut control, start, config.owner)?;
        let (mut copy, _) = listener.accept()?;
        configure(&copy)?;
        let request = Header::read(&mut copy, config.scope)?;
        if request.op != COPY
            || request.first != start.first
            || request.through != start.through
            || request.length != config.logical_bytes
        {
            return Err(io::Error::other(
                "copy attempted outside the sealed generation",
            ));
        }
        for offset in (0..config.logical_bytes).step_by(CHUNK) {
            let bytes = CHUNK.min((config.logical_bytes - offset) as usize);
            match terminal.backend.as_ref() {
                ZcnblkWalLeafBackend::PersistentJournal { store, .. } => {
                    store.read_at(offset, &mut buffer[..bytes])?
                }
                _ => {
                    return Err(io::Error::other(
                        "sealed repair currently requires a userspace persistent WAL terminal",
                    ));
                }
            }
            copy.write_all(&buffer[..bytes])?;
        }
    } else {
        if let ZcnblkWalLeafBackend::PersistentJournal { store, .. } = terminal.backend.as_ref() {
            if store.stats().appended_sequence != 0 {
                return Err(io::Error::other(
                    "refusing to overwrite a nonempty replacement WAL",
                ));
            }
        } else {
            return Err(io::Error::other(
                "replacement must use a persistent WAL terminal",
            ));
        }
        let mut copy = socket(&config.copy_endpoint)?;
        start.write(&mut copy)?;
        for offset in (0..config.logical_bytes).step_by(CHUNK) {
            let bytes = CHUNK.min((config.logical_bytes - offset) as usize);
            copy.read_exact(&mut buffer[..bytes])?;
            write_payload(&terminal, &mut ring, offset, &buffer[..bytes])?;
        }
        terminal.sync()?;
        reply(&mut control, start, config.owner)?;
    }
    let mut next = start.first + 1;
    loop {
        let command = Header::read(&mut control, config.scope)?;
        if command.through != start.through {
            return Err(io::Error::other("replay fence changed"));
        }
        match command.op {
            WRITE => {
                if command.first != next
                    || next > start.through
                    || command.length == 0
                    || command.length % 4096 != 0
                    || command.offset % 4096 != 0
                    || command
                        .offset
                        .checked_add(command.length)
                        .is_none_or(|end| end > config.logical_bytes)
                {
                    return Err(io::Error::other(
                        "replay sequence hole or invalid logical range",
                    ));
                }
                for within in (0..command.length).step_by(CHUNK) {
                    let bytes = CHUNK.min((command.length - within) as usize);
                    control.read_exact(&mut buffer[..bytes])?;
                    write_payload(
                        &terminal,
                        &mut ring,
                        command.offset + within,
                        &buffer[..bytes],
                    )?;
                }
                next += 1;
            }
            COMMIT => {
                if next != start.through + 1
                    || command.first != start.first
                    || command.length != start.length
                    || command.offset != 0
                {
                    return Err(io::Error::other(
                        "repair cannot commit an incomplete replay prefix",
                    ));
                }
                terminal.sync()?;
                reply(&mut control, command, config.owner)?;
                eprintln!(
                    "client-wal-repair-terminal-complete: replica={} incarnation={} through={} payload_copy_bytes=0",
                    config.owner.id, config.owner.incarnation, start.through
                );
                return Ok(());
            }
            _ => return Err(io::Error::other("unknown repair operation")),
        }
    }
}

fn write_payload(
    terminal: &ZcRaidMirrorTerminal,
    _ring: &mut Option<RawRing>,
    offset: u64,
    payload: &[u8],
) -> io::Result<()> {
    match terminal.backend.as_ref() {
        ZcnblkWalLeafBackend::PersistentJournal { store, .. } => {
            store.append_contiguous(offset, payload)?;
            // Bound the replay backlog; reducer works while the next chunk is
            // in flight. This cold recovery gate is not an application I/O gate.
            store.sync()?;
            Ok(())
        }
        _ => Err(io::Error::other(
            "repair payload requires a persistent WAL terminal",
        )),
    }
}

pub(super) fn cli(mut args: impl Iterator<Item = String>) -> io::Result<()> {
    let path = args
        .next()
        .ok_or_else(|| io::Error::other("usage: zcraid-repair-terminal CONFIG.json"))?;
    if args.next().is_some() {
        return Err(io::Error::other("unexpected repair arguments"));
    }
    let config: TerminalConfig =
        serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
    if config.logical_bytes == 0 || config.logical_bytes % 4096 != 0 {
        return Err(io::Error::other("invalid repair volume size"));
    }
    let terminal = ZcRaidMirrorTerminal::open(&config.target, 4096, config.logical_bytes)?;
    let listener = TcpListener::bind(&config.bind)?;
    eprintln!(
        "client-wal-repair-terminal-ready: replica={} incarnation={} endpoint={} survivor={} old_ingress_stopped=true",
        config.owner.id, config.owner.incarnation, config.bind, config.survivor
    );
    serve(config, listener, terminal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_wal_commitment::Policy;
    use crate::client_wal_journal::Record;
    use crate::persistent_wal::{BackingIoMode, IntegrityMode, PersistentWalRuntime};

    fn terminal(path: &Path, name: &str) -> ZcRaidMirrorTerminal {
        let store = PersistentWalRuntime::open_with_integrity(
            path.join(format!("{name}.wal")),
            path.join(format!("{name}.base")),
            65536,
            4 * 1024 * 1024,
            IntegrityMode::Frame,
        )
        .unwrap();
        ZcRaidMirrorTerminal {
            backend: Arc::new(ZcnblkWalLeafBackend::PersistentJournal {
                label: name.into(),
                store,
                device_bytes: 65536,
            }),
            io_mode: ZcnblkWalLeafIoMode::Blocking,
            allow_volatile_sync: false,
        }
    }

    #[test]
    fn reconstructs_reclaimed_prefix_and_overwrites_with_lost_survivor_acks() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = env::var_os("ZC_CLIENT_WAL_TEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/tmp"));
        let path = root.join(format!(
            "zc-client-repair-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let scope = CommitScope {
            volume: 41,
            log: 62,
            writer_epoch: 3,
            lane: 0,
        };
        let replicas = [1, 2, 3].map(|id| Replica {
            id,
            incarnation: 1,
            failure_domain: id,
        });
        let policy = Policy {
            scope,
            count_client_local_wal: true,
            local: replicas[0],
            remote: [replicas[1], replicas[2]],
        };
        let mut tracker = Commitment::new(policy).unwrap();
        let mut journal = Journal::open(
            &path.join("client.wal"),
            256 * 1024,
            scope,
            replicas[0],
            BackingIoMode::Direct,
        )
        .unwrap();
        let survivor = terminal(&path, "survivor");
        let replacement = terminal(&path, "replacement");
        let storage = FixedSendBuffers::new(1, 4096).unwrap();
        let buffer = unsafe { slice::from_raw_parts_mut(storage.ptr(0), 4096) };
        let mut expected = vec![0u8; 65536];
        let offsets = [
            0, 4096, 8192, 12288, 0, 8192, 16384, 0, 24576, 4096, 8192, 4096,
        ];
        for (index, offset) in offsets.into_iter().enumerate() {
            let seq = index as u64 + 1;
            buffer.fill((seq * 17) as u8);
            expected[offset as usize..offset as usize + 4096].copy_from_slice(buffer);
            tracker
                .submitted(
                    journal
                        .append(
                            &[Record {
                                logical_offset: offset,
                                length: 4096,
                            }],
                            &[IoSlice::new(buffer)],
                        )
                        .unwrap(),
                )
                .unwrap();
            tracker.durable(journal.commit().unwrap()).unwrap();
            tracker
                .durable(DurableReceipt {
                    scope,
                    replica: 2,
                    incarnation: 1,
                    through: seq,
                })
                .unwrap();
            // S really has six writes, but C only observed acknowledgements up
            // through four. Two of these newer values overwrite old pages.
            if seq <= 6 {
                survivor.backend.write_at(offset, buffer).unwrap();
                survivor.sync().unwrap();
            }
            if seq <= 4 {
                tracker
                    .durable(DurableReceipt {
                        scope,
                        replica: 3,
                        incarnation: 1,
                        through: seq,
                    })
                    .unwrap();
            }
            if seq == 4 {
                journal
                    .release_remote_prefix(&tracker.redundant_prefix())
                    .unwrap();
            }
        }
        assert_eq!(journal.released_hwm(), 4);
        assert!(
            journal.replay(1, 12).is_err(),
            "test must really require the surviving base"
        );
        assert_eq!(tracker.progress().acknowledged, 12);
        let source_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let copy_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let copy_endpoint = copy_listener.local_addr().unwrap().to_string();
        drop(copy_listener);
        let new_replica = Replica {
            incarnation: 2,
            failure_domain: 4,
            ..replicas[1]
        };
        let config = Config {
            survivor_control: source_listener.local_addr().unwrap().to_string(),
            replacement_control: target_listener.local_addr().unwrap().to_string(),
            replacement: new_replica,
            controller_directory: path.join("controller"),
            logical_bytes: 65536,
            replacement_wait_seconds: 60,
        };
        let source_cfg = TerminalConfig {
            scope,
            owner: replicas[2],
            bind: config.survivor_control.clone(),
            copy_endpoint: copy_endpoint.clone(),
            survivor: true,
            target: String::new(),
            logical_bytes: 65536,
        };
        let target_cfg = TerminalConfig {
            scope,
            owner: new_replica,
            bind: config.replacement_control.clone(),
            copy_endpoint,
            survivor: false,
            target: String::new(),
            logical_bytes: 65536,
        };
        let source_reader = survivor.clone();
        let target_reader = replacement.clone();
        let source_thread = thread::spawn(move || serve(source_cfg, source_listener, survivor));
        let target_thread = thread::spawn(move || serve(target_cfg, target_listener, replacement));
        rebuild(&config, &mut journal, &mut tracker).unwrap();
        source_thread.join().unwrap().unwrap();
        target_thread.join().unwrap().unwrap();
        for terminal in [&source_reader, &target_reader] {
            assert_eq!(terminal.backend.read_at(0, 65536).unwrap(), expected);
        }
        assert_eq!(tracker.progress().remote_redundant, 12);
        assert_eq!(journal.released_hwm(), 12);
        assert!(
            tracker
                .durable(DurableReceipt {
                    scope,
                    replica: 2,
                    incarnation: 1,
                    through: 12
                })
                .is_err()
        );
        let controller =
            EvolutionController::open(config.controller_directory.join("topology.ndjson"), 1)
                .unwrap();
        let state = controller.state();
        assert_eq!(
            state.custody["replica-2-incarnation-2"].state,
            CustodyState::Active
        );
        state
            .verify_coverage("serial-two-full-replicas", &BTreeMap::from([(0, 12)]))
            .unwrap();
        drop(controller);
        drop(source_reader);
        drop(target_reader);
        drop(journal);
        fs::remove_dir_all(path).unwrap();
    }
}
