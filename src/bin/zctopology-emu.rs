use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;
use zcutils::ha_metadata::{
    CommittedHaBatch, DataReplica, DataReplicaRole, DurabilityPolicy, GroupConfig, HaCommand,
    HaMetadataStore, ReplicaHwm,
};
use zcutils::topology::CustodyState;
use zcutils::topology_controller::{
    EvolutionController, NodePlacement, ReplicaPlacement, fact_str, fact_u64, region_obligation,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NodeState {
    node_id: String,
    region: String,
    az: String,
    tier: String,
    cost_class: u64,
    durable_hwms: BTreeMap<String, Vec<(u32, u64)>>,
    #[serde(default)]
    dataset_sequence: u64,
    #[serde(default)]
    dataset: BTreeMap<String, String>,
    #[serde(default)]
    dataset_wal: Vec<DatasetMutation>,
    #[serde(default)]
    snapshots: BTreeMap<String, DatasetSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DatasetMutation {
    sequence: u64,
    key: String,
    value: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DatasetSnapshot {
    snapshot_id: String,
    through_sequence: u64,
    values: BTreeMap<String, String>,
    digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RecoveryBundle {
    snapshot: DatasetSnapshot,
    target_sequence: u64,
    mutations: Vec<DatasetMutation>,
    expected_values: BTreeMap<String, String>,
    expected_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DatasetView {
    sequence: u64,
    values: BTreeMap<String, String>,
    digest: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
enum Request {
    Probe,
    ReadHwm {
        group_id: String,
    },
    Replicate {
        group_id: String,
        lane_hwms: Vec<(u32, u64)>,
    },
    SetTier {
        tier: String,
        cost_class: u64,
    },
    ApplyMutations {
        mutations: Vec<DatasetMutation>,
    },
    CaptureDatasetSnapshot {
        snapshot_id: String,
        through_sequence: u64,
    },
    ExportRecovery {
        snapshot_id: String,
        target_sequence: u64,
    },
    RestoreRecovery {
        bundle: RecoveryBundle,
    },
    ReadDataset,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    ok: bool,
    message: String,
    state: Option<NodeState>,
    lane_hwms: Option<Vec<(u32, u64)>>,
    payload: Option<serde_json::Value>,
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("node") if args.len() == 9 => run_node(
            &args[2],
            NodeState {
                node_id: args[3].clone(),
                region: args[4].clone(),
                az: args[5].clone(),
                tier: args[6].clone(),
                cost_class: args[7].parse().map_err(|_| invalid("invalid cost class"))?,
                durable_hwms: BTreeMap::new(),
                dataset_sequence: 0,
                dataset: BTreeMap::new(),
                dataset_wal: Vec::new(),
                snapshots: BTreeMap::new(),
            },
            Path::new(&args[8]),
        ),
        Some("scenario") if args.len() == 8 => run_scenario(
            Path::new(&args[2]),
            [&args[3], &args[4], &args[5], &args[6], &args[7]],
        ),
        _ => Err(invalid(
            "usage: zctopology-emu node LISTEN ID REGION AZ TIER COST STATE | scenario LOG A B COLD_C REPL_B HOT_C",
        )),
    }
}

fn run_node(listen: &str, mut state: NodeState, state_path: &Path) -> io::Result<()> {
    if state_path.exists() {
        state = serde_json::from_reader(File::open(state_path)?)
            .map_err(|error| invalid(format!("read node state: {error}")))?;
    }
    persist_state(state_path, &state)?;
    let listener = TcpListener::bind(listen)?;
    println!(
        "TOPOLOGY_NODE_READY id={} listen={} region={} tier={}",
        state.node_id, listen, state.region, state.tier
    );
    for incoming in listener.incoming() {
        let mut stream = incoming?;
        stream.set_nodelay(true)?;
        let mut request_line = String::new();
        if BufReader::new(stream.try_clone()?).read_line(&mut request_line)? == 0 {
            continue;
        }
        let request: Request = match serde_json::from_str(&request_line) {
            Ok(request) => request,
            Err(error) => {
                let response = Response {
                    ok: false,
                    message: format!("decode request: {error}"),
                    state: None,
                    lane_hwms: None,
                    payload: None,
                };
                if serde_json::to_writer(&mut stream, &response).is_ok() {
                    let _ = stream.write_all(b"\n");
                }
                continue;
            }
        };
        let mut stop = false;
        let response =
            match request {
                Request::Probe => Response {
                    ok: true,
                    message: "ready".into(),
                    state: Some(state.clone()),
                    lane_hwms: None,
                    payload: None,
                },
                Request::ReadHwm { group_id } => Response {
                    ok: true,
                    message: "durable_hwm".into(),
                    state: None,
                    lane_hwms: Some(
                        state
                            .durable_hwms
                            .get(&group_id)
                            .cloned()
                            .unwrap_or_default(),
                    ),
                    payload: None,
                },
                Request::Replicate {
                    group_id,
                    lane_hwms,
                } => {
                    let current = state.durable_hwms.entry(group_id).or_default();
                    let mut by_lane: BTreeMap<u32, u64> = current.iter().copied().collect();
                    for (lane, hwm) in lane_hwms {
                        let old = by_lane.entry(lane).or_default();
                        if hwm < *old {
                            return Err(invalid("node HWM regression"));
                        }
                        *old = hwm;
                    }
                    *current = by_lane.into_iter().collect();
                    let persisted_hwms = current.clone();
                    persist_state(state_path, &state)?;
                    Response {
                        ok: true,
                        message: "replicated_and_synced".into(),
                        state: None,
                        lane_hwms: Some(persisted_hwms),
                        payload: None,
                    }
                }
                Request::SetTier { tier, cost_class } => {
                    state.tier = tier;
                    state.cost_class = cost_class;
                    persist_state(state_path, &state)?;
                    Response {
                        ok: true,
                        message: "tier_changed".into(),
                        state: Some(state.clone()),
                        lane_hwms: None,
                        payload: None,
                    }
                }
                Request::ApplyMutations { mutations } => {
                    apply_mutations(
                        &mut state.dataset_sequence,
                        &mut state.dataset,
                        &mut state.dataset_wal,
                        &mutations,
                    )?;
                    persist_state(state_path, &state)?;
                    Response {
                        ok: true,
                        message: "dataset_wal_synced".into(),
                        state: None,
                        lane_hwms: None,
                        payload: Some(
                            serde_json::to_value(dataset_view(&state)).map_err(|error| {
                                invalid(format!("encode dataset view: {error}"))
                            })?,
                        ),
                    }
                }
                Request::CaptureDatasetSnapshot {
                    snapshot_id,
                    through_sequence,
                } => {
                    if through_sequence != state.dataset_sequence {
                        return Err(invalid(
                            "snapshot cut does not equal the applied dataset sequence",
                        ));
                    }
                    if state.snapshots.contains_key(&snapshot_id) {
                        return Err(invalid("duplicate dataset snapshot"));
                    }
                    let snapshot = DatasetSnapshot {
                        snapshot_id: snapshot_id.clone(),
                        through_sequence,
                        values: state.dataset.clone(),
                        digest: dataset_digest(&state.dataset)?,
                    };
                    state.snapshots.insert(snapshot_id, snapshot.clone());
                    persist_state(state_path, &state)?;
                    Response {
                        ok: true,
                        message: "dataset_snapshot_synced".into(),
                        state: None,
                        lane_hwms: None,
                        payload: Some(serde_json::to_value(snapshot).map_err(|error| {
                            invalid(format!("encode dataset snapshot: {error}"))
                        })?),
                    }
                }
                Request::ExportRecovery {
                    snapshot_id,
                    target_sequence,
                } => {
                    let bundle = export_recovery(&state, &snapshot_id, target_sequence)?;
                    Response {
                        ok: true,
                        message: "recovery_bundle".into(),
                        state: None,
                        lane_hwms: None,
                        payload: Some(serde_json::to_value(bundle).map_err(|error| {
                            invalid(format!("encode recovery bundle: {error}"))
                        })?),
                    }
                }
                Request::RestoreRecovery { bundle } => {
                    restore_recovery(&mut state, &bundle)?;
                    persist_state(state_path, &state)?;
                    Response {
                        ok: true,
                        message: "recovery_restored_and_synced".into(),
                        state: None,
                        lane_hwms: None,
                        payload: Some(serde_json::to_value(dataset_view(&state)).map_err(
                            |error| invalid(format!("encode restored dataset: {error}")),
                        )?),
                    }
                }
                Request::ReadDataset => Response {
                    ok: true,
                    message: "dataset".into(),
                    state: None,
                    lane_hwms: None,
                    payload: Some(
                        serde_json::to_value(dataset_view(&state))
                            .map_err(|error| invalid(format!("encode dataset: {error}")))?,
                    ),
                },
                Request::Shutdown => {
                    stop = true;
                    Response {
                        ok: true,
                        message: "shutdown".into(),
                        state: None,
                        lane_hwms: None,
                        payload: None,
                    }
                }
            };
        if serde_json::to_writer(&mut stream, &response).is_err()
            || stream.write_all(b"\n").is_err()
        {
            continue;
        }
        if stop {
            break;
        }
    }
    println!("TOPOLOGY_NODE_STOP id={}", state.node_id);
    Ok(())
}

fn persist_state(path: &Path, state: &NodeState) -> io::Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, state)
            .map_err(|error| invalid(format!("encode node state: {error}")))?;
        writer.flush()?;
        writer.get_ref().sync_data()?;
    }
    std::fs::rename(tmp, path)?;
    File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
    Ok(())
}

fn apply_mutations(
    sequence: &mut u64,
    values: &mut BTreeMap<String, String>,
    wal: &mut Vec<DatasetMutation>,
    mutations: &[DatasetMutation],
) -> io::Result<()> {
    if mutations.is_empty() {
        return Err(invalid("dataset mutation batch is empty"));
    }
    let mut staged_sequence = *sequence;
    let mut staged_values = values.clone();
    let mut staged_wal = wal.clone();
    for mutation in mutations {
        if mutation.sequence != staged_sequence.saturating_add(1) || mutation.key.is_empty() {
            return Err(invalid(
                "dataset WAL sequence is non-contiguous or key is empty",
            ));
        }
        match &mutation.value {
            Some(value) => {
                staged_values.insert(mutation.key.clone(), value.clone());
            }
            None => {
                staged_values.remove(&mutation.key);
            }
        }
        staged_sequence = mutation.sequence;
        staged_wal.push(mutation.clone());
    }
    *sequence = staged_sequence;
    *values = staged_values;
    *wal = staged_wal;
    Ok(())
}

fn dataset_digest(values: &BTreeMap<String, String>) -> io::Result<String> {
    let encoded = serde_json::to_vec(values)
        .map_err(|error| invalid(format!("encode dataset digest input: {error}")))?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
}

fn dataset_view(state: &NodeState) -> DatasetView {
    DatasetView {
        sequence: state.dataset_sequence,
        values: state.dataset.clone(),
        digest: dataset_digest(&state.dataset).expect("dataset values are JSON encodable"),
    }
}

fn export_recovery(
    state: &NodeState,
    snapshot_id: &str,
    target_sequence: u64,
) -> io::Result<RecoveryBundle> {
    let snapshot = state
        .snapshots
        .get(snapshot_id)
        .ok_or_else(|| invalid(format!("unknown dataset snapshot {snapshot_id}")))?
        .clone();
    if target_sequence < snapshot.through_sequence || target_sequence > state.dataset_sequence {
        return Err(invalid("recovery target is outside the retained WAL range"));
    }
    let mutations: Vec<_> = state
        .dataset_wal
        .iter()
        .filter(|mutation| {
            mutation.sequence > snapshot.through_sequence && mutation.sequence <= target_sequence
        })
        .cloned()
        .collect();
    let mut sequence = snapshot.through_sequence;
    let mut expected_values = snapshot.values.clone();
    let mut replayed = Vec::new();
    if target_sequence > snapshot.through_sequence {
        apply_mutations(
            &mut sequence,
            &mut expected_values,
            &mut replayed,
            &mutations,
        )?;
    }
    if sequence != target_sequence {
        return Err(invalid("retained WAL does not reach the recovery target"));
    }
    let expected_digest = dataset_digest(&expected_values)?;
    Ok(RecoveryBundle {
        snapshot,
        target_sequence,
        mutations,
        expected_values,
        expected_digest,
    })
}

fn restore_recovery(state: &mut NodeState, bundle: &RecoveryBundle) -> io::Result<()> {
    if dataset_digest(&bundle.snapshot.values)? != bundle.snapshot.digest {
        return Err(invalid("base snapshot digest mismatch"));
    }
    let mut sequence = bundle.snapshot.through_sequence;
    let mut values = bundle.snapshot.values.clone();
    let mut wal = Vec::new();
    if bundle.target_sequence > sequence {
        apply_mutations(&mut sequence, &mut values, &mut wal, &bundle.mutations)?;
    }
    if sequence != bundle.target_sequence
        || values != bundle.expected_values
        || dataset_digest(&values)? != bundle.expected_digest
    {
        return Err(invalid(
            "replayed recovery bundle does not match its certified result",
        ));
    }
    state.dataset_sequence = sequence;
    state.dataset = values;
    state.dataset_wal = wal;
    state
        .snapshots
        .insert(bundle.snapshot.snapshot_id.clone(), bundle.snapshot.clone());
    Ok(())
}

fn call(address: &str, request: &Request) -> io::Result<Response> {
    let socket: SocketAddr = address
        .parse()
        .map_err(|_| invalid(format!("invalid address {address}")))?;
    let mut stream = TcpStream::connect_timeout(&socket, Duration::from_millis(300))?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.set_nodelay(true)?;
    serde_json::to_writer(&mut stream, request)
        .map_err(|error| invalid(format!("encode request: {error}")))?;
    stream.write_all(b"\n")?;
    let mut response_line = String::new();
    BufReader::new(stream).read_line(&mut response_line)?;
    let response: Response = serde_json::from_str(&response_line)
        .map_err(|error| invalid(format!("decode response: {error}")))?;
    if !response.ok {
        return Err(invalid(response.message));
    }
    Ok(response)
}

fn placement(replica: &str, node: &NodeState) -> ReplicaPlacement {
    ReplicaPlacement {
        replica_id: replica.into(),
        node: NodePlacement {
            node_id: node.node_id.clone(),
            region: node.region.clone(),
            az: node.az.clone(),
            tier: node.tier.clone(),
            cost_class: node.cost_class,
            durability_role: "leaf".into(),
            available: true,
        },
        group_id: "volume-0".into(),
        log_id: "linear-log-0".into(),
    }
}

fn probe(address: &str) -> io::Result<NodeState> {
    call(address, &Request::Probe)?
        .state
        .ok_or_else(|| invalid("probe omitted node state"))
}

fn copy_hwm(source: &str, target: &str) -> io::Result<BTreeMap<u32, u64>> {
    let lane_hwms: BTreeMap<u32, u64> = call(
        source,
        &Request::ReadHwm {
            group_id: "volume-0".into(),
        },
    )?
    .lane_hwms
    .ok_or_else(|| invalid("source omitted HWM"))?
    .into_iter()
    .collect();
    if lane_hwms.is_empty() {
        return Err(invalid("source has no durable HWM"));
    }
    call(
        target,
        &Request::Replicate {
            group_id: "volume-0".into(),
            lane_hwms: lane_hwms.iter().map(|(&lane, &hwm)| (lane, hwm)).collect(),
        },
    )?;
    Ok(lane_hwms)
}

fn supervisor_quorum(addresses: &[&str]) -> io::Result<()> {
    let available = addresses
        .iter()
        .filter(|address| probe(address).is_ok())
        .count();
    let required = addresses.len() / 2 + 1;
    if available < required {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            format!("supervisor quorum unavailable: available={available} required={required}"),
        ));
    }
    Ok(())
}

fn apply_ha_with_quorum(
    store: &HaMetadataStore,
    supervisors: &[&str],
    batch: CommittedHaBatch,
) -> io::Result<()> {
    supervisor_quorum(supervisors)?;
    store.apply_committed_batch(&batch)
}

fn report(replica_id: &str, term: u64, config_epoch: u64, sequence: u64) -> ReplicaHwm {
    ReplicaHwm {
        replica_id: replica_id.into(),
        term,
        config_epoch,
        log_id: "linear-log-0".into(),
        lane_hwms: BTreeMap::from([(0, sequence)]),
    }
}

fn apply_dataset(addresses: &[&str], mutations: &[DatasetMutation]) -> io::Result<()> {
    for address in addresses {
        call(
            address,
            &Request::ApplyMutations {
                mutations: mutations.to_vec(),
            },
        )?;
    }
    Ok(())
}

fn capture_dataset_snapshot(
    addresses: &[&str],
    snapshot_id: &str,
    through_sequence: u64,
) -> io::Result<()> {
    for address in addresses {
        call(
            address,
            &Request::CaptureDatasetSnapshot {
                snapshot_id: snapshot_id.into(),
                through_sequence,
            },
        )?;
    }
    Ok(())
}

fn response_payload<T: for<'de> Deserialize<'de>>(response: Response) -> io::Result<T> {
    serde_json::from_value(
        response
            .payload
            .ok_or_else(|| invalid("response omitted payload"))?,
    )
    .map_err(|error| invalid(format!("decode response payload: {error}")))
}

fn run_scenario(log: &Path, addresses: [&str; 5]) -> io::Result<()> {
    let [a_addr, b_addr, cold_c_addr, repl_b_addr, hot_c_addr] = addresses;
    let [a, b, cold_c, repl_b, hot_c] = [
        probe(a_addr)?,
        probe(b_addr)?,
        probe(cold_c_addr)?,
        probe(repl_b_addr)?,
        probe(hot_c_addr)?,
    ];
    let baseline = BTreeMap::from([(0, 9_000), (1, 8_750), (2, 8_500), (3, 8_250)]);
    for address in [a_addr, b_addr] {
        call(
            address,
            &Request::Replicate {
                group_id: "volume-0".into(),
                lane_hwms: baseline.iter().map(|(&lane, &hwm)| (lane, hwm)).collect(),
            },
        )?;
    }
    let supervisors = [cold_c_addr, repl_b_addr, hot_c_addr];
    let ha_log = PathBuf::from(format!("{}.ha", log.display()));
    let expected_final;
    let expected_ha;
    {
        let controller = EvolutionController::open(log, 11)?;
        let ha = HaMetadataStore::open(&ha_log)?;

        println!("EVOLUTION_REQUEST_SUPERVISOR_QUORUM_LOSS phase=isolated");
        io::stdout().flush()?;
        for _ in 0..100 {
            if supervisor_quorum(&supervisors).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let topology_before = controller.state();
        let ha_before = ha.state();
        let rejected_topology = supervisor_quorum(&supervisors)
            .and_then(|_| controller.set_tier("must-not-commit", &a.node_id, "warm", 4));
        let initial_ha_batch = CommittedHaBatch {
            index: 1,
            term: 11,
            transaction_id: "configure-pitr-group".into(),
            commands: vec![
                HaCommand::ConfigureGroup {
                    config: GroupConfig {
                        group_id: "volume-0".into(),
                        volume_id: "volume-0".into(),
                        log_id: "linear-log-0".into(),
                        config_epoch: 1,
                        placement_epoch: 1,
                        voters: vec![
                            "supervisor-cold-c".into(),
                            "supervisor-repl-b".into(),
                            "supervisor-hot-c".into(),
                        ],
                        data_replicas: vec![
                            DataReplica {
                                replica_id: "rep-a".into(),
                                role: DataReplicaRole::Leaf,
                                failure_domain: "region-a".into(),
                            },
                            DataReplica {
                                replica_id: "rep-b".into(),
                                role: DataReplicaRole::Leaf,
                                failure_domain: "region-b".into(),
                            },
                        ],
                        durability: DurabilityPolicy {
                            required_distinct_failure_domains: 2,
                            required_hop_witnesses: 0,
                            required_leaf_witnesses: 2,
                        },
                    },
                },
                HaCommand::GrantLease {
                    group_id: "volume-0".into(),
                    leader_id: "node-a".into(),
                    term: 11,
                    config_epoch: 1,
                    issued_unix_nanos: 1_000,
                    expires_unix_nanos: 10_000,
                    quorum_voters: vec!["supervisor-cold-c".into(), "supervisor-repl-b".into()],
                },
            ],
        };
        let rejected_ha = apply_ha_with_quorum(&ha, &supervisors, initial_ha_batch.clone());
        if rejected_topology.is_ok()
            || rejected_ha.is_ok()
            || controller.state() != topology_before
            || ha.state() != ha_before
            || ha.change_revision() != 0
        {
            return Err(invalid("supervisor quorum loss did not fail atomically"));
        }
        println!(
            "EVOLUTION_PHASE_PASS phase=supervisor-quorum-loss committed=false topology_unchanged=true ha_unchanged=true"
        );
        println!("EVOLUTION_REQUEST_SUPERVISOR_QUORUM_RESTORE phase=isolated");
        io::stdout().flush()?;
        for _ in 0..100 {
            if supervisor_quorum(&supervisors).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        supervisor_quorum(&supervisors)?;
        apply_ha_with_quorum(&ha, &supervisors, initial_ha_batch)?;

        controller.bootstrap(
            "bootstrap-hot-a-cold-b",
            region_obligation("cross-region", "volume-0", 2, 2),
            &[
                (placement("rep-a", &a), baseline.clone()),
                (placement("rep-b", &b), baseline.clone()),
            ],
        )?;
        controller
            .state()
            .verify_coverage("cross-region", &baseline)?;
        println!("EVOLUTION_PHASE_PASS phase=bootstrap policy=steady-state copies=2 regions=2");

        let snapshot_mutations = vec![
            DatasetMutation {
                sequence: 1,
                key: "account.balance".into(),
                value: Some("100".into()),
            },
            DatasetMutation {
                sequence: 2,
                key: "order.state".into(),
                value: Some("open".into()),
            },
            DatasetMutation {
                sequence: 3,
                key: "profile.tier".into(),
                value: Some("gold".into()),
            },
            DatasetMutation {
                sequence: 4,
                key: "snapshot.marker".into(),
                value: Some("s0".into()),
            },
        ];
        apply_dataset(&[a_addr, b_addr], &snapshot_mutations)?;
        capture_dataset_snapshot(&[a_addr, b_addr], "snapshot-s0", 4)?;
        apply_ha_with_quorum(
            &ha,
            &supervisors,
            CommittedHaBatch {
                index: 2,
                term: 11,
                transaction_id: "publish-and-snapshot-s0".into(),
                commands: vec![
                    HaCommand::PublishHwm {
                        group_id: "volume-0".into(),
                        leader_id: "node-a".into(),
                        term: 11,
                        config_epoch: 1,
                        reports: vec![report("rep-a", 11, 1, 4), report("rep-b", 11, 1, 4)],
                    },
                    HaCommand::CaptureSnapshot {
                        snapshot_id: "snapshot-s0".into(),
                        volume_id: "volume-0".into(),
                        created_unix_nanos: 2_000,
                        application_consistent: true,
                    },
                ],
            },
        )?;
        println!(
            "EVOLUTION_PHASE_PASS phase=pitr-snapshot cut=4 replicas=2 application_consistent=true"
        );

        apply_dataset(
            &[a_addr, b_addr],
            &[DatasetMutation {
                sequence: 5,
                key: "account.balance".into(),
                value: Some("80".into()),
            }],
        )?;
        capture_dataset_snapshot(&[a_addr, b_addr], "snapshot-s1", 5)?;
        apply_ha_with_quorum(
            &ha,
            &supervisors,
            CommittedHaBatch {
                index: 3,
                term: 11,
                transaction_id: "publish-and-snapshot-s1".into(),
                commands: vec![
                    HaCommand::PublishHwm {
                        group_id: "volume-0".into(),
                        leader_id: "node-a".into(),
                        term: 11,
                        config_epoch: 1,
                        reports: vec![report("rep-a", 11, 1, 5), report("rep-b", 11, 1, 5)],
                    },
                    HaCommand::CaptureSnapshot {
                        snapshot_id: "snapshot-s1".into(),
                        volume_id: "volume-0".into(),
                        created_unix_nanos: 2_500,
                        application_consistent: true,
                    },
                ],
            },
        )?;
        apply_dataset(
            &[a_addr, b_addr],
            &[DatasetMutation {
                sequence: 6,
                key: "order.state".into(),
                value: Some("paid".into()),
            }],
        )?;
        apply_ha_with_quorum(
            &ha,
            &supervisors,
            CommittedHaBatch {
                index: 4,
                term: 11,
                transaction_id: "publish-recovery-point-p0".into(),
                commands: vec![
                    HaCommand::PublishHwm {
                        group_id: "volume-0".into(),
                        leader_id: "node-a".into(),
                        term: 11,
                        config_epoch: 1,
                        reports: vec![report("rep-a", 11, 1, 6), report("rep-b", 11, 1, 6)],
                    },
                    HaCommand::CaptureRecoveryPoint {
                        recovery_point_id: "recovery-p0".into(),
                        volume_id: "volume-0".into(),
                        created_unix_nanos: 3_000,
                        base_snapshot_id: Some("snapshot-s0".into()),
                        application_consistent: true,
                    },
                ],
            },
        )?;
        apply_dataset(
            &[a_addr, b_addr],
            &[DatasetMutation {
                sequence: 7,
                key: "account.balance".into(),
                value: Some("0".into()),
            }],
        )?;
        apply_ha_with_quorum(
            &ha,
            &supervisors,
            CommittedHaBatch {
                index: 5,
                term: 11,
                transaction_id: "publish-recovery-point-p1".into(),
                commands: vec![
                    HaCommand::PublishHwm {
                        group_id: "volume-0".into(),
                        leader_id: "node-a".into(),
                        term: 11,
                        config_epoch: 1,
                        reports: vec![report("rep-a", 11, 1, 7), report("rep-b", 11, 1, 7)],
                    },
                    HaCommand::CaptureRecoveryPoint {
                        recovery_point_id: "recovery-p1".into(),
                        volume_id: "volume-0".into(),
                        created_unix_nanos: 3_500,
                        base_snapshot_id: Some("snapshot-s1".into()),
                        application_consistent: true,
                    },
                ],
            },
        )?;
        apply_dataset(
            &[a_addr, b_addr],
            &[DatasetMutation {
                sequence: 8,
                key: "order.state".into(),
                value: None,
            }],
        )?;
        if ha.state().retention_floor("volume-0", 0) != Some(4) {
            return Err(invalid("snapshot/recovery point did not pin the WAL floor"));
        }
        let before_pinned_delete = ha.state();
        if ha
            .apply_committed_batch(&CommittedHaBatch {
                index: 6,
                term: 11,
                transaction_id: "pinned-snapshot-delete-must-fail".into(),
                commands: vec![HaCommand::DeleteSnapshot {
                    snapshot_id: "snapshot-s0".into(),
                }],
            })
            .is_ok()
            || ha.state() != before_pinned_delete
            || ha.change_revision() != 5
        {
            return Err(invalid(
                "overlapping recovery pins allowed snapshot deletion",
            ));
        }
        println!(
            "EVOLUTION_PHASE_PASS phase=pitr-overlapping-snapshots intervals=s0:4-6,s1:5-7 shared=5-6 retention_floor=4 pinned_delete_rejected=true post_point_sequence=8"
        );

        println!(
            "EVOLUTION_REQUEST_REGION_FAILURE node={} address={}",
            a.node_id, a_addr
        );
        io::stdout().flush()?;
        let mut failed = false;
        for _ in 0..100 {
            if probe(a_addr).is_err() {
                failed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !failed {
            return Err(invalid("region-a failure was not injected"));
        }
        controller.set_available("observe-region-a-failure", &a.node_id, false)?;
        if controller
            .state()
            .verify_coverage("cross-region", &baseline)
            .is_ok()
        {
            return Err(invalid("unavailable region remained a durability witness"));
        }
        println!(
            "EVOLUTION_PHASE_PASS phase=data-layer-loss policy=reactive-dr durability_quorum=false fail_closed=true"
        );

        println!(
            "EVOLUTION_REQUEST_OVERLAP_FAILURE data_node={} supervisors=2",
            b.node_id
        );
        io::stdout().flush()?;
        for _ in 0..100 {
            if probe(b_addr).is_err() && supervisor_quorum(&supervisors).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let topology_before_overlap = controller.state();
        let ha_before_overlap = ha.state();
        let export_rejected = call(
            b_addr,
            &Request::ExportRecovery {
                snapshot_id: "snapshot-s0".into(),
                target_sequence: 6,
            },
        );
        let topology_rejected = supervisor_quorum(&supervisors)
            .and_then(|_| controller.set_tier("overlap-must-not-commit", &b.node_id, "hot", 9));
        let ha_rejected = apply_ha_with_quorum(
            &ha,
            &supervisors,
            CommittedHaBatch {
                index: 6,
                term: 11,
                transaction_id: "overlap-must-not-commit".into(),
                commands: vec![HaCommand::CaptureRecoveryPoint {
                    recovery_point_id: "must-not-exist".into(),
                    volume_id: "volume-0".into(),
                    created_unix_nanos: 3_500,
                    base_snapshot_id: Some("snapshot-s0".into()),
                    application_consistent: false,
                }],
            },
        );
        if export_rejected.is_ok()
            || topology_rejected.is_ok()
            || ha_rejected.is_ok()
            || controller.state() != topology_before_overlap
            || ha.state() != ha_before_overlap
            || ha.change_revision() != 5
        {
            return Err(invalid(
                "overlapping supervisor/data loss did not fail atomically",
            ));
        }
        println!(
            "EVOLUTION_PHASE_PASS phase=overlap-failure supervisor_quorum=false data_sources=0 recovery_rejected=true commits=0"
        );
        println!("EVOLUTION_REQUEST_OVERLAP_RESTORE");
        io::stdout().flush()?;
        for _ in 0..100 {
            if probe(b_addr).is_ok() && supervisor_quorum(&supervisors).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        probe(b_addr)?;
        supervisor_quorum(&supervisors)?;

        let bundle: RecoveryBundle = response_payload(call(
            b_addr,
            &Request::ExportRecovery {
                snapshot_id: "snapshot-s0".into(),
                target_sequence: 6,
            },
        )?)?;
        let restored: DatasetView = response_payload(call(
            cold_c_addr,
            &Request::RestoreRecovery {
                bundle: bundle.clone(),
            },
        )?)?;
        let reread: DatasetView = response_payload(call(cold_c_addr, &Request::ReadDataset)?)?;
        if restored != reread
            || restored.sequence != 6
            || restored.digest != bundle.expected_digest
            || restored.values != bundle.expected_values
            || restored.values.get("account.balance").map(String::as_str) != Some("80")
            || restored.values.get("order.state").map(String::as_str) != Some("paid")
        {
            return Err(invalid(
                "PITR restored data does not match recovery point p0",
            ));
        }
        println!(
            "EVOLUTION_PHASE_PASS phase=pitr-payload-recovery base_cut=4 replay_through=6 excluded_post_point=7,8 digest={}",
            restored.digest
        );
        let overlapping_bundle: RecoveryBundle = response_payload(call(
            b_addr,
            &Request::ExportRecovery {
                snapshot_id: "snapshot-s1".into(),
                target_sequence: 7,
            },
        )?)?;
        let overlapping_restored: DatasetView = response_payload(call(
            hot_c_addr,
            &Request::RestoreRecovery {
                bundle: overlapping_bundle.clone(),
            },
        )?)?;
        let overlapping_reread: DatasetView =
            response_payload(call(hot_c_addr, &Request::ReadDataset)?)?;
        if overlapping_restored != overlapping_reread
            || overlapping_restored.sequence != 7
            || overlapping_restored.digest != overlapping_bundle.expected_digest
            || overlapping_restored.values != overlapping_bundle.expected_values
            || overlapping_restored
                .values
                .get("account.balance")
                .map(String::as_str)
                != Some("0")
            || overlapping_restored
                .values
                .get("order.state")
                .map(String::as_str)
                != Some("paid")
            || overlapping_restored.digest == restored.digest
        {
            return Err(invalid(
                "overlapping PITR branch did not restore independently",
            ));
        }
        println!(
            "EVOLUTION_PHASE_PASS phase=pitr-overlapping-recovery branches=p0:6,p1:7 targets=node-c-cold,node-c-hot shared_wal=5-6 digests_distinct=true excluded_sequence=8"
        );

        let cold_c_placement = placement("rep-c-cold", &cold_c);
        let handoff = controller.stage_replica(
            "stage-cold-dr-c",
            "rep-b",
            &cold_c_placement,
            baseline.clone(),
        )?;
        let caught_up = copy_hwm(b_addr, cold_c_addr)?;
        controller.activate_copied_replica("activate-cold-dr-c", &handoff, caught_up)?;
        controller
            .state()
            .verify_coverage("cross-region", &baseline)?;
        controller.release_replica(
            "release-failed-a",
            "rep-a",
            "cross-region",
            baseline.clone(),
        )?;
        controller.retire_released_replica("retire-failed-a", "rep-a")?;
        apply_ha_with_quorum(
            &ha,
            &supervisors,
            CommittedHaBatch {
                index: 6,
                term: 12,
                transaction_id: "reconfigure-recovered-replica".into(),
                commands: vec![
                    HaCommand::ConfigureGroup {
                        config: GroupConfig {
                            group_id: "volume-0".into(),
                            volume_id: "volume-0".into(),
                            log_id: "linear-log-0".into(),
                            config_epoch: 2,
                            placement_epoch: 2,
                            voters: vec![
                                "supervisor-cold-c".into(),
                                "supervisor-repl-b".into(),
                                "supervisor-hot-c".into(),
                            ],
                            data_replicas: vec![
                                DataReplica {
                                    replica_id: "rep-b".into(),
                                    role: DataReplicaRole::Leaf,
                                    failure_domain: "region-b".into(),
                                },
                                DataReplica {
                                    replica_id: "rep-c-cold".into(),
                                    role: DataReplicaRole::Leaf,
                                    failure_domain: "region-c".into(),
                                },
                            ],
                            durability: DurabilityPolicy {
                                required_distinct_failure_domains: 2,
                                required_hop_witnesses: 0,
                                required_leaf_witnesses: 2,
                            },
                        },
                    },
                    HaCommand::GrantLease {
                        group_id: "volume-0".into(),
                        leader_id: "node-b".into(),
                        term: 12,
                        config_epoch: 2,
                        issued_unix_nanos: 4_000,
                        expires_unix_nanos: 14_000,
                        quorum_voters: vec!["supervisor-cold-c".into(), "supervisor-repl-b".into()],
                    },
                ],
            },
        )?;
        apply_ha_with_quorum(
            &ha,
            &supervisors,
            CommittedHaBatch {
                index: 7,
                term: 12,
                transaction_id: "certify-recovered-replica".into(),
                commands: vec![HaCommand::PublishHwm {
                    group_id: "volume-0".into(),
                    leader_id: "node-b".into(),
                    term: 12,
                    config_epoch: 2,
                    reports: vec![report("rep-b", 12, 2, 6), report("rep-c-cold", 12, 2, 6)],
                }],
            },
        )?;
        println!(
            "EVOLUTION_PHASE_PASS phase=pitr-custody-activation sequence=6 data_domains=2 metadata_term=12 config_epoch=2"
        );
        println!("EVOLUTION_PHASE_PASS phase=grow-cold-dr policy=reactive-dr regions=2");

        call(
            b_addr,
            &Request::SetTier {
                tier: "hot".into(),
                cost_class: 9,
            },
        )?;
        controller.set_tier("promote-region-b-hot", &b.node_id, "hot", 9)?;
        let hot_c_placement = placement("rep-c-hot", &hot_c);
        let handoff = controller.stage_replica(
            "stage-hot-c",
            "rep-c-cold",
            &hot_c_placement,
            baseline.clone(),
        )?;
        let caught_up = copy_hwm(cold_c_addr, hot_c_addr)?;
        controller.activate_copied_replica("activate-hot-c", &handoff, caught_up)?;
        controller.release_replica(
            "release-cold-c",
            "rep-c-cold",
            "cross-region",
            baseline.clone(),
        )?;
        controller.retire_released_replica("retire-cold-c", "rep-c-cold")?;
        println!("EVOLUTION_PHASE_PASS phase=promote-hot policy=latency-promoter hot_regions=2");

        let repl_b_placement = placement("rep-b-warm", &repl_b);
        let handoff = controller.stage_replica(
            "stage-replacement-b",
            "rep-b",
            &repl_b_placement,
            baseline.clone(),
        )?;
        let caught_up = copy_hwm(b_addr, repl_b_addr)?;
        controller.activate_copied_replica("activate-replacement-b", &handoff, caught_up)?;
        controller.release_replica(
            "release-expensive-b",
            "rep-b",
            "cross-region",
            baseline.clone(),
        )?;
        controller.retire_released_replica("retire-expensive-b", "rep-b")?;
        println!(
            "EVOLUTION_PHASE_PASS phase=replace-replica policy=repair-controller old_removed=true"
        );

        let cold_c2 = ReplicaPlacement {
            replica_id: "rep-c-cold-2".into(),
            ..placement("unused", &cold_c)
        };
        let handoff =
            controller.stage_replica("restage-cold-c", "rep-c-hot", &cold_c2, baseline.clone())?;
        let caught_up = copy_hwm(hot_c_addr, cold_c_addr)?;
        controller.activate_copied_replica("activate-cold-c-2", &handoff, caught_up)?;
        controller.release_replica(
            "release-expensive-c",
            "rep-c-hot",
            "cross-region",
            baseline.clone(),
        )?;
        controller.retire_released_replica("retire-expensive-c", "rep-c-hot")?;
        controller
            .state()
            .verify_coverage("cross-region", &baseline)?;
        println!(
            "EVOLUTION_PHASE_PASS phase=collapse-expensive policy=cost-reducer copies=2 regions=2"
        );

        let state = controller.state();
        if fact_str(&state, &repl_b.node_id, "tier.class") != Some("warm")
            || fact_u64(&state, &repl_b.node_id, "tier.cost_class") != Some(repl_b.cost_class)
            || state
                .custody
                .values()
                .filter(|lease| lease.state == CustodyState::Active)
                .count()
                != 2
        {
            return Err(invalid("final topology does not match cost-reduced policy"));
        }
        expected_final = state;
        expected_ha = ha.state();
    }
    let replayed = EvolutionController::open(log, 12)?.state();
    if replayed != expected_final {
        return Err(invalid("controller replay mismatch"));
    }
    println!(
        "EVOLUTION_PHASE_PASS phase=controller-restart replay_exact=true epoch={}",
        replayed.topology_epoch
    );
    let replayed_ha = HaMetadataStore::open(&ha_log)?.state();
    if replayed_ha != expected_ha
        || replayed_ha.snapshots["snapshot-s0"].cuts["volume-0"].lane_hwms[&0] != 4
        || replayed_ha.recovery_points["recovery-p0"].cuts["volume-0"].lane_hwms[&0] != 6
        || replayed_ha.snapshots["snapshot-s1"].cuts["volume-0"].lane_hwms[&0] != 5
        || replayed_ha.recovery_points["recovery-p1"].cuts["volume-0"].lane_hwms[&0] != 7
    {
        return Err(invalid("HA PITR metadata replay mismatch"));
    }
    println!(
        "EVOLUTION_PHASE_PASS phase=pitr-metadata-replay snapshot_cuts=4,5 recovery_cuts=6,7 overlap=5-6 exact=true"
    );
    for address in [b_addr, cold_c_addr, repl_b_addr, hot_c_addr] {
        let _ = call(address, &Request::Shutdown);
    }
    println!(
        "ZCTOPOLOGY_EVOLUTION_PASS epochs={} active_copies=2 regions=2 pitr=overlapping-snapshots-plus-wal supervisor_quorum_failures=2 data_failures=2 policies=steady-state,reactive-dr,latency-promoter,repair-controller,cost-reducer",
        replayed.topology_epoch
    );
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node() -> NodeState {
        NodeState {
            node_id: "test".into(),
            region: "r0".into(),
            az: "a0".into(),
            tier: "cold".into(),
            cost_class: 1,
            durable_hwms: BTreeMap::new(),
            dataset_sequence: 0,
            dataset: BTreeMap::new(),
            dataset_wal: Vec::new(),
            snapshots: BTreeMap::new(),
        }
    }

    #[test]
    fn snapshot_plus_bounded_wal_excludes_later_mutations() {
        let mut state = node();
        apply_mutations(
            &mut state.dataset_sequence,
            &mut state.dataset,
            &mut state.dataset_wal,
            &[
                DatasetMutation {
                    sequence: 1,
                    key: "k".into(),
                    value: Some("base".into()),
                },
                DatasetMutation {
                    sequence: 2,
                    key: "x".into(),
                    value: Some("keep".into()),
                },
            ],
        )
        .unwrap();
        state.snapshots.insert(
            "s0".into(),
            DatasetSnapshot {
                snapshot_id: "s0".into(),
                through_sequence: 2,
                values: state.dataset.clone(),
                digest: dataset_digest(&state.dataset).unwrap(),
            },
        );
        apply_mutations(
            &mut state.dataset_sequence,
            &mut state.dataset,
            &mut state.dataset_wal,
            &[
                DatasetMutation {
                    sequence: 3,
                    key: "k".into(),
                    value: Some("target".into()),
                },
                DatasetMutation {
                    sequence: 4,
                    key: "k".into(),
                    value: Some("too-late".into()),
                },
            ],
        )
        .unwrap();
        let bundle = export_recovery(&state, "s0", 3).unwrap();
        let mut target = node();
        restore_recovery(&mut target, &bundle).unwrap();
        assert_eq!(target.dataset_sequence, 3);
        assert_eq!(target.dataset["k"], "target");
    }

    #[test]
    fn corrupt_recovery_bundle_fails_without_changing_target() {
        let mut source = node();
        apply_mutations(
            &mut source.dataset_sequence,
            &mut source.dataset,
            &mut source.dataset_wal,
            &[DatasetMutation {
                sequence: 1,
                key: "k".into(),
                value: Some("v".into()),
            }],
        )
        .unwrap();
        source.snapshots.insert(
            "s0".into(),
            DatasetSnapshot {
                snapshot_id: "s0".into(),
                through_sequence: 1,
                values: source.dataset.clone(),
                digest: dataset_digest(&source.dataset).unwrap(),
            },
        );
        let mut bundle = export_recovery(&source, "s0", 1).unwrap();
        bundle.expected_digest = "sha256:bad".into();
        let mut target = node();
        let before = target.clone();
        assert!(restore_recovery(&mut target, &bundle).is_err());
        assert_eq!(target.dataset_sequence, before.dataset_sequence);
        assert_eq!(target.dataset, before.dataset);
        assert_eq!(target.dataset_wal, before.dataset_wal);
    }
}
