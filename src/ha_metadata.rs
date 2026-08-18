//! Consensus-applied HA metadata for userspace WAL groups.
//!
//! This module deliberately contains no payload placement or block-device I/O.
//! A Raft implementation applies already-committed entries here; lane workers
//! retain an `Arc<PublishedGroupView>` and validate read authority without a
//! mutex, allocation, or consensus round trip.

use crate::change_log::{
    CHANGE_BATCH_SCHEMA_VERSION, ChangeBatch, ChangeLogStore, CommittedChangeBatch,
    ComponentChange, content_hash,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, RwLock};

pub const HA_LOG_STREAM: &str = "zccusan.ha.v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupConfig {
    pub group_id: String,
    pub volume_id: String,
    pub log_id: String,
    pub config_epoch: u64,
    pub placement_epoch: u64,
    pub voters: Vec<String>,
    pub data_replicas: Vec<DataReplica>,
    pub durability: DurabilityPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataReplicaRole {
    Hop,
    Leaf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataReplica {
    pub replica_id: String,
    pub role: DataReplicaRole,
    pub failure_domain: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurabilityPolicy {
    pub required_distinct_failure_domains: usize,
    pub required_hop_witnesses: usize,
    pub required_leaf_witnesses: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaHwm {
    pub replica_id: String,
    pub term: u64,
    pub config_epoch: u64,
    pub log_id: String,
    #[serde(with = "lane_hwm_map")]
    pub lane_hwms: BTreeMap<u32, u64>,
}

/// A durability certificate produced directly from configured WAL witnesses.
/// It can satisfy a client barrier without first becoming a Raft log entry.
/// `PublishHwm` checkpoints the same result for bounded failover work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurabilityCertificate {
    pub group_id: String,
    pub term: u64,
    pub config_epoch: u64,
    pub log_id: String,
    pub reporter_ids: Vec<String>,
    #[serde(with = "lane_hwm_map")]
    pub lane_hwms: BTreeMap<u32, u64>,
}

pub fn certify_durable_hwm(
    config: &GroupConfig,
    term: u64,
    reports: &[ReplicaHwm],
) -> io::Result<DurabilityCertificate> {
    let replica_map: BTreeMap<&str, &DataReplica> = config
        .data_replicas
        .iter()
        .map(|replica| (replica.replica_id.as_str(), replica))
        .collect();
    let mut reporters = BTreeSet::new();
    for report in reports {
        if !replica_map.contains_key(report.replica_id.as_str()) {
            return Err(invalid(format!(
                "unknown data HWM reporter {}",
                report.replica_id
            )));
        }
        if !reporters.insert(report.replica_id.as_str()) {
            return Err(invalid(format!(
                "duplicate HWM reporter {}",
                report.replica_id
            )));
        }
        if report.term != term
            || report.config_epoch != config.config_epoch
            || report.log_id != config.log_id
        {
            return Err(invalid(format!(
                "HWM report identity mismatch for {}",
                report.replica_id
            )));
        }
    }
    let lanes: BTreeSet<u32> = reports
        .iter()
        .flat_map(|report| report.lane_hwms.keys().copied())
        .collect();
    if lanes.is_empty() {
        return Err(invalid("HWM certificate contains no lanes"));
    }
    let mut lane_hwms = BTreeMap::new();
    for lane in lanes {
        let mut values: Vec<u64> = reports
            .iter()
            .filter_map(|report| report.lane_hwms.get(&lane).copied())
            .collect();
        values.sort_unstable();
        values.dedup();
        let certified = values.into_iter().rev().find(|candidate| {
            let mut domains = BTreeSet::new();
            let mut hops = BTreeSet::new();
            let mut leaves = BTreeSet::new();
            for report in reports {
                if report
                    .lane_hwms
                    .get(&lane)
                    .is_some_and(|hwm| hwm >= candidate)
                {
                    let replica = replica_map[report.replica_id.as_str()];
                    domains.insert(replica.failure_domain.as_str());
                    if replica.role == DataReplicaRole::Leaf {
                        leaves.insert(replica.replica_id.as_str());
                    } else {
                        hops.insert(replica.replica_id.as_str());
                    }
                }
            }
            domains.len() >= config.durability.required_distinct_failure_domains
                && hops.len() >= config.durability.required_hop_witnesses
                && leaves.len() >= config.durability.required_leaf_witnesses
        });
        lane_hwms.insert(
            lane,
            certified.ok_or_else(|| {
                invalid(format!(
                    "lane {lane} lacks required durable failure-domain coverage"
                ))
            })?,
        );
    }
    Ok(DurabilityCertificate {
        group_id: config.group_id.clone(),
        term,
        config_epoch: config.config_epoch,
        log_id: config.log_id.clone(),
        reporter_ids: reporters.into_iter().map(str::to_string).collect(),
        lane_hwms,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCut {
    pub group_id: String,
    pub term: u64,
    pub config_epoch: u64,
    pub log_id: String,
    #[serde(with = "lane_hwm_map")]
    pub lane_hwms: BTreeMap<u32, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub snapshot_id: String,
    pub volume_id: String,
    pub created_unix_nanos: u64,
    pub application_consistent: bool,
    pub cuts: BTreeMap<String, GroupCut>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryPoint {
    pub recovery_point_id: String,
    pub volume_id: String,
    pub created_unix_nanos: u64,
    pub base_snapshot_id: Option<String>,
    pub application_consistent: bool,
    pub cuts: BTreeMap<String, GroupCut>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum HaCommand {
    ConfigureGroup {
        config: GroupConfig,
    },
    GrantLease {
        group_id: String,
        leader_id: String,
        term: u64,
        config_epoch: u64,
        issued_unix_nanos: u64,
        expires_unix_nanos: u64,
        quorum_voters: Vec<String>,
    },
    PublishHwm {
        group_id: String,
        leader_id: String,
        term: u64,
        config_epoch: u64,
        reports: Vec<ReplicaHwm>,
    },
    CaptureSnapshot {
        snapshot_id: String,
        volume_id: String,
        created_unix_nanos: u64,
        application_consistent: bool,
    },
    DeleteSnapshot {
        snapshot_id: String,
    },
    CaptureRecoveryPoint {
        recovery_point_id: String,
        volume_id: String,
        created_unix_nanos: u64,
        base_snapshot_id: Option<String>,
        application_consistent: bool,
    },
    DeleteRecoveryPoint {
        recovery_point_id: String,
    },
}

impl HaCommand {
    fn key(&self) -> &str {
        match self {
            Self::ConfigureGroup { config } => &config.group_id,
            Self::GrantLease { group_id, .. } | Self::PublishHwm { group_id, .. } => group_id,
            Self::CaptureSnapshot { snapshot_id, .. } | Self::DeleteSnapshot { snapshot_id } => {
                snapshot_id
            }
            Self::CaptureRecoveryPoint {
                recovery_point_id, ..
            }
            | Self::DeleteRecoveryPoint { recovery_point_id } => recovery_point_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedHaEntry {
    pub index: u64,
    pub term: u64,
    pub command: HaCommand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedHaBatch {
    pub index: u64,
    pub term: u64,
    pub transaction_id: String,
    pub commands: Vec<HaCommand>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupState {
    pub config: Option<GroupConfig>,
    pub term: u64,
    pub leader_id: String,
    pub lease_issued_unix_nanos: u64,
    pub lease_expires_unix_nanos: u64,
    #[serde(with = "lane_hwm_map")]
    pub certified_lane_hwms: BTreeMap<u32, u64>,
}

mod lane_hwm_map {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S>(value: &BTreeMap<u32, u64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value
            .iter()
            .map(|(&lane, &hwm)| (lane, hwm))
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BTreeMap<u32, u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Vec::<(u32, u64)>::deserialize(deserializer)?
            .into_iter()
            .collect())
    }
}

impl GroupState {
    pub fn cut(&self, group_id: &str) -> io::Result<GroupCut> {
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| invalid(format!("HA group {group_id} is not configured")))?;
        Ok(GroupCut {
            group_id: group_id.to_string(),
            term: self.term,
            config_epoch: config.config_epoch,
            log_id: config.log_id.clone(),
            lane_hwms: self.certified_lane_hwms.clone(),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HaState {
    pub applied_index: u64,
    pub applied_term: u64,
    pub groups: BTreeMap<String, GroupState>,
    pub snapshots: BTreeMap<String, SnapshotRecord>,
    pub recovery_points: BTreeMap<String, RecoveryPoint>,
}

impl HaState {
    pub fn apply(&mut self, entry: &CommittedHaEntry) -> io::Result<()> {
        self.apply_batch(
            entry.index,
            entry.term,
            std::slice::from_ref(&entry.command),
        )
    }

    /// Validate and install a committed Raft entry as one all-or-nothing
    /// state transition. Every command sees earlier commands in this batch.
    pub fn apply_batch(&mut self, index: u64, term: u64, commands: &[HaCommand]) -> io::Result<()> {
        if commands.is_empty() {
            return Err(invalid("HA command batch must not be empty"));
        }
        if index != self.applied_index.saturating_add(1) {
            return Err(invalid(format!(
                "HA committed index gap expected={} got={}",
                self.applied_index.saturating_add(1),
                index
            )));
        }
        if term < self.applied_term {
            return Err(invalid(format!(
                "HA term regressed from {} to {}",
                self.applied_term, term
            )));
        }
        let mut staged = self.clone();
        for command in commands {
            staged.apply_command(term, command)?;
        }
        staged.applied_index = index;
        staged.applied_term = term;
        *self = staged;
        Ok(())
    }

    fn apply_command(&mut self, raft_term: u64, command: &HaCommand) -> io::Result<()> {
        match command {
            HaCommand::ConfigureGroup { config } => self.configure(config),
            HaCommand::GrantLease {
                group_id,
                leader_id,
                term,
                config_epoch,
                issued_unix_nanos,
                expires_unix_nanos,
                quorum_voters,
            } => self.grant_lease(
                raft_term,
                group_id,
                leader_id,
                *term,
                *config_epoch,
                *issued_unix_nanos,
                *expires_unix_nanos,
                quorum_voters,
            ),
            HaCommand::PublishHwm {
                group_id,
                leader_id,
                term,
                config_epoch,
                reports,
            } => self.publish_hwm(
                raft_term,
                group_id,
                leader_id,
                *term,
                *config_epoch,
                reports,
            ),
            HaCommand::CaptureSnapshot {
                snapshot_id,
                volume_id,
                created_unix_nanos,
                application_consistent,
            } => self.capture_snapshot(
                snapshot_id,
                volume_id,
                *created_unix_nanos,
                *application_consistent,
            ),
            HaCommand::DeleteSnapshot { snapshot_id } => self.delete_snapshot(snapshot_id),
            HaCommand::CaptureRecoveryPoint {
                recovery_point_id,
                volume_id,
                created_unix_nanos,
                base_snapshot_id,
                application_consistent,
            } => self.capture_recovery_point(
                recovery_point_id,
                volume_id,
                *created_unix_nanos,
                base_snapshot_id.as_deref(),
                *application_consistent,
            ),
            HaCommand::DeleteRecoveryPoint { recovery_point_id } => {
                self.recovery_points
                    .remove(recovery_point_id)
                    .ok_or_else(|| {
                        invalid(format!("unknown recovery point {recovery_point_id}"))
                    })?;
                Ok(())
            }
        }
    }

    fn configure(&mut self, config: &GroupConfig) -> io::Result<()> {
        validate_id(&config.group_id, "group_id")?;
        validate_id(&config.volume_id, "volume_id")?;
        validate_id(&config.log_id, "log_id")?;
        if config.config_epoch == 0 || config.placement_epoch == 0 {
            return Err(invalid("HA configuration epochs must be nonzero"));
        }
        let voters = unique_voters(&config.voters)?;
        if voters.len() < 3 || voters.len() % 2 == 0 {
            return Err(invalid("HA voter count must be odd and at least three"));
        }
        validate_durability_config(config)?;
        let group = self.groups.entry(config.group_id.clone()).or_default();
        if let Some(current) = group.config.as_ref() {
            if config.config_epoch <= current.config_epoch {
                return Err(invalid(format!(
                    "HA config epoch must advance beyond {}",
                    current.config_epoch
                )));
            }
            if config.volume_id != current.volume_id || config.log_id != current.log_id {
                return Err(invalid(
                    "HA reconfiguration cannot change volume_id or log_id",
                ));
            }
        }
        group.config = Some(config.clone());
        group.leader_id.clear();
        group.lease_issued_unix_nanos = 0;
        group.lease_expires_unix_nanos = 0;
        // A durability certificate is bound to the configured replica set and
        // failure-domain policy.  It cannot be carried across a config epoch;
        // the new witnesses must publish a fresh certificate (which may be a
        // deliberately earlier PITR cut). Named snapshot/recovery cuts remain
        // independently pinned in `snapshots` and `recovery_points`.
        group.certified_lane_hwms.clear();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn grant_lease(
        &mut self,
        raft_term: u64,
        group_id: &str,
        leader_id: &str,
        term: u64,
        config_epoch: u64,
        issued: u64,
        expires: u64,
        quorum_voters: &[String],
    ) -> io::Result<()> {
        validate_id(leader_id, "leader_id")?;
        if term != raft_term {
            return Err(invalid("lease term must equal committed Raft entry term"));
        }
        if expires <= issued {
            return Err(invalid("lease expiration must follow issuance"));
        }
        let group = self
            .groups
            .get_mut(group_id)
            .ok_or_else(|| invalid(format!("unknown HA group {group_id}")))?;
        let config = group.config.as_ref().expect("configured group");
        validate_config_epoch(config, config_epoch)?;
        validate_quorum(config, quorum_voters)?;
        if term < group.term {
            return Err(invalid("stale leader term"));
        }
        if term == group.term && !group.leader_id.is_empty() && group.leader_id != leader_id {
            return Err(invalid("leader changed without a higher term"));
        }
        if term == group.term && expires < group.lease_expires_unix_nanos {
            return Err(invalid("lease expiration regressed"));
        }
        group.term = term;
        group.leader_id = leader_id.to_string();
        group.lease_issued_unix_nanos = issued;
        group.lease_expires_unix_nanos = expires;
        Ok(())
    }

    fn publish_hwm(
        &mut self,
        raft_term: u64,
        group_id: &str,
        leader_id: &str,
        term: u64,
        config_epoch: u64,
        reports: &[ReplicaHwm],
    ) -> io::Result<()> {
        if term != raft_term {
            return Err(invalid("HWM term must equal committed Raft entry term"));
        }
        let group = self
            .groups
            .get_mut(group_id)
            .ok_or_else(|| invalid(format!("unknown HA group {group_id}")))?;
        let config = group.config.as_ref().expect("configured group");
        validate_config_epoch(config, config_epoch)?;
        if group.term != term || group.leader_id != leader_id {
            return Err(invalid("HWM publisher is not the fenced group leader"));
        }
        let certificate = certify_durable_hwm(config, term, reports)?;
        for (lane, certified) in certificate.lane_hwms {
            let current = group.certified_lane_hwms.entry(lane).or_default();
            if certified < *current {
                return Err(invalid(format!(
                    "certified lane {lane} HWM regressed from {current} to {certified}"
                )));
            }
            *current = certified;
        }
        Ok(())
    }

    fn volume_cuts(&self, volume_id: &str) -> io::Result<BTreeMap<String, GroupCut>> {
        let mut cuts = BTreeMap::new();
        for (group_id, group) in &self.groups {
            let Some(config) = group.config.as_ref() else {
                continue;
            };
            if config.volume_id == volume_id {
                if group.certified_lane_hwms.is_empty() {
                    return Err(invalid(format!("HA group {group_id} has no certified HWM")));
                }
                cuts.insert(group_id.clone(), group.cut(group_id)?);
            }
        }
        if cuts.is_empty() {
            return Err(invalid(format!("volume {volume_id} has no HA groups")));
        }
        Ok(cuts)
    }

    fn capture_snapshot(
        &mut self,
        snapshot_id: &str,
        volume_id: &str,
        created_unix_nanos: u64,
        application_consistent: bool,
    ) -> io::Result<()> {
        validate_id(snapshot_id, "snapshot_id")?;
        if self.snapshots.contains_key(snapshot_id) {
            return Err(invalid(format!("snapshot {snapshot_id} already exists")));
        }
        let cuts = self.volume_cuts(volume_id)?;
        self.snapshots.insert(
            snapshot_id.to_string(),
            SnapshotRecord {
                snapshot_id: snapshot_id.to_string(),
                volume_id: volume_id.to_string(),
                created_unix_nanos,
                application_consistent,
                cuts,
            },
        );
        Ok(())
    }

    fn delete_snapshot(&mut self, snapshot_id: &str) -> io::Result<()> {
        if self
            .recovery_points
            .values()
            .any(|point| point.base_snapshot_id.as_deref() == Some(snapshot_id))
        {
            return Err(invalid(format!(
                "snapshot {snapshot_id} is pinned by a recovery point"
            )));
        }
        self.snapshots
            .remove(snapshot_id)
            .ok_or_else(|| invalid(format!("unknown snapshot {snapshot_id}")))?;
        Ok(())
    }

    fn capture_recovery_point(
        &mut self,
        recovery_point_id: &str,
        volume_id: &str,
        created_unix_nanos: u64,
        base_snapshot_id: Option<&str>,
        application_consistent: bool,
    ) -> io::Result<()> {
        validate_id(recovery_point_id, "recovery_point_id")?;
        if self.recovery_points.contains_key(recovery_point_id) {
            return Err(invalid(format!(
                "recovery point {recovery_point_id} already exists"
            )));
        }
        let cuts = self.volume_cuts(volume_id)?;
        if let Some(snapshot_id) = base_snapshot_id {
            let snapshot = self
                .snapshots
                .get(snapshot_id)
                .ok_or_else(|| invalid(format!("unknown base snapshot {snapshot_id}")))?;
            if snapshot.volume_id != volume_id {
                return Err(invalid("base snapshot belongs to another volume"));
            }
            cuts_cover(&cuts, &snapshot.cuts)?;
        }
        self.recovery_points.insert(
            recovery_point_id.to_string(),
            RecoveryPoint {
                recovery_point_id: recovery_point_id.to_string(),
                volume_id: volume_id.to_string(),
                created_unix_nanos,
                base_snapshot_id: base_snapshot_id.map(str::to_string),
                application_consistent,
                cuts,
            },
        );
        Ok(())
    }

    pub fn retention_floor(&self, group_id: &str, lane: u32) -> Option<u64> {
        self.snapshots
            .values()
            .filter_map(|snapshot| snapshot.cuts.get(group_id))
            .chain(
                self.recovery_points
                    .values()
                    .filter_map(|point| point.cuts.get(group_id)),
            )
            .filter_map(|cut| cut.lane_hwms.get(&lane).copied())
            .min()
    }
}

fn cuts_cover(
    target: &BTreeMap<String, GroupCut>,
    base: &BTreeMap<String, GroupCut>,
) -> io::Result<()> {
    for (group_id, base_cut) in base {
        let target_cut = target
            .get(group_id)
            .ok_or_else(|| invalid(format!("target cut omits base group {group_id}")))?;
        if target_cut.log_id != base_cut.log_id {
            return Err(invalid(format!(
                "log identity changed for group {group_id}"
            )));
        }
        for (lane, base_hwm) in &base_cut.lane_hwms {
            if target_cut.lane_hwms.get(lane).copied().unwrap_or_default() < *base_hwm {
                return Err(invalid(format!(
                    "target cut precedes base group={group_id} lane={lane}"
                )));
            }
        }
    }
    Ok(())
}

#[repr(align(64))]
pub struct PublishedGroupView {
    sequence: AtomicU64,
    term: AtomicU64,
    config_epoch: AtomicU64,
    placement_epoch: AtomicU64,
    leader_hash: AtomicU64,
    lease_expires_unix_nanos: AtomicU64,
    certified_floor: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadAuthority {
    pub term: u64,
    pub config_epoch: u64,
    pub placement_epoch: u64,
    pub leader_hash: u64,
    pub lease_expires_unix_nanos: u64,
    pub certified_floor: u64,
}

impl PublishedGroupView {
    fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            term: AtomicU64::new(0),
            config_epoch: AtomicU64::new(0),
            placement_epoch: AtomicU64::new(0),
            leader_hash: AtomicU64::new(0),
            lease_expires_unix_nanos: AtomicU64::new(0),
            certified_floor: AtomicU64::new(0),
        }
    }

    fn publish(&self, group: &GroupState) {
        self.sequence.fetch_add(1, Ordering::AcqRel);
        let config = group.config.as_ref();
        self.term.store(group.term, Ordering::Relaxed);
        self.config_epoch.store(
            config.map_or(0, |value| value.config_epoch),
            Ordering::Relaxed,
        );
        self.placement_epoch.store(
            config.map_or(0, |value| value.placement_epoch),
            Ordering::Relaxed,
        );
        self.leader_hash
            .store(stable_id_hash(&group.leader_id), Ordering::Relaxed);
        self.lease_expires_unix_nanos
            .store(group.lease_expires_unix_nanos, Ordering::Relaxed);
        self.certified_floor.store(
            group
                .certified_lane_hwms
                .values()
                .copied()
                .min()
                .unwrap_or_default(),
            Ordering::Relaxed,
        );
        self.sequence.fetch_add(1, Ordering::Release);
    }

    pub fn load(&self) -> ReadAuthority {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let view = ReadAuthority {
                term: self.term.load(Ordering::Relaxed),
                config_epoch: self.config_epoch.load(Ordering::Relaxed),
                placement_epoch: self.placement_epoch.load(Ordering::Relaxed),
                leader_hash: self.leader_hash.load(Ordering::Relaxed),
                lease_expires_unix_nanos: self.lease_expires_unix_nanos.load(Ordering::Relaxed),
                certified_floor: self.certified_floor.load(Ordering::Relaxed),
            };
            let after = self.sequence.load(Ordering::Acquire);
            if before == after {
                return view;
            }
        }
    }

    pub fn authorizes(
        &self,
        leader_id: &str,
        term: u64,
        config_epoch: u64,
        now_unix_nanos: u64,
        required_hwm: u64,
    ) -> bool {
        self.authorizes_hash(
            stable_id_hash(leader_id),
            term,
            config_epoch,
            now_unix_nanos,
            required_hwm,
        )
    }

    /// Hot-path form for lane workers that cached the route's leader hash.
    pub fn authorizes_hash(
        &self,
        leader_hash: u64,
        term: u64,
        config_epoch: u64,
        now_unix_nanos: u64,
        required_hwm: u64,
    ) -> bool {
        let view = self.load();
        view.term == term
            && view.config_epoch == config_epoch
            && view.leader_hash == leader_hash
            && now_unix_nanos < view.lease_expires_unix_nanos
            && required_hwm <= view.certified_floor
    }
}

pub struct HaMetadataStore {
    changes: ChangeLogStore,
    state: Mutex<HaState>,
    views: RwLock<BTreeMap<String, Arc<PublishedGroupView>>>,
}

impl HaMetadataStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let changes = ChangeLogStore::open(path)?;
        let mut state = HaState::default();
        for envelope in changes.replay_from(HA_LOG_STREAM, 0) {
            let mut commands = Vec::with_capacity(envelope.batch.changes.len());
            for change in &envelope.batch.changes {
                if change.component_id != "zcutils.ha" {
                    return Err(invalid(format!(
                        "unexpected component {} in HA stream",
                        change.component_id
                    )));
                }
                let command: HaCommand = serde_json::from_value(change.payload.clone())
                    .map_err(|error| invalid(format!("decode HA command: {error}")))?;
                commands.push(command);
            }
            state.apply_batch(envelope.raft_index, envelope.raft_term, &commands)?;
            if content_hash(&state)? != envelope.batch.resulting_state_hash {
                return Err(invalid(format!(
                    "HA state hash mismatch at revision {}",
                    envelope.batch.new_revision
                )));
            }
        }
        let views = state
            .groups
            .iter()
            .map(|(group_id, group)| {
                let view = Arc::new(PublishedGroupView::new());
                view.publish(group);
                (group_id.clone(), view)
            })
            .collect();
        Ok(Self {
            changes,
            state: Mutex::new(state),
            views: RwLock::new(views),
        })
    }

    pub fn apply_committed(&self, entry: &CommittedHaEntry) -> io::Result<()> {
        self.apply_committed_batch(&CommittedHaBatch {
            index: entry.index,
            term: entry.term,
            transaction_id: format!("raft-{}", entry.index),
            commands: vec![entry.command.clone()],
        })
    }

    pub fn apply_committed_batch(&self, batch: &CommittedHaBatch) -> io::Result<()> {
        let mut state = self.state.lock().expect("HA state mutex poisoned");
        let mut staged = state.clone();
        staged.apply_batch(batch.index, batch.term, &batch.commands)?;
        let expected_revision = self.changes.revision(HA_LOG_STREAM);
        let schema_hash = content_hash(&"zcutils.ha.command.v1")?;
        let changes = batch
            .commands
            .iter()
            .map(|command| {
                Ok(ComponentChange {
                    component_id: "zcutils.ha".to_string(),
                    entity_id: command.key().to_string(),
                    operation: ha_operation(command).to_string(),
                    schema_hash: schema_hash.clone(),
                    payload: serde_json::to_value(command).map_err(|error| {
                        invalid(format!("encode HA command for change batch: {error}"))
                    })?,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let envelope = Arc::new(CommittedChangeBatch {
            raft_term: batch.term,
            raft_index: batch.index,
            batch: ChangeBatch {
                schema_version: CHANGE_BATCH_SCHEMA_VERSION,
                stream_id: HA_LOG_STREAM.to_string(),
                transaction_id: batch.transaction_id.clone(),
                expected_revision,
                new_revision: expected_revision.saturating_add(1),
                topology_epoch: staged
                    .groups
                    .values()
                    .filter_map(|group| group.config.as_ref())
                    .map(|config| config.placement_epoch)
                    .max()
                    .unwrap_or_default(),
                changes,
                referenced_object_hashes: Vec::new(),
                resulting_state_hash: content_hash(&staged)?,
            },
        });
        self.changes.persist(&envelope)?;
        *state = staged;
        let affected_groups: BTreeSet<&str> = batch
            .commands
            .iter()
            .filter_map(|command| match command {
                HaCommand::ConfigureGroup { config } => Some(config.group_id.as_str()),
                HaCommand::GrantLease { group_id, .. } | HaCommand::PublishHwm { group_id, .. } => {
                    Some(group_id.as_str())
                }
                _ => None,
            })
            .collect();
        for group_id in affected_groups {
            let Some(group) = state.groups.get(group_id) else {
                continue;
            };
            let view = {
                let mut views = self.views.write().expect("HA views lock poisoned");
                Arc::clone(
                    views
                        .entry(group_id.to_string())
                        .or_insert_with(|| Arc::new(PublishedGroupView::new())),
                )
            };
            view.publish(group);
        }
        drop(state);
        self.changes.publish(envelope);
        Ok(())
    }

    pub fn subscribe_changes(&self) -> Receiver<Arc<CommittedChangeBatch>> {
        self.changes.subscribe()
    }

    pub fn change_revision(&self) -> u64 {
        self.changes.revision(HA_LOG_STREAM)
    }

    pub fn state(&self) -> HaState {
        self.state.lock().expect("HA state mutex poisoned").clone()
    }

    /// Resolve this once when routing changes; retain the returned Arc on the
    /// lane worker so ordinary reads never acquire the registry lock.
    pub fn published_view(&self, group_id: &str) -> Option<Arc<PublishedGroupView>> {
        self.views
            .read()
            .expect("HA views lock poisoned")
            .get(group_id)
            .cloned()
    }
}

fn ha_operation(command: &HaCommand) -> &'static str {
    match command {
        HaCommand::ConfigureGroup { .. } => "group.configure",
        HaCommand::GrantLease { .. } => "lease.grant",
        HaCommand::PublishHwm { .. } => "hwm.publish",
        HaCommand::CaptureSnapshot { .. } => "snapshot.capture",
        HaCommand::DeleteSnapshot { .. } => "snapshot.delete",
        HaCommand::CaptureRecoveryPoint { .. } => "recovery_point.capture",
        HaCommand::DeleteRecoveryPoint { .. } => "recovery_point.delete",
    }
}

pub fn stable_id_hash(value: &str) -> u64 {
    let digest = Sha256::digest(value.as_bytes());
    u64::from_le_bytes(digest[..8].try_into().expect("SHA-256 prefix"))
}

fn quorum_size(voters: usize) -> usize {
    voters / 2 + 1
}

fn unique_voters(voters: &[String]) -> io::Result<BTreeSet<&str>> {
    let mut unique = BTreeSet::new();
    for voter in voters {
        validate_id(voter, "voter")?;
        if !unique.insert(voter.as_str()) {
            return Err(invalid(format!("duplicate HA voter {voter}")));
        }
    }
    Ok(unique)
}

fn validate_quorum(config: &GroupConfig, acknowledgements: &[String]) -> io::Result<()> {
    let voters: BTreeSet<&str> = config.voters.iter().map(String::as_str).collect();
    let acknowledgements = unique_voters(acknowledgements)?;
    if !acknowledgements.iter().all(|voter| voters.contains(voter)) {
        return Err(invalid("lease acknowledgement contains a non-voter"));
    }
    let required = quorum_size(voters.len());
    if acknowledgements.len() < required {
        return Err(invalid(format!(
            "lease has {} acknowledgements but needs {required}",
            acknowledgements.len()
        )));
    }
    Ok(())
}

fn validate_durability_config(config: &GroupConfig) -> io::Result<()> {
    if config.durability.required_distinct_failure_domains < 2 {
        return Err(invalid(
            "HA durability must require at least two independent failure domains",
        ));
    }
    let mut replica_ids = BTreeSet::new();
    let mut failure_domains = BTreeSet::new();
    let mut leaf_count = 0usize;
    let mut hop_count = 0usize;
    for replica in &config.data_replicas {
        validate_id(&replica.replica_id, "data replica")?;
        validate_id(&replica.failure_domain, "failure domain")?;
        if !replica_ids.insert(replica.replica_id.as_str()) {
            return Err(invalid(format!(
                "duplicate data replica {}",
                replica.replica_id
            )));
        }
        failure_domains.insert(replica.failure_domain.as_str());
        leaf_count += usize::from(replica.role == DataReplicaRole::Leaf);
        hop_count += usize::from(replica.role == DataReplicaRole::Hop);
    }
    if failure_domains.len() < config.durability.required_distinct_failure_domains {
        return Err(invalid(
            "configured data replicas cannot satisfy the failure-domain policy",
        ));
    }
    if leaf_count < config.durability.required_leaf_witnesses {
        return Err(invalid(
            "configured data replicas cannot satisfy the downstream-leaf policy",
        ));
    }
    if hop_count < config.durability.required_hop_witnesses {
        return Err(invalid(
            "configured data replicas cannot satisfy the retained-hop policy",
        ));
    }
    Ok(())
}

fn validate_config_epoch(config: &GroupConfig, epoch: u64) -> io::Result<()> {
    if config.config_epoch != epoch {
        return Err(invalid(format!(
            "HA config epoch mismatch expected={} got={epoch}",
            config.config_epoch
        )));
    }
    Ok(())
}

fn validate_id(value: &str, label: &str) -> io::Result<()> {
    if value.is_empty() || value.contains('\0') || value.contains('\n') {
        return Err(invalid(format!(
            "invalid empty or control-containing {label}"
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn config() -> GroupConfig {
        GroupConfig {
            group_id: "g0".into(),
            volume_id: "v0".into(),
            log_id: "log-a".into(),
            config_epoch: 1,
            placement_epoch: 7,
            voters: vec!["a".into(), "b".into(), "c".into()],
            data_replicas: vec![
                DataReplica {
                    replica_id: "a".into(),
                    role: DataReplicaRole::Hop,
                    failure_domain: "hop-host".into(),
                },
                DataReplica {
                    replica_id: "b".into(),
                    role: DataReplicaRole::Leaf,
                    failure_domain: "leaf-host-b".into(),
                },
                DataReplica {
                    replica_id: "c".into(),
                    role: DataReplicaRole::Leaf,
                    failure_domain: "leaf-host-c".into(),
                },
            ],
            durability: DurabilityPolicy {
                required_distinct_failure_domains: 2,
                required_hop_witnesses: 1,
                required_leaf_witnesses: 1,
            },
        }
    }

    fn entry(index: u64, term: u64, command: HaCommand) -> CommittedHaEntry {
        CommittedHaEntry {
            index,
            term,
            command,
        }
    }

    fn configured_state() -> HaState {
        let mut state = HaState::default();
        state
            .apply(&entry(1, 1, HaCommand::ConfigureGroup { config: config() }))
            .unwrap();
        state
            .apply(&entry(
                2,
                2,
                HaCommand::GrantLease {
                    group_id: "g0".into(),
                    leader_id: "a".into(),
                    term: 2,
                    config_epoch: 1,
                    issued_unix_nanos: 100,
                    expires_unix_nanos: 200,
                    quorum_voters: vec!["a".into(), "b".into()],
                },
            ))
            .unwrap();
        state
    }

    fn report(replica: &str, hwm0: u64, hwm1: u64) -> ReplicaHwm {
        ReplicaHwm {
            replica_id: replica.into(),
            term: 2,
            config_epoch: 1,
            log_id: "log-a".into(),
            lane_hwms: BTreeMap::from([(0, hwm0), (1, hwm1)]),
        }
    }

    #[test]
    fn coverage_hwm_requires_the_retained_hop_and_a_leaf() {
        let mut state = configured_state();
        state
            .apply(&entry(
                3,
                2,
                HaCommand::PublishHwm {
                    group_id: "g0".into(),
                    leader_id: "a".into(),
                    term: 2,
                    config_epoch: 1,
                    reports: vec![
                        report("a", 100, 80),
                        report("b", 90, 95),
                        report("c", 70, 85),
                    ],
                },
            ))
            .unwrap();
        assert_eq!(
            state.groups["g0"].certified_lane_hwms,
            BTreeMap::from([(0, 90), (1, 80)])
        );
    }

    #[test]
    fn stale_terms_wrong_logs_and_nonvoters_are_rejected() {
        let mut state = configured_state();
        let mut bad = report("b", 90, 90);
        bad.log_id = "other".into();
        assert!(
            state
                .apply(&entry(
                    3,
                    2,
                    HaCommand::PublishHwm {
                        group_id: "g0".into(),
                        leader_id: "a".into(),
                        term: 2,
                        config_epoch: 1,
                        reports: vec![report("a", 100, 100), bad],
                    },
                ))
                .is_err()
        );
        assert_eq!(state.applied_index, 2);
    }

    #[test]
    fn reconfiguration_requires_a_fresh_hwm_certificate() {
        let mut state = configured_state();
        state
            .apply(&entry(
                3,
                2,
                HaCommand::PublishHwm {
                    group_id: "g0".into(),
                    leader_id: "a".into(),
                    term: 2,
                    config_epoch: 1,
                    reports: vec![report("a", 100, 100), report("b", 90, 90)],
                },
            ))
            .unwrap();
        assert!(!state.groups["g0"].certified_lane_hwms.is_empty());
        let mut replacement = config();
        replacement.config_epoch = 2;
        replacement.placement_epoch = 8;
        state
            .apply(&entry(
                4,
                2,
                HaCommand::ConfigureGroup {
                    config: replacement,
                },
            ))
            .unwrap();
        assert!(state.groups["g0"].certified_lane_hwms.is_empty());
    }

    #[test]
    fn published_lease_fails_closed_at_expiration_and_hwm() {
        let state = configured_state();
        let group = &state.groups["g0"];
        let view = PublishedGroupView::new();
        view.publish(group);
        assert!(view.authorizes("a", 2, 1, 199, 0));
        assert!(!view.authorizes("a", 2, 1, 200, 0));
        assert!(!view.authorizes("b", 2, 1, 199, 0));
        assert!(!view.authorizes("a", 2, 1, 199, 1));
    }

    #[test]
    fn higher_term_leader_fences_the_old_lease() {
        let mut state = configured_state();
        state
            .apply(&entry(
                3,
                3,
                HaCommand::GrantLease {
                    group_id: "g0".into(),
                    leader_id: "c".into(),
                    term: 3,
                    config_epoch: 1,
                    issued_unix_nanos: 150,
                    expires_unix_nanos: 250,
                    quorum_voters: vec!["b".into(), "c".into()],
                },
            ))
            .unwrap();
        let view = PublishedGroupView::new();
        view.publish(&state.groups["g0"]);
        assert!(!view.authorizes("a", 2, 1, 175, 0));
        assert!(view.authorizes("c", 3, 1, 175, 0));
    }

    #[test]
    fn direct_fsync_certificate_does_not_require_state_machine_apply() {
        let config = config();
        let certificate =
            certify_durable_hwm(&config, 2, &[report("a", 120, 110), report("c", 100, 115)])
                .unwrap();
        assert_eq!(certificate.lane_hwms, BTreeMap::from([(0, 100), (1, 110)]));
        assert_eq!(certificate.reporter_ids, vec!["a", "c"]);
    }

    #[test]
    fn colocated_hop_and_leaf_do_not_count_as_two_copies() {
        let mut config = config();
        config.data_replicas[1].failure_domain = "hop-host".into();
        assert!(
            certify_durable_hwm(&config, 2, &[report("a", 120, 110), report("b", 100, 100)],)
                .is_err()
        );
    }

    #[test]
    fn snapshot_captures_a_multiraft_vector() {
        let mut state = configured_state();
        state
            .apply(&entry(
                3,
                2,
                HaCommand::PublishHwm {
                    group_id: "g0".into(),
                    leader_id: "a".into(),
                    term: 2,
                    config_epoch: 1,
                    reports: vec![report("a", 20, 30), report("b", 18, 25)],
                },
            ))
            .unwrap();
        let mut second = config();
        second.group_id = "g1".into();
        second.log_id = "log-b".into();
        state
            .apply(&entry(
                4,
                2,
                HaCommand::ConfigureGroup {
                    config: second.clone(),
                },
            ))
            .unwrap();
        state
            .apply(&entry(
                5,
                3,
                HaCommand::GrantLease {
                    group_id: "g1".into(),
                    leader_id: "b".into(),
                    term: 3,
                    config_epoch: 1,
                    issued_unix_nanos: 100,
                    expires_unix_nanos: 200,
                    quorum_voters: vec!["a".into(), "b".into()],
                },
            ))
            .unwrap();
        let second_report = |replica_id: &str, hwm: u64| ReplicaHwm {
            replica_id: replica_id.into(),
            term: 3,
            config_epoch: 1,
            log_id: "log-b".into(),
            lane_hwms: BTreeMap::from([(7, hwm)]),
        };
        state
            .apply(&entry(
                6,
                3,
                HaCommand::PublishHwm {
                    group_id: "g1".into(),
                    leader_id: "b".into(),
                    term: 3,
                    config_epoch: 1,
                    reports: vec![second_report("a", 42), second_report("b", 40)],
                },
            ))
            .unwrap();
        state
            .apply(&entry(
                7,
                3,
                HaCommand::CaptureSnapshot {
                    snapshot_id: "multi".into(),
                    volume_id: "v0".into(),
                    created_unix_nanos: 180,
                    application_consistent: true,
                },
            ))
            .unwrap();
        let snapshot = &state.snapshots["multi"];
        assert_eq!(snapshot.cuts.len(), 2);
        assert_eq!(snapshot.cuts["g0"].lane_hwms[&0], 18);
        assert_eq!(snapshot.cuts["g1"].lane_hwms[&7], 40);
    }

    #[test]
    fn pitr_cut_pins_snapshot_and_survives_replay() {
        let path = temp_path("pitr-replay");
        let store = HaMetadataStore::open(&path).unwrap();
        store
            .apply_committed(&entry(1, 1, HaCommand::ConfigureGroup { config: config() }))
            .unwrap();
        store
            .apply_committed(&entry(
                2,
                2,
                HaCommand::GrantLease {
                    group_id: "g0".into(),
                    leader_id: "a".into(),
                    term: 2,
                    config_epoch: 1,
                    issued_unix_nanos: 100,
                    expires_unix_nanos: 200,
                    quorum_voters: vec!["a".into(), "b".into()],
                },
            ))
            .unwrap();
        store
            .apply_committed(&entry(
                3,
                2,
                HaCommand::PublishHwm {
                    group_id: "g0".into(),
                    leader_id: "a".into(),
                    term: 2,
                    config_epoch: 1,
                    reports: vec![report("a", 100, 90), report("b", 90, 80)],
                },
            ))
            .unwrap();
        store
            .apply_committed(&entry(
                4,
                2,
                HaCommand::CaptureSnapshot {
                    snapshot_id: "s0".into(),
                    volume_id: "v0".into(),
                    created_unix_nanos: 150,
                    application_consistent: false,
                },
            ))
            .unwrap();
        store
            .apply_committed(&entry(
                5,
                2,
                HaCommand::CaptureRecoveryPoint {
                    recovery_point_id: "p0".into(),
                    volume_id: "v0".into(),
                    created_unix_nanos: 160,
                    base_snapshot_id: Some("s0".into()),
                    application_consistent: false,
                },
            ))
            .unwrap();
        assert!(
            store
                .apply_committed(&entry(
                    6,
                    2,
                    HaCommand::DeleteSnapshot {
                        snapshot_id: "s0".into()
                    }
                ))
                .is_err()
        );
        drop(store);
        let reopened = HaMetadataStore::open(&path).unwrap();
        let state = reopened.state();
        assert_eq!(state.applied_index, 5);
        assert_eq!(state.retention_floor("g0", 0), Some(90));
        assert_eq!(state.recovery_points["p0"].cuts["g0"].lane_hwms[&1], 80);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_atomic_batch_changes_neither_state_nor_durable_revision() {
        let path = temp_path("atomic-reject");
        let store = HaMetadataStore::open(&path).unwrap();
        let batch = CommittedHaBatch {
            index: 1,
            term: 1,
            transaction_id: "configure-and-invalid-delete".into(),
            commands: vec![
                HaCommand::ConfigureGroup { config: config() },
                HaCommand::DeleteSnapshot {
                    snapshot_id: "missing".into(),
                },
            ],
        };
        assert!(store.apply_committed_batch(&batch).is_err());
        assert_eq!(store.state(), HaState::default());
        assert_eq!(store.change_revision(), 0);
        drop(store);
        assert_eq!(
            HaMetadataStore::open(&path).unwrap().state(),
            HaState::default()
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn atomic_configure_and_lease_is_one_notification_and_replays() {
        let path = temp_path("atomic-configure-lease");
        let store = HaMetadataStore::open(&path).unwrap();
        let notifications = store.subscribe_changes();
        let batch = CommittedHaBatch {
            index: 1,
            term: 1,
            transaction_id: "configure-and-lease".into(),
            commands: vec![
                HaCommand::ConfigureGroup { config: config() },
                HaCommand::GrantLease {
                    group_id: "g0".into(),
                    leader_id: "a".into(),
                    term: 1,
                    config_epoch: 1,
                    issued_unix_nanos: 100,
                    expires_unix_nanos: 200,
                    quorum_voters: vec!["a".into(), "b".into()],
                },
            ],
        };
        store.apply_committed_batch(&batch).unwrap();
        let notification = notifications.recv().unwrap();
        assert_eq!(notification.batch.transaction_id, "configure-and-lease");
        assert_eq!(notification.batch.changes.len(), 2);
        assert_eq!(store.change_revision(), 1);
        assert!(
            store
                .published_view("g0")
                .unwrap()
                .authorizes("a", 1, 1, 150, 0)
        );
        let expected = store.state();
        drop(store);
        assert_eq!(HaMetadataStore::open(&path).unwrap().state(), expected);
        fs::remove_file(path).unwrap();
    }

    fn temp_path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("zc-ha-{label}-{}-{nonce}.log", std::process::id()))
    }
}
