use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use zcutils::global_policy::{
    ClusterLink, GlobalDemandSnapshot, GlobalPolicyCommand, GlobalPolicyState, GlobalRatePolicy,
    GlobalRegionSpec, KeyEscrowMode, RegionalInboundPolicy, RegionalTrustGrant,
    RegionalTrustPermissions,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(80);
const AUTHORITY_LEASE: Duration = Duration::from_millis(400);
const RPC_TIMEOUT: Duration = Duration::from_millis(180);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RPC_FRAME_BYTES: usize = 1024 * 1024;
const MAX_ACTIVE_CONNECTIONS: usize = 64;
const MAX_CONNECTIONS_PER_SOURCE: usize = 8;
const MAX_RATE_LIMIT_SOURCES: usize = 1_024;

#[derive(Clone, Debug)]
struct Peer {
    id: String,
    address: SocketAddr,
    leader_eligible: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogEntry {
    index: u64,
    term: u64,
    /// Per-entry salt withheld from blind witnesses, preventing dictionary
    /// attacks against low-entropy policy commands.
    commitment_nonce: String,
    command: GlobalPolicyCommand,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WitnessEntry {
    index: u64,
    term: u64,
    digest: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedNodeState {
    #[serde(default)]
    federation_id: String,
    current_term: u64,
    voted_for: Option<String>,
    committed: GlobalPolicyState,
    pending: Option<LogEntry>,
    #[serde(default)]
    witness_pending: Option<WitnessEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    Follower,
    Candidate,
    Leader,
}

#[derive(Debug)]
struct VolatileState {
    persisted: PersistedNodeState,
    role: Role,
    leader_id: Option<String>,
    last_leader_contact: Instant,
    authority_until: Instant,
}

struct Node {
    federation_id: String,
    id: String,
    region_id: String,
    peers: Vec<Peer>,
    state_path: PathBuf,
    state: Mutex<VolatileState>,
    proposal_lock: Mutex<()>,
    stopped: AtomicBool,
    active_connections: AtomicUsize,
    rpc_rates: Mutex<BTreeMap<IpAddr, RpcRateState>>,
    source_connections: Mutex<BTreeMap<IpAddr, usize>>,
    admin_token: Vec<u8>,
}

#[derive(Debug)]
struct RpcRateState {
    last_refill: Instant,
    consensus_tokens: f64,
    client_tokens: f64,
    connection_tokens: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "rpc", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    RequestVote {
        term: u64,
        candidate_id: String,
        last_index: u64,
        last_term: u64,
    },
    Heartbeat {
        term: u64,
        leader_id: String,
        /// Present only when a follower reports it is behind.
        committed: Option<GlobalPolicyState>,
        /// Redacted catch-up record for a voting-only witness.
        witness_checkpoint: Option<WitnessEntry>,
    },
    Append {
        term: u64,
        leader_id: String,
        entry: LogEntry,
    },
    AppendWitness {
        term: u64,
        leader_id: String,
        entry: WitnessEntry,
    },
    Commit {
        term: u64,
        leader_id: String,
        index: u64,
    },
    Propose {
        command: GlobalPolicyCommand,
    },
    Status,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestEnvelope {
    federation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    admin_token: Option<String>,
    request: Request,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Response {
    ok: bool,
    term: u64,
    commit_index: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    leader_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    vote_granted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<NodeStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NodeStatus {
    federation_id: String,
    node_id: String,
    region_id: String,
    role: Role,
    leader_eligible: bool,
    blind_witness: bool,
    term: u64,
    leader_id: Option<String>,
    commit_index: u64,
    policy_revision: u64,
    authority_valid: bool,
    effective_iops: u64,
    protected_iops: u64,
    cluster_links: Vec<ClusterLink>,
    region_trust_grants: Vec<RegionalTrustGrant>,
    regional_inbound_policies: Vec<RegionalInboundPolicy>,
}

impl Node {
    fn open(
        federation_id: String,
        id: String,
        region_id: String,
        peers: Vec<Peer>,
        state_path: PathBuf,
        admin_token: Vec<u8>,
    ) -> io::Result<Arc<Self>> {
        if !valid_federation_id(&federation_id)
            || id.is_empty()
            || region_id.is_empty()
            || peers.len() < 3
        {
            return Err(invalid(
                "global Raft requires a federation id, node id, region, and at least three voters",
            ));
        }
        if admin_token.len() < 32 {
            return Err(invalid(
                "global Raft management token must be at least 32 bytes",
            ));
        }
        if peers.iter().filter(|peer| peer.leader_eligible).count() < 2 {
            return Err(invalid(
                "global Raft requires at least two leader-eligible full replicas",
            ));
        }
        if !peers.iter().any(|peer| peer.id == id) {
            return Err(invalid("global Raft peer set does not include this node"));
        }
        let mut persisted: PersistedNodeState = if state_path.exists() {
            let file = File::open(&state_path)?;
            if file.metadata()?.len() > MAX_RPC_FRAME_BYTES as u64 {
                return Err(invalid("global Raft state exceeds structural size limit"));
            }
            serde_json::from_reader(BufReader::new(file))
                .map_err(|error| invalid(format!("invalid global Raft state: {error}")))?
        } else {
            PersistedNodeState::default()
        };
        if persisted.federation_id.is_empty() {
            persisted.federation_id = federation_id.clone();
        } else if persisted.federation_id != federation_id {
            return Err(invalid(
                "global Raft state belongs to a different federation",
            ));
        }
        let now = Instant::now();
        Ok(Arc::new(Self {
            federation_id,
            id,
            region_id,
            peers,
            state_path,
            state: Mutex::new(VolatileState {
                persisted,
                role: Role::Follower,
                leader_id: None,
                last_leader_contact: now,
                authority_until: now,
            }),
            proposal_lock: Mutex::new(()),
            stopped: AtomicBool::new(false),
            active_connections: AtomicUsize::new(0),
            rpc_rates: Mutex::new(BTreeMap::new()),
            source_connections: Mutex::new(BTreeMap::new()),
            admin_token,
        }))
    }

    fn management_authorized(&self, envelope: &RequestEnvelope) -> bool {
        if !matches!(envelope.request, Request::Propose { .. } | Request::Status) {
            return true;
        }
        envelope
            .admin_token
            .as_deref()
            .is_some_and(|provided| constant_time_eq(provided.as_bytes(), &self.admin_token))
    }

    fn majority(&self) -> usize {
        self.peers.len() / 2 + 1
    }

    fn other_peers(&self) -> impl Iterator<Item = &Peer> {
        self.peers.iter().filter(|peer| peer.id != self.id)
    }

    fn leader_eligible(&self, node_id: &str) -> bool {
        self.peers
            .iter()
            .any(|peer| peer.id == node_id && peer.leader_eligible)
    }

    fn rpc_allowed(&self, source: IpAddr, request: &Request) -> bool {
        let consensus_identity = match request {
            Request::RequestVote { candidate_id, .. } => Some(candidate_id.as_str()),
            Request::Heartbeat { leader_id, .. }
            | Request::Append { leader_id, .. }
            | Request::AppendWitness { leader_id, .. }
            | Request::Commit { leader_id, .. } => Some(leader_id.as_str()),
            Request::Propose { .. } | Request::Status => None,
        };
        if let Some(peer_id) = consensus_identity
            && !self
                .peers
                .iter()
                .any(|peer| peer.id == peer_id && peer.address.ip() == source)
        {
            return false;
        }

        let mut rates = self.rpc_rates.lock().expect("global Raft RPC rates lock");
        if !rates.contains_key(&source) && rates.len() >= MAX_RATE_LIMIT_SOURCES {
            return false;
        }
        let rate = rates.entry(source).or_insert_with(|| RpcRateState {
            last_refill: Instant::now(),
            consensus_tokens: 256.0,
            client_tokens: 40.0,
            connection_tokens: 128.0,
        });
        let now = Instant::now();
        let elapsed = now.duration_since(rate.last_refill).as_secs_f64();
        rate.last_refill = now;
        rate.consensus_tokens = (rate.consensus_tokens + elapsed * 128.0).min(256.0);
        rate.client_tokens = (rate.client_tokens + elapsed * 20.0).min(40.0);
        rate.connection_tokens = (rate.connection_tokens + elapsed * 128.0).min(128.0);
        let tokens = if consensus_identity.is_some() {
            &mut rate.consensus_tokens
        } else {
            &mut rate.client_tokens
        };
        if *tokens < 1.0 {
            false
        } else {
            *tokens -= 1.0;
            true
        }
    }

    fn open_connection(&self, source: IpAddr) -> bool {
        {
            let mut rates = self.rpc_rates.lock().expect("global Raft RPC rates lock");
            if !rates.contains_key(&source) && rates.len() >= MAX_RATE_LIMIT_SOURCES {
                return false;
            }
            let rate = rates.entry(source).or_insert_with(|| RpcRateState {
                last_refill: Instant::now(),
                consensus_tokens: 256.0,
                client_tokens: 40.0,
                connection_tokens: 128.0,
            });
            let now = Instant::now();
            let elapsed = now.duration_since(rate.last_refill).as_secs_f64();
            rate.last_refill = now;
            rate.consensus_tokens = (rate.consensus_tokens + elapsed * 128.0).min(256.0);
            rate.client_tokens = (rate.client_tokens + elapsed * 20.0).min(40.0);
            rate.connection_tokens = (rate.connection_tokens + elapsed * 128.0).min(128.0);
            if rate.connection_tokens < 1.0 {
                return false;
            }
            rate.connection_tokens -= 1.0;
        }
        let mut sources = self
            .source_connections
            .lock()
            .expect("global Raft source connection lock");
        let active = sources.entry(source).or_default();
        if *active >= MAX_CONNECTIONS_PER_SOURCE {
            false
        } else {
            *active += 1;
            true
        }
    }

    fn close_connection(&self, source: IpAddr) {
        let mut sources = self
            .source_connections
            .lock()
            .expect("global Raft source connection lock");
        if let Some(active) = sources.get_mut(&source) {
            *active = active.saturating_sub(1);
            if *active == 0 {
                sources.remove(&source);
            }
        }
    }

    fn persist_locked(&self, state: &VolatileState) -> io::Result<()> {
        persist_state(&self.state_path, &state.persisted)
    }

    fn election_timeout(&self) -> Duration {
        let hash = self.id.bytes().fold(0u64, |value, byte| {
            value.wrapping_mul(131).wrapping_add(u64::from(byte))
        });
        // A follower cannot campaign until every previously renewed leader
        // authority lease is necessarily dead. Keep randomized election
        // timeout strictly beyond AUTHORITY_LEASE.
        Duration::from_millis(560 + hash % 170)
    }

    fn run_election_loop(self: Arc<Self>) {
        while !self.stopped.load(Ordering::Relaxed) {
            let (role, elapsed) = {
                let state = self.state.lock().expect("global Raft state lock");
                (state.role, state.last_leader_contact.elapsed())
            };
            if role == Role::Leader {
                self.leader_heartbeat_round();
                thread::sleep(HEARTBEAT_INTERVAL);
            } else if elapsed >= self.election_timeout() {
                self.start_election();
            } else {
                thread::sleep(Duration::from_millis(30));
            }
        }
    }

    fn start_election(&self) {
        if !self.leader_eligible(&self.id) {
            let mut state = self.state.lock().expect("global Raft state lock");
            state.role = Role::Follower;
            state.last_leader_contact = Instant::now();
            return;
        }
        let (term, last_index, last_term) = {
            let mut state = self.state.lock().expect("global Raft state lock");
            state.persisted.current_term = state.persisted.current_term.saturating_add(1);
            state.persisted.voted_for = Some(self.id.clone());
            state.role = Role::Candidate;
            state.leader_id = None;
            state.last_leader_contact = Instant::now();
            if self.persist_locked(&state).is_err() {
                state.role = Role::Follower;
                return;
            }
            (
                state.persisted.current_term,
                state.persisted.committed.applied_index,
                state.persisted.committed.applied_term,
            )
        };
        let request = Request::RequestVote {
            term,
            candidate_id: self.id.clone(),
            last_index,
            last_term,
        };
        let mut votes = 1usize;
        for peer in self.other_peers() {
            if let Ok(response) = rpc(peer.address, &self.federation_id, &request) {
                if response.term > term {
                    self.observe_higher_term(response.term, response.leader_id);
                    return;
                }
                votes += usize::from(response.vote_granted);
            }
        }
        let mut state = self.state.lock().expect("global Raft state lock");
        if state.persisted.current_term == term && state.role == Role::Candidate {
            if votes >= self.majority() {
                state.role = Role::Leader;
                state.leader_id = Some(self.id.clone());
                state.authority_until = Instant::now();
                println!(
                    "GLOBAL_RAFT_LEADER node={} region={} term={} votes={}",
                    self.id, self.region_id, term, votes
                );
            } else {
                state.role = Role::Follower;
            }
        }
    }

    fn leader_heartbeat_round(&self) {
        let round_started = Instant::now();
        let (term, committed) = {
            let state = self.state.lock().expect("global Raft state lock");
            if state.role != Role::Leader {
                return;
            }
            (
                state.persisted.current_term,
                state.persisted.committed.clone(),
            )
        };
        let request = Request::Heartbeat {
            term,
            leader_id: self.id.clone(),
            committed: None,
            witness_checkpoint: None,
        };
        let mut acknowledgements = 1usize;
        for peer in self.other_peers() {
            if let Ok(response) = rpc(peer.address, &self.federation_id, &request) {
                if response.term > term {
                    self.observe_higher_term(response.term, response.leader_id);
                    return;
                }
                acknowledgements += usize::from(response.ok);
                if response.ok && response.commit_index < committed.applied_index {
                    let _ = rpc(
                        peer.address,
                        &self.federation_id,
                        &Request::Heartbeat {
                            term,
                            leader_id: self.id.clone(),
                            committed: peer.leader_eligible.then(|| committed.clone()),
                            witness_checkpoint: (!peer.leader_eligible)
                                .then(|| witness_checkpoint(&committed)),
                        },
                    );
                }
            }
        }
        let mut state = self.state.lock().expect("global Raft state lock");
        if state.role == Role::Leader && state.persisted.current_term == term {
            if acknowledgements >= self.majority() {
                // Anchor the lease at round start so slow/dead later peers can
                // never extend it past a follower's election horizon.
                state.authority_until = round_started + AUTHORITY_LEASE;
                state.last_leader_contact = Instant::now();
            } else if Instant::now() >= state.authority_until {
                state.role = Role::Follower;
                state.leader_id = None;
            }
        }
    }

    fn observe_higher_term(&self, term: u64, leader_id: Option<String>) {
        let mut state = self.state.lock().expect("global Raft state lock");
        if term > state.persisted.current_term {
            state.persisted.current_term = term;
            state.persisted.voted_for = None;
            state.role = Role::Follower;
            state.leader_id = leader_id;
            state.authority_until = Instant::now();
            state.last_leader_contact = Instant::now();
            let _ = self.persist_locked(&state);
        }
    }

    fn handle(&self, request: Request) -> Response {
        match request {
            Request::RequestVote {
                term,
                candidate_id,
                last_index,
                last_term,
            } => self.handle_vote(term, candidate_id, last_index, last_term),
            Request::Heartbeat {
                term,
                leader_id,
                committed,
                witness_checkpoint,
            } => self.handle_heartbeat(term, leader_id, committed, witness_checkpoint),
            Request::Append {
                term,
                leader_id,
                entry,
            } => self.handle_append(term, leader_id, entry),
            Request::AppendWitness {
                term,
                leader_id,
                entry,
            } => self.handle_append_witness(term, leader_id, entry),
            Request::Commit {
                term,
                leader_id,
                index,
            } => self.handle_commit(term, leader_id, index),
            Request::Propose { command } => self.propose(command),
            Request::Status => self.status_response(),
        }
    }

    fn base_response(state: &VolatileState) -> Response {
        Response {
            ok: true,
            term: state.persisted.current_term,
            commit_index: state.persisted.committed.applied_index,
            leader_id: state.leader_id.clone(),
            vote_granted: false,
            error: None,
            status: None,
        }
    }

    fn handle_vote(
        &self,
        term: u64,
        candidate_id: String,
        last_index: u64,
        last_term: u64,
    ) -> Response {
        let mut state = self.state.lock().expect("global Raft state lock");
        if !self.leader_eligible(&candidate_id) {
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some("candidate_not_leader_eligible".into());
            return response;
        }
        let prior_term = state.persisted.current_term;
        if term > state.persisted.current_term {
            state.persisted.current_term = term;
            state.persisted.voted_for = None;
            state.role = Role::Follower;
            state.leader_id = None;
            state.authority_until = Instant::now();
        }
        let local_log = (
            state.persisted.committed.applied_term,
            state.persisted.committed.applied_index,
        );
        let candidate_log = (last_term, last_index);
        let can_vote = term == state.persisted.current_term
            && candidate_log >= local_log
            && state
                .persisted
                .voted_for
                .as_ref()
                .is_none_or(|voted| voted == &candidate_id);
        if can_vote {
            state.persisted.voted_for = Some(candidate_id);
            state.last_leader_contact = Instant::now();
        }
        let persistence_required = can_vote || state.persisted.current_term != prior_term;
        let persisted = !persistence_required || self.persist_locked(&state).is_ok();
        let mut response = Self::base_response(&state);
        response.vote_granted = can_vote && persisted;
        response.ok = response.vote_granted;
        response
    }

    fn accept_leader_locked(
        &self,
        state: &mut VolatileState,
        term: u64,
        leader_id: String,
    ) -> bool {
        if !self.leader_eligible(&leader_id) {
            return false;
        }
        if term < state.persisted.current_term {
            return false;
        }
        if term > state.persisted.current_term {
            state.persisted.current_term = term;
            state.persisted.voted_for = None;
        }
        state.role = Role::Follower;
        state.leader_id = Some(leader_id);
        state.last_leader_contact = Instant::now();
        true
    }

    fn handle_heartbeat(
        &self,
        term: u64,
        leader_id: String,
        committed: Option<GlobalPolicyState>,
        witness_checkpoint: Option<WitnessEntry>,
    ) -> Response {
        let mut state = self.state.lock().expect("global Raft state lock");
        let prior_term = state.persisted.current_term;
        let prior_index = state.persisted.committed.applied_index;
        if !self.accept_leader_locked(&mut state, term, leader_id) {
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some("stale_term".into());
            return response;
        }
        if (!self.leader_eligible(&self.id) && committed.is_some())
            || (self.leader_eligible(&self.id) && witness_checkpoint.is_some())
        {
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some("replication_payload_role_mismatch".into());
            return response;
        }
        if let Some(committed) = committed
            && committed.applied_index > state.persisted.committed.applied_index
        {
            state.persisted.committed = committed;
            state.persisted.pending = None;
        }
        if let Some(checkpoint) = witness_checkpoint
            && checkpoint.index > state.persisted.committed.applied_index
        {
            state.persisted.committed.applied_index = checkpoint.index;
            state.persisted.committed.applied_term = checkpoint.term;
            state.persisted.witness_pending = None;
        }
        state.authority_until = Instant::now() + AUTHORITY_LEASE;
        // Ordinary heartbeats renew only volatile authority. Persist when the
        // durable term or committed snapshot advances, never at heartbeat rate.
        if (term != prior_term || state.persisted.committed.applied_index != prior_index)
            && let Err(error) = self.persist_locked(&state)
        {
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some(error.to_string());
            return response;
        }
        Self::base_response(&state)
    }

    fn handle_append(&self, term: u64, leader_id: String, entry: LogEntry) -> Response {
        let mut state = self.state.lock().expect("global Raft state lock");
        if !self.leader_eligible(&self.id)
            || !self.accept_leader_locked(&mut state, term, leader_id)
            || entry.term != term
            || entry.index != state.persisted.committed.applied_index.saturating_add(1)
        {
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some("append_identity_or_index_mismatch".into());
            return response;
        }
        let mut validation = state.persisted.committed.clone();
        if let Err(error) = validation.apply(entry.index, entry.term, &entry.command) {
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some(error.to_string());
            return response;
        }
        state.persisted.pending = Some(entry);
        if let Err(error) = self.persist_locked(&state) {
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some(error.to_string());
            return response;
        }
        Self::base_response(&state)
    }

    fn handle_append_witness(&self, term: u64, leader_id: String, entry: WitnessEntry) -> Response {
        let mut state = self.state.lock().expect("global Raft state lock");
        if self.leader_eligible(&self.id)
            || !self.accept_leader_locked(&mut state, term, leader_id)
            || entry.term != term
            || entry.index != state.persisted.committed.applied_index.saturating_add(1)
            || entry.digest.len() != 64
            || !entry.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some("witness_append_identity_or_index_mismatch".into());
            return response;
        }
        state.persisted.witness_pending = Some(entry);
        if let Err(error) = self.persist_locked(&state) {
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some(error.to_string());
            return response;
        }
        Self::base_response(&state)
    }

    fn handle_commit(&self, term: u64, leader_id: String, index: u64) -> Response {
        let mut state = self.state.lock().expect("global Raft state lock");
        if !self.accept_leader_locked(&mut state, term, leader_id) {
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some("stale_term".into());
            return response;
        }
        if !self.leader_eligible(&self.id) {
            let Some(entry) = state.persisted.witness_pending.take() else {
                let mut response = Self::base_response(&state);
                response.ok = index == state.persisted.committed.applied_index;
                return response;
            };
            if entry.index != index {
                state.persisted.witness_pending = Some(entry);
                let mut response = Self::base_response(&state);
                response.ok = false;
                response.error = Some("witness_commit_index_mismatch".into());
                return response;
            }
            state.persisted.committed.applied_index = entry.index;
            state.persisted.committed.applied_term = entry.term;
            if let Err(error) = self.persist_locked(&state) {
                let mut response = Self::base_response(&state);
                response.ok = false;
                response.error = Some(error.to_string());
                return response;
            }
            return Self::base_response(&state);
        }
        let Some(entry) = state.persisted.pending.take() else {
            let mut response = Self::base_response(&state);
            response.ok = index == state.persisted.committed.applied_index;
            return response;
        };
        if entry.index != index {
            state.persisted.pending = Some(entry);
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some("commit_index_mismatch".into());
            return response;
        }
        if let Err(error) = state
            .persisted
            .committed
            .apply(entry.index, entry.term, &entry.command)
        {
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some(error.to_string());
            return response;
        }
        if let Err(error) = self.persist_locked(&state) {
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some(error.to_string());
            return response;
        }
        Self::base_response(&state)
    }

    fn propose(&self, command: GlobalPolicyCommand) -> Response {
        let _proposal = self
            .proposal_lock
            .lock()
            .expect("global Raft proposal lock");
        let entry = {
            let mut state = self.state.lock().expect("global Raft state lock");
            if state.role != Role::Leader {
                let mut response = Self::base_response(&state);
                response.ok = false;
                response.error = Some("not_leader".into());
                return response;
            }
            let entry = LogEntry {
                index: state.persisted.committed.applied_index.saturating_add(1),
                term: state.persisted.current_term,
                commitment_nonce: match random_nonce() {
                    Ok(nonce) => nonce,
                    Err(error) => {
                        let mut response = Self::base_response(&state);
                        response.ok = false;
                        response.error = Some(error.to_string());
                        return response;
                    }
                },
                command,
            };
            let mut validation = state.persisted.committed.clone();
            if let Err(error) = validation.apply(entry.index, entry.term, &entry.command) {
                let mut response = Self::base_response(&state);
                response.ok = false;
                response.error = Some(error.to_string());
                return response;
            }
            state.persisted.pending = Some(entry.clone());
            if let Err(error) = self.persist_locked(&state) {
                let mut response = Self::base_response(&state);
                response.ok = false;
                response.error = Some(error.to_string());
                return response;
            }
            entry
        };

        let witness_entry = witness_entry(&entry);
        let mut acknowledgements = 1usize;
        let mut full_replica_acknowledgements = 1usize;
        let mut appended = Vec::new();
        for peer in self.other_peers() {
            let append = if peer.leader_eligible {
                Request::Append {
                    term: entry.term,
                    leader_id: self.id.clone(),
                    entry: entry.clone(),
                }
            } else {
                Request::AppendWitness {
                    term: entry.term,
                    leader_id: self.id.clone(),
                    entry: witness_entry.clone(),
                }
            };
            if let Ok(response) = rpc(peer.address, &self.federation_id, &append) {
                if response.term > entry.term {
                    self.observe_higher_term(response.term, response.leader_id);
                    return self.error_response("higher_term_during_append");
                }
                if response.ok {
                    acknowledgements += 1;
                    full_replica_acknowledgements += usize::from(peer.leader_eligible);
                    appended.push(peer.clone());
                }
            }
        }
        if acknowledgements < self.majority() || full_replica_acknowledgements < 2 {
            let mut state = self.state.lock().expect("global Raft state lock");
            state.persisted.pending = None;
            let _ = self.persist_locked(&state);
            let mut response = Self::base_response(&state);
            response.ok = false;
            response.error = Some(if acknowledgements < self.majority() {
                "quorum_unavailable".into()
            } else {
                "trusted_full_replica_unavailable".into()
            });
            return response;
        }

        {
            let mut state = self.state.lock().expect("global Raft state lock");
            let pending = state
                .persisted
                .pending
                .take()
                .expect("leader pending entry");
            if let Err(error) =
                state
                    .persisted
                    .committed
                    .apply(pending.index, pending.term, &pending.command)
            {
                let mut response = Self::base_response(&state);
                response.ok = false;
                response.error = Some(error.to_string());
                return response;
            }
            if let Err(error) = self.persist_locked(&state) {
                let mut response = Self::base_response(&state);
                response.ok = false;
                response.error = Some(error.to_string());
                return response;
            }
            state.authority_until = Instant::now() + AUTHORITY_LEASE;
        }
        let commit = Request::Commit {
            term: entry.term,
            leader_id: self.id.clone(),
            index: entry.index,
        };
        let mut commit_acknowledgements = 1usize;
        let mut full_commit_acknowledgements = 1usize;
        for peer in appended {
            if let Ok(response) = rpc(peer.address, &self.federation_id, &commit) {
                if response.term > entry.term {
                    self.observe_higher_term(response.term, response.leader_id);
                    return self.error_response("higher_term_during_commit");
                }
                commit_acknowledgements += usize::from(response.ok);
                full_commit_acknowledgements += usize::from(response.ok && peer.leader_eligible);
            }
        }
        if commit_acknowledgements < self.majority() || full_commit_acknowledgements < 2 {
            return self.error_response("commit_quorum_unavailable");
        }
        println!(
            "GLOBAL_RAFT_COMMIT node={} term={} index={} acknowledgements={} operation={}",
            self.id,
            entry.term,
            entry.index,
            commit_acknowledgements,
            command_name(&entry.command)
        );
        self.status_response()
    }

    fn error_response(&self, error: &str) -> Response {
        let state = self.state.lock().expect("global Raft state lock");
        let mut response = Self::base_response(&state);
        response.ok = false;
        response.error = Some(error.into());
        response
    }

    fn status_response(&self) -> Response {
        let state = self.state.lock().expect("global Raft state lock");
        let authority_valid = Instant::now() < state.authority_until;
        let protected_iops = state.persisted.committed.protected_iops(&self.region_id);
        let effective_iops = if authority_valid {
            state.persisted.committed.authorized_iops(&self.region_id)
        } else {
            protected_iops
        };
        let mut response = Self::base_response(&state);
        response.status = Some(NodeStatus {
            federation_id: self.federation_id.clone(),
            node_id: self.id.clone(),
            region_id: self.region_id.clone(),
            role: state.role,
            leader_eligible: self.leader_eligible(&self.id),
            blind_witness: !self.leader_eligible(&self.id),
            term: state.persisted.current_term,
            leader_id: state.leader_id.clone(),
            commit_index: state.persisted.committed.applied_index,
            policy_revision: state
                .persisted
                .committed
                .rate_policy
                .as_ref()
                .map_or(0, |policy| policy.revision),
            authority_valid,
            effective_iops,
            protected_iops,
            cluster_links: state
                .persisted
                .committed
                .cluster_links
                .values()
                .cloned()
                .collect(),
            region_trust_grants: state
                .persisted
                .committed
                .region_trust_grants
                .values()
                .cloned()
                .collect(),
            regional_inbound_policies: state
                .persisted
                .committed
                .regional_inbound_policies
                .values()
                .cloned()
                .collect(),
        });
        response
    }
}

fn command_name(command: &GlobalPolicyCommand) -> &'static str {
    match command {
        GlobalPolicyCommand::SetRatePolicy { .. } => "set_rate_policy",
        GlobalPolicyCommand::LinkClusters { .. } => "link_clusters",
        GlobalPolicyCommand::UnlinkClusters { .. } => "unlink_clusters",
        GlobalPolicyCommand::SetRegionTrust { .. } => "set_region_trust",
        GlobalPolicyCommand::RevokeRegionTrust { .. } => "revoke_region_trust",
        GlobalPolicyCommand::SetRegionalInboundPolicy { .. } => "set_regional_inbound_policy",
        GlobalPolicyCommand::RevokeRegionalInboundPolicy { .. } => "revoke_regional_inbound_policy",
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random_nonce() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex_digest(&bytes))
}

fn witness_entry(entry: &LogEntry) -> WitnessEntry {
    let mut digest = Sha256::new();
    digest.update(entry.index.to_le_bytes());
    digest.update(entry.term.to_le_bytes());
    digest.update(entry.commitment_nonce.as_bytes());
    digest.update(serde_json::to_vec(&entry.command).expect("global policy command serialization"));
    WitnessEntry {
        index: entry.index,
        term: entry.term,
        digest: hex_digest(&digest.finalize()),
    }
}

fn witness_checkpoint(state: &GlobalPolicyState) -> WitnessEntry {
    let mut digest = Sha256::new();
    digest.update(b"zcglobal-blind-witness-checkpoint-v1");
    digest.update(state.applied_index.to_le_bytes());
    digest.update(state.applied_term.to_le_bytes());
    WitnessEntry {
        index: state.applied_index,
        term: state.applied_term,
        digest: hex_digest(&digest.finalize()),
    }
}

fn persist_state(path: &Path, state: &PersistedNodeState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let encoded = serde_json::to_vec(state).map_err(io::Error::other)?;
    if encoded.len() + 1 > MAX_RPC_FRAME_BYTES {
        return Err(invalid("global Raft state exceeds structural size limit"));
    }
    let mut file = File::create(&temporary)?;
    file.write_all(&encoded)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn serve(node: Arc<Node>, bind: SocketAddr) -> io::Result<()> {
    let listener = TcpListener::bind(bind)?;
    println!(
        "GLOBAL_RAFT_READY federation={} node={} region={} bind={} voters={} majority={}",
        node.federation_id,
        node.id,
        node.region_id,
        bind,
        node.peers.len(),
        node.majority()
    );
    let election_node = node.clone();
    thread::spawn(move || election_node.run_election_loop());
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                let Ok(source) = stream.peer_addr().map(|address| address.ip()) else {
                    continue;
                };
                if !node.open_connection(source) {
                    eprintln!(
                        "GLOBAL_RAFT_RPC_REJECT node={} source={} reason=source_connection_limit",
                        node.id, source
                    );
                    continue;
                }
                if node.active_connections.fetch_add(1, Ordering::AcqRel) >= MAX_ACTIVE_CONNECTIONS
                {
                    node.active_connections.fetch_sub(1, Ordering::AcqRel);
                    node.close_connection(source);
                    eprintln!(
                        "GLOBAL_RAFT_RPC_REJECT node={} reason=connection_limit",
                        node.id
                    );
                    continue;
                }
                let node = node.clone();
                thread::spawn(move || {
                    let _guard = ActiveConnectionGuard {
                        node: node.clone(),
                        source,
                    };
                    if let Err(error) = handle_connection(&node, stream) {
                        eprintln!("GLOBAL_RAFT_RPC_ERROR node={} error={error}", node.id);
                    }
                });
            }
            Err(error) => eprintln!("GLOBAL_RAFT_ACCEPT_ERROR node={} error={error}", node.id),
        }
    }
    Ok(())
}

struct ActiveConnectionGuard {
    node: Arc<Node>,
    source: IpAddr,
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.node.active_connections.fetch_sub(1, Ordering::AcqRel);
        self.node.close_connection(self.source);
    }
}

fn handle_connection(node: &Node, mut stream: TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(RPC_TIMEOUT))?;
    stream.set_write_timeout(Some(RPC_TIMEOUT))?;
    let source = stream.peer_addr()?.ip();
    let reader = BufReader::new(stream.try_clone()?);
    let mut limited = reader.take((MAX_RPC_FRAME_BYTES + 1) as u64);
    let mut line = Vec::new();
    limited.read_until(b'\n', &mut line)?;
    if line.is_empty() || line.len() > MAX_RPC_FRAME_BYTES || line.last() != Some(&b'\n') {
        return Err(invalid(
            "global Raft RPC frame missing, oversized, or unterminated",
        ));
    }
    let envelope: RequestEnvelope = serde_json::from_slice(&line)
        .map_err(|error| invalid(format!("invalid global Raft RPC: {error}")))?;
    if envelope.federation_id != node.federation_id {
        return write_response(&mut stream, &node.error_response("federation_mismatch"));
    }
    if !node.management_authorized(&envelope) {
        return write_response(
            &mut stream,
            &node.error_response("management_authentication_failed"),
        );
    }
    if !node.rpc_allowed(source, &envelope.request) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "global Raft RPC peer identity or rate rejected",
        ));
    }
    let response = node.handle(envelope.request);
    write_response(&mut stream, &response)
}

fn write_response(stream: &mut TcpStream, response: &Response) -> io::Result<()> {
    let encoded = serde_json::to_vec(&response).map_err(io::Error::other)?;
    if encoded.len() + 1 > MAX_RPC_FRAME_BYTES {
        return Err(invalid("global Raft RPC response exceeds frame limit"));
    }
    stream.write_all(&encoded)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn rpc(address: SocketAddr, federation_id: &str, request: &Request) -> io::Result<Response> {
    rpc_with_timeout(address, federation_id, None, request, RPC_TIMEOUT)
}

fn rpc_with_timeout(
    address: SocketAddr,
    federation_id: &str,
    admin_token: Option<String>,
    request: &Request,
    timeout: Duration,
) -> io::Result<Response> {
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let encoded = serde_json::to_vec(&RequestEnvelope {
        federation_id: federation_id.to_owned(),
        admin_token,
        request: request.clone(),
    })
    .map_err(io::Error::other)?;
    if encoded.len() + 1 > MAX_RPC_FRAME_BYTES {
        return Err(invalid("global Raft RPC request exceeds frame limit"));
    }
    stream.write_all(&encoded)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let reader = BufReader::new(stream);
    let mut limited = reader.take((MAX_RPC_FRAME_BYTES + 1) as u64);
    let mut line = Vec::new();
    limited.read_until(b'\n', &mut line)?;
    if line.is_empty() || line.len() > MAX_RPC_FRAME_BYTES || line.last() != Some(&b'\n') {
        return Err(invalid(
            "global Raft RPC response missing, oversized, or unterminated",
        ));
    }
    serde_json::from_slice(&line).map_err(io::Error::other)
}

fn parse_peers(value: &str) -> io::Result<Vec<Peer>> {
    let mut peers = Vec::new();
    for token in value.split(',') {
        let (identity, eligibility) = token.rsplit_once('#').unwrap_or((token, "leader"));
        let leader_eligible = match eligibility {
            "leader" => true,
            "voter" => false,
            _ => return Err(invalid("peer eligibility must be #leader or #voter")),
        };
        let (id, address) = identity
            .split_once('@')
            .ok_or_else(|| invalid("peer must use id@address"))?;
        if id.is_empty() || peers.iter().any(|peer: &Peer| peer.id == id) {
            return Err(invalid("empty or duplicate peer id"));
        }
        peers.push(Peer {
            id: id.into(),
            address: address
                .parse()
                .map_err(|_| invalid("invalid peer socket address"))?,
            leader_eligible,
        });
    }
    peers.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(peers)
}

fn parse_regions(value: &str) -> io::Result<Vec<GlobalRegionSpec>> {
    value
        .split(',')
        .map(|token| {
            let fields = token.split(':').collect::<Vec<_>>();
            if fields.len() != 4 {
                return Err(invalid("region must use id:guarantee:ceiling:weight"));
            }
            Ok(GlobalRegionSpec {
                region_id: fields[0].into(),
                guaranteed_iops: parse_u64(fields[1], "regional guarantee")?,
                ceiling_iops: parse_u64(fields[2], "regional ceiling")?,
                borrow_weight: fields[3]
                    .parse()
                    .map_err(|_| invalid("invalid regional weight"))?,
            })
        })
        .collect()
}

fn parse_demand(value: &str) -> io::Result<std::collections::BTreeMap<String, u64>> {
    value
        .split(',')
        .map(|token| {
            let (region, demand) = token
                .split_once(':')
                .ok_or_else(|| invalid("demand must use region:iops"))?;
            Ok((region.into(), parse_u64(demand, "regional demand")?))
        })
        .collect()
}

fn parse_u64(value: &str, name: &str) -> io::Result<u64> {
    value
        .parse()
        .map_err(|_| invalid(format!("invalid {name}")))
}

fn parse_bool(value: &str, name: &str) -> io::Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(invalid(format!("invalid {name}; expected true or false"))),
    }
}

fn parse_escrow_mode(value: &str) -> io::Result<KeyEscrowMode> {
    match value {
        "denied" => Ok(KeyEscrowMode::Denied),
        "on-demand" => Ok(KeyEscrowMode::OnDemand),
        "automatic-on-loss" => Ok(KeyEscrowMode::AutomaticOnLoss),
        _ => Err(invalid(
            "invalid escrow mode; expected denied, on-demand, or automatic-on-loss",
        )),
    }
}

fn parse_release_regions(value: &str) -> std::collections::BTreeSet<String> {
    if value == "-" {
        Default::default()
    } else {
        value.split(',').map(str::to_owned).collect()
    }
}

fn parse_string_set(value: &str) -> std::collections::BTreeSet<String> {
    parse_release_regions(value)
}

fn parse_attributes(value: &str) -> io::Result<std::collections::BTreeMap<String, String>> {
    if value == "-" {
        return Ok(Default::default());
    }
    value
        .split(',')
        .map(|item| {
            let (key, value) = item
                .split_once('=')
                .ok_or_else(|| invalid("attribute must use key=value"))?;
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn send_cli(address: &str, request: Request) -> io::Result<()> {
    let federation_id = env::var("ZCGLOBAL_FEDERATION_ID")
        .map_err(|_| invalid("ZCGLOBAL_FEDERATION_ID must identify the target federation"))?;
    let token_path = env::var("ZCGLOBAL_ADMIN_TOKEN_FILE")
        .map_err(|_| invalid("ZCGLOBAL_ADMIN_TOKEN_FILE must name the management token file"))?;
    let admin_token = String::from_utf8(read_secret_file(Path::new(&token_path))?)
        .map_err(|_| invalid("management token must be UTF-8"))?;
    let response = rpc_with_timeout(
        address
            .parse()
            .map_err(|_| invalid("invalid server address"))?,
        &federation_id,
        Some(admin_token),
        &request,
        CLIENT_TIMEOUT,
    )?;
    println!("{}", serde_json::to_string(&response).unwrap());
    if response.ok {
        Ok(())
    } else {
        Err(io::Error::other(
            response.error.unwrap_or_else(|| "request rejected".into()),
        ))
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn valid_federation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let maximum = left.len().max(right.len());
    for index in 0..maximum {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn read_secret_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut secret = fs::read(path)?;
    while matches!(secret.last(), Some(b'\n' | b'\r')) {
        secret.pop();
    }
    Ok(secret)
}

fn usage() -> io::Error {
    invalid(
        "usage:\n  zcglobal-policy-node serve FEDERATION ID REGION BIND STATE ID@ADDR#leader,ID@ADDR#leader,ID@ADDR#voter ADMIN_TOKEN_FILE\n  \
         zcglobal-policy-node status ADDR\n  zcglobal-policy-node set-rate ADDR REV GLOBAL_GUARANTEE \
         GLOBAL_CEILING REGION:GUARANTEE:CEILING:WEIGHT,... REGION:DEMAND,...\n  \
         zcglobal-policy-node link-clusters ADDR LINK_ID SOURCE_CLUSTER TARGET_CLUSTER SOURCE_REGION \
         TARGET_REGION GENERATION RESERVED_IOPS CEILING_IOPS TRUST_GRANT_ID TRUST_GENERATION\n  \
         zcglobal-policy-node unlink-clusters ADDR LINK_ID GENERATION\n  \
         zcglobal-policy-node grant-region ADDR GRANT_ID OWNER_REGION DELEGATE_REGION GENERATION \
         STORE_ENCRYPTED STORE_UNENCRYPTED RESTORE_SERVE ESCROW_MODE RELEASE_REGION,...|-\n  \
         zcglobal-policy-node revoke-region ADDR GRANT_ID GENERATION\n  \
         zcglobal-policy-node set-inbound-policy ADDR REGION GENERATION SOURCE,... \
         ACCEPT_ENCRYPTED ACCEPT_UNENCRYPTED ESCROW_MODE MAX_BYTES CLASS,...|- \
         REQUIRED_KEY=VALUE,...|- DENIED_KEY=VALUE,...|-\n  \
         zcglobal-policy-node revoke-inbound-policy ADDR REGION GENERATION",
    )
}

fn main() -> io::Result<()> {
    let args = env::args().collect::<Vec<_>>();
    match args.get(1).map(String::as_str) {
        Some("serve") if args.len() == 9 => {
            let peers = parse_peers(&args[7])?;
            let node = Node::open(
                args[2].clone(),
                args[3].clone(),
                args[4].clone(),
                peers,
                args[6].clone().into(),
                read_secret_file(Path::new(&args[8]))?,
            )?;
            serve(
                node,
                args[5]
                    .parse()
                    .map_err(|_| invalid("invalid bind address"))?,
            )
        }
        Some("status") if args.len() == 3 => send_cli(&args[2], Request::Status),
        Some("set-rate") if args.len() == 10 => send_cli(
            &args[2],
            Request::Propose {
                command: GlobalPolicyCommand::SetRatePolicy {
                    policy: GlobalRatePolicy {
                        revision: parse_u64(&args[3], "policy revision")?,
                        guaranteed_iops: parse_u64(&args[4], "global guarantee")?,
                        ceiling_iops: parse_u64(&args[5], "global ceiling")?,
                        regions: parse_regions(&args[6])?,
                    },
                    demand: GlobalDemandSnapshot {
                        region_demand_iops: parse_demand(&args[7])?,
                        interval_start_ns: parse_u64(&args[8], "interval start")?,
                        interval_end_ns: parse_u64(&args[9], "interval end")?,
                    },
                },
            },
        ),
        Some("link-clusters") if args.len() == 13 => send_cli(
            &args[2],
            Request::Propose {
                command: GlobalPolicyCommand::LinkClusters {
                    link: ClusterLink {
                        link_id: args[3].clone(),
                        source_cluster_id: args[4].clone(),
                        target_cluster_id: args[5].clone(),
                        source_region_id: args[6].clone(),
                        target_region_id: args[7].clone(),
                        generation: parse_u64(&args[8], "link generation")?,
                        reserved_iops: parse_u64(&args[9], "link reservation")?,
                        ceiling_iops: parse_u64(&args[10], "link ceiling")?,
                        trust_grant_id: args[11].clone(),
                        trust_grant_generation: parse_u64(&args[12], "trust grant generation")?,
                    },
                },
            },
        ),
        Some("unlink-clusters") if args.len() == 5 => send_cli(
            &args[2],
            Request::Propose {
                command: GlobalPolicyCommand::UnlinkClusters {
                    link_id: args[3].clone(),
                    generation: parse_u64(&args[4], "unlink generation")?,
                },
            },
        ),
        Some("grant-region") if args.len() == 12 => send_cli(
            &args[2],
            Request::Propose {
                command: GlobalPolicyCommand::SetRegionTrust {
                    grant: RegionalTrustGrant {
                        grant_id: args[3].clone(),
                        owner_region_id: args[4].clone(),
                        delegate_region_id: args[5].clone(),
                        generation: parse_u64(&args[6], "trust grant generation")?,
                        permissions: RegionalTrustPermissions {
                            store_encrypted_replicas: parse_bool(
                                &args[7],
                                "encrypted replica permission",
                            )?,
                            store_unencrypted_replicas: parse_bool(
                                &args[8],
                                "unencrypted replica permission",
                            )?,
                            serve_encrypted_restore: parse_bool(
                                &args[9],
                                "restore serving permission",
                            )?,
                            key_escrow: parse_escrow_mode(&args[10])?,
                            key_release_regions: parse_release_regions(&args[11]),
                        },
                    },
                },
            },
        ),
        Some("revoke-region") if args.len() == 5 => send_cli(
            &args[2],
            Request::Propose {
                command: GlobalPolicyCommand::RevokeRegionTrust {
                    grant_id: args[3].clone(),
                    generation: parse_u64(&args[4], "trust revocation generation")?,
                },
            },
        ),
        Some("set-inbound-policy") if args.len() == 13 => send_cli(
            &args[2],
            Request::Propose {
                command: GlobalPolicyCommand::SetRegionalInboundPolicy {
                    policy: RegionalInboundPolicy {
                        region_id: args[3].clone(),
                        generation: parse_u64(&args[4], "inbound policy generation")?,
                        allowed_source_regions: parse_string_set(&args[5]),
                        accept_encrypted_volumes: parse_bool(
                            &args[6],
                            "encrypted volume admission",
                        )?,
                        accept_unencrypted_volumes: parse_bool(
                            &args[7],
                            "unencrypted volume admission",
                        )?,
                        accept_key_escrow: parse_escrow_mode(&args[8])?,
                        max_volume_bytes: parse_u64(&args[9], "maximum volume bytes")?,
                        allowed_data_classes: parse_string_set(&args[10]),
                        required_attributes: parse_attributes(&args[11])?,
                        denied_attributes: parse_attributes(&args[12])?,
                    },
                },
            },
        ),
        Some("revoke-inbound-policy") if args.len() == 5 => send_cli(
            &args[2],
            Request::Propose {
                command: GlobalPolicyCommand::RevokeRegionalInboundPolicy {
                    region_id: args[3].clone(),
                    generation: parse_u64(&args[4], "inbound policy revocation generation")?,
                },
            },
        ),
        _ => Err(usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_node(id: &str, label: &str) -> Arc<Node> {
        Node::open(
            "test-federation".into(),
            id.into(),
            "region-test".into(),
            parse_peers(
                "us@127.0.0.11:9910#leader,uk@127.0.0.12:9910#leader,pottsylvania@127.0.0.13:9910#voter",
            )
            .unwrap(),
            std::env::temp_dir().join(format!(
                "zcglobal-policy-eligibility-{}-{id}-{label}.json",
                std::process::id(),
            )),
            b"test-management-token-32-bytes-minimum".to_vec(),
        )
        .unwrap()
    }

    #[test]
    fn voting_only_member_never_campaigns() {
        let node = test_node("pottsylvania", "never-campaigns");
        node.start_election();
        let state = node.state.lock().unwrap();
        assert_eq!(state.role, Role::Follower);
        assert_eq!(state.persisted.current_term, 0);
    }

    #[test]
    fn voting_only_candidate_and_leader_are_rejected_without_term_change() {
        let node = test_node("us", "rejects-ineligible");
        let vote = node.handle_vote(100, "pottsylvania".into(), 0, 0);
        assert!(!vote.ok);
        assert_eq!(vote.error.as_deref(), Some("candidate_not_leader_eligible"));
        let heartbeat = node.handle_heartbeat(100, "pottsylvania".into(), None, None);
        assert!(!heartbeat.ok);
        assert_eq!(node.state.lock().unwrap().persisted.current_term, 0);
    }

    #[test]
    fn blind_witness_persists_only_commitment_metadata() {
        let node = test_node("pottsylvania", "blind-persistence");
        let append = node.handle_append_witness(
            1,
            "us".into(),
            WitnessEntry {
                index: 1,
                term: 1,
                digest: "ab".repeat(32),
            },
        );
        assert!(append.ok);
        assert!(node.handle_commit(1, "us".into(), 1).ok);
        let persisted = fs::read_to_string(&node.state_path).unwrap();
        assert!(!persisted.contains("set_rate_policy"));
        assert!(!persisted.contains("source_cluster_id"));
        assert!(persisted.len() < 1_024);
        let state = node.state.lock().unwrap();
        assert_eq!(state.persisted.committed.applied_index, 1);
        assert!(state.persisted.committed.rate_policy.is_none());
        assert!(state.persisted.committed.cluster_links.is_empty());
        assert!(state.persisted.committed.region_trust_grants.is_empty());
    }

    #[test]
    fn management_requests_require_the_exact_federation_token() {
        let node = test_node("us", "management-auth");
        let authorized = RequestEnvelope {
            federation_id: "test-federation".into(),
            admin_token: Some("test-management-token-32-bytes-minimum".into()),
            request: Request::Status,
        };
        assert!(node.management_authorized(&authorized));

        let mut wrong = authorized.clone();
        wrong.admin_token = Some("other-management-token-32-bytes-minimum".into());
        assert!(!node.management_authorized(&wrong));

        let consensus = RequestEnvelope {
            federation_id: "test-federation".into(),
            admin_token: None,
            request: Request::Heartbeat {
                term: 1,
                leader_id: "uk".into(),
                committed: None,
                witness_checkpoint: None,
            },
        };
        assert!(node.management_authorized(&consensus));
    }

    #[test]
    fn persisted_state_cannot_be_reopened_as_another_federation() {
        let path = std::env::temp_dir().join(format!(
            "zcglobal-policy-federation-binding-{}.json",
            std::process::id()
        ));
        let peers = parse_peers(
            "us@127.0.0.11:9910#leader,uk@127.0.0.12:9910#leader,pottsylvania@127.0.0.13:9910#voter",
        )
        .unwrap();
        let node = Node::open(
            "federation-a".into(),
            "us".into(),
            "us".into(),
            peers.clone(),
            path.clone(),
            b"test-management-token-32-bytes-minimum".to_vec(),
        )
        .unwrap();
        {
            let state = node.state.lock().unwrap();
            node.persist_locked(&state).unwrap();
        }
        drop(node);
        let error = match Node::open(
            "federation-b".into(),
            "us".into(),
            "us".into(),
            peers,
            path.clone(),
            b"test-management-token-32-bytes-minimum".to_vec(),
        ) {
            Ok(_) => panic!("state was reopened under a foreign federation"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("different federation"));
        fs::remove_file(path).unwrap();
    }
}
