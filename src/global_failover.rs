//! Platform-neutral global volume failover state machine.
//!
//! Kubernetes is one adapter.  It never owns volume custody: regional storage
//! agents publish durable progress, generic clients publish sessions, and
//! workload adapters (Kubernetes or otherwise) reconcile actions emitted from
//! the same committed operation.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::io;

pub type LaneCuts = BTreeMap<u32, u64>;
pub type VolumeCuts = BTreeMap<String, LaneCuts>;

const MAX_VOLUMES: usize = 4_096;
const MAX_CONSISTENCY_SETS: usize = 1_024;
const MAX_SET_MEMBERS: usize = 256;
const MAX_LANES_PER_VOLUME: usize = 256;
const MAX_REGIONAL_OBSERVATIONS: usize = 16_384;
const MAX_CHECKPOINTS: usize = 16_384;
const MAX_SESSIONS: usize = 65_536;
const MAX_OPERATIONS: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    Kubernetes,
    External,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadFailoverPolicy {
    Stay,
    FollowVolume,
    ObserveOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadBinding {
    pub binding_id: String,
    pub adapter_id: String,
    pub adapter_kind: AdapterKind,
    pub policy: WorkloadFailoverPolicy,
    pub source_replicas: u32,
    pub target_replicas: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSpec {
    pub volume_id: String,
    pub authority_region: String,
    pub placement_epoch: u64,
    pub consistency_set_id: Option<String>,
    pub workload_bindings: Vec<WorkloadBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutDependency {
    /// The `before` application hook must quiesce before `after`.
    pub before: String,
    pub after: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsistencySetSpec {
    pub set_id: String,
    pub members: BTreeSet<String>,
    pub dependencies: Vec<CutDependency>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionalVolumeProgress {
    pub volume_id: String,
    pub region: String,
    pub observed_revision: u64,
    pub placement_epoch: u64,
    #[serde(with = "lane_cuts_json")]
    pub durable_lane_hwms: LaneCuts,
    #[serde(with = "lane_cuts_json")]
    pub applied_lane_hwms: LaneCuts,
    pub configured_replicas: u32,
    pub required_quorum: u32,
    pub live_replicas: u32,
    pub durable_failure_domains: BTreeSet<String>,
    pub reachable: bool,
    pub quiesced: bool,
}

impl RegionalVolumeProgress {
    pub fn is_ha(&self) -> bool {
        self.configured_replicas >= 3
            && self.required_quorum >= 2
            && self.required_quorum > self.configured_replicas / 2
            && self.live_replicas >= self.required_quorum
            && self.durable_failure_domains.len() >= self.configured_replicas as usize
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsistencyCheckpoint {
    pub checkpoint_id: String,
    pub set_id: String,
    pub sequence: u64,
    #[serde(with = "volume_cuts_json")]
    pub cuts: VolumeCuts,
    pub application_consistent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
    Fenced,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSession {
    pub session_id: String,
    pub volume_id: String,
    pub frontend: String,
    pub region: String,
    pub observed_placement_epoch: u64,
    /// The frontend can atomically redirect this live session to a committed
    /// clean-cut placement epoch without exposing data from both epochs.
    /// Older serialized sessions default to false and are fenced safely.
    #[serde(default)]
    pub supports_transparent_rebind: bool,
    pub state: SessionState,
    pub fence_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum FailoverMode {
    Clean,
    AcceptDeclaredLoss {
        reason: String,
        max_missing_operations_per_lane: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailoverRequest {
    pub operation_id: String,
    pub requested_volume_ids: BTreeSet<String>,
    pub source_region: String,
    pub target_region: String,
    pub expected_revision: u64,
    pub mode: FailoverMode,
}

/// Deterministic mutations admitted by the low-rate global Raft log.
///
/// Storage observations are committed too: a failover decision is therefore
/// reproducible from the log rather than depending on whichever leader's
/// volatile view happened to win an election.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum GlobalFailoverCommand {
    PutVolume {
        expected_revision: u64,
        spec: VolumeSpec,
    },
    PutConsistencySet {
        expected_revision: u64,
        spec: ConsistencySetSpec,
    },
    ObserveRegion {
        progress: RegionalVolumeProgress,
    },
    PublishCheckpoint {
        checkpoint: ConsistencyCheckpoint,
    },
    RegisterSession {
        session: ClientSession,
    },
    RequestFailover {
        request: FailoverRequest,
    },
    Reconcile {
        operation_id: String,
    },
    AcknowledgeWorkloadAction {
        action_id: String,
        adapter_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailoverPhase {
    Quiescing,
    WaitingForReplica,
    AwaitingLossAcceptance,
    FencingSource,
    PromotingTarget,
    ReconcilingWorkloads,
    Completed,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredLaneLoss {
    pub volume_id: String,
    pub lane: u32,
    pub last_known_source_hwm: u64,
    pub accepted_target_hwm: u64,
    pub first_missing: Option<u64>,
    pub last_missing: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LossRecord {
    pub operation_id: String,
    pub reason: String,
    pub checkpoint_id: String,
    pub losses: Vec<DeclaredLaneLoss>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadAction {
    pub action_id: String,
    pub operation_id: String,
    pub volume_id: String,
    pub binding_id: String,
    pub adapter_id: String,
    pub adapter_kind: AdapterKind,
    pub policy: WorkloadFailoverPolicy,
    pub source_region: String,
    pub target_region: String,
    pub source_replicas: u32,
    pub target_replicas: u32,
    pub add_source_taint: bool,
    pub remove_target_taint: bool,
    /// The global operation has formally declared the source region lost.
    /// Adapters may use destructive control-plane cleanup for stale workload
    /// objects only when this committed bit is true.
    #[serde(default)]
    pub source_region_lost: bool,
    pub acknowledged: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailoverOperation {
    pub operation_id: String,
    pub source_region: String,
    pub target_region: String,
    pub volume_ids: BTreeSet<String>,
    /// Configuration revision against which regional observations and the
    /// custody decision were admitted.
    #[serde(default)]
    pub decision_revision: u64,
    pub mode: FailoverMode,
    pub phase: FailoverPhase,
    /// Deterministic dependency order in which application hooks quiesce.
    #[serde(default)]
    pub quiesce_order: Vec<String>,
    /// Reverse dependency order in which applications may resume.
    #[serde(default)]
    pub resume_order: Vec<String>,
    pub checkpoint_id: Option<String>,
    #[serde(with = "volume_cuts_json")]
    pub cut: VolumeCuts,
    pub old_epochs: BTreeMap<String, u64>,
    pub new_epochs: BTreeMap<String, u64>,
    pub loss_record: Option<LossRecord>,
    pub blocked_reason: Option<String>,
    pub workload_action_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailoverState {
    pub revision: u64,
    pub volumes: BTreeMap<String, VolumeSpec>,
    pub consistency_sets: BTreeMap<String, ConsistencySetSpec>,
    /// JSON-safe volume -> region -> observation index.
    pub regional_progress: BTreeMap<String, BTreeMap<String, RegionalVolumeProgress>>,
    pub checkpoints: BTreeMap<String, ConsistencyCheckpoint>,
    pub sessions: BTreeMap<String, ClientSession>,
    pub operations: BTreeMap<String, FailoverOperation>,
    pub workload_actions: BTreeMap<String, WorkloadAction>,
    pub loss_records: Vec<LossRecord>,
}

impl FailoverState {
    pub fn apply(&mut self, command: &GlobalFailoverCommand) -> io::Result<()> {
        match command {
            GlobalFailoverCommand::PutVolume {
                expected_revision,
                spec,
            } => self
                .put_volume(*expected_revision, spec.clone())
                .map(|_| ()),
            GlobalFailoverCommand::PutConsistencySet {
                expected_revision,
                spec,
            } => self
                .put_consistency_set(*expected_revision, spec.clone())
                .map(|_| ()),
            GlobalFailoverCommand::ObserveRegion { progress } => {
                self.observe_region(progress.clone())
            }
            GlobalFailoverCommand::PublishCheckpoint { checkpoint } => {
                self.publish_checkpoint(checkpoint.clone())
            }
            GlobalFailoverCommand::RegisterSession { session } => {
                self.register_session(session.clone())
            }
            GlobalFailoverCommand::RequestFailover { request } => {
                self.request_failover(request.clone()).map(|_| ())
            }
            GlobalFailoverCommand::Reconcile { operation_id } => {
                self.reconcile(operation_id).map(|_| ())
            }
            GlobalFailoverCommand::AcknowledgeWorkloadAction {
                action_id,
                adapter_id,
            } => self.acknowledge_workload_action(action_id, adapter_id),
        }
    }

    pub fn put_volume(&mut self, expected_revision: u64, spec: VolumeSpec) -> io::Result<u64> {
        self.require_revision(expected_revision)?;
        validate_id(&spec.volume_id, "volume_id")?;
        validate_id(&spec.authority_region, "authority_region")?;
        if spec.placement_epoch == u64::MAX {
            return Err(invalid("placement epoch has no successor"));
        }
        if let Some(set_id) = &spec.consistency_set_id {
            validate_id(set_id, "consistency_set_id")?;
        }
        if let Some(existing) = self.volumes.get(&spec.volume_id) {
            if spec.authority_region != existing.authority_region
                || spec.placement_epoch != existing.placement_epoch
            {
                return Err(invalid(
                    "volume custody and placement epoch may change only through failover",
                ));
            }
            if spec.consistency_set_id != existing.consistency_set_id {
                return Err(invalid(
                    "consistency-set membership is owned by put_consistency_set",
                ));
            }
            if spec.workload_bindings != existing.workload_bindings
                && self.operations.values().any(|operation| {
                    operation.volume_ids.contains(&spec.volume_id)
                        && !matches!(
                            operation.phase,
                            FailoverPhase::Completed | FailoverPhase::Blocked
                        )
                })
            {
                return Err(invalid(
                    "workload bindings cannot change during an active failover",
                ));
            }
        } else if spec.consistency_set_id.is_some() {
            return Err(invalid(
                "new volume consistency-set membership must be declared atomically by put_consistency_set",
            ));
        }
        let mut bindings = BTreeSet::new();
        for binding in &spec.workload_bindings {
            validate_id(&binding.binding_id, "binding_id")?;
            validate_id(&binding.adapter_id, "adapter_id")?;
            if !bindings.insert(binding.binding_id.as_str()) {
                return Err(invalid("duplicate workload binding id"));
            }
        }
        if self.volumes.len() >= MAX_VOLUMES && !self.volumes.contains_key(&spec.volume_id) {
            return Err(invalid("global failover volume limit reached"));
        }
        self.volumes.insert(spec.volume_id.clone(), spec);
        Ok(self.bump_revision())
    }

    pub fn put_consistency_set(
        &mut self,
        expected_revision: u64,
        spec: ConsistencySetSpec,
    ) -> io::Result<u64> {
        self.require_revision(expected_revision)?;
        validate_id(&spec.set_id, "set_id")?;
        if spec.members.is_empty() || spec.members.len() > MAX_SET_MEMBERS {
            return Err(invalid("consistency set must contain at least one volume"));
        }
        if self.consistency_sets.len() >= MAX_CONSISTENCY_SETS
            && !self.consistency_sets.contains_key(&spec.set_id)
        {
            return Err(invalid("global consistency-set limit reached"));
        }
        let previous = self.consistency_sets.get(&spec.set_id).cloned();
        if previous.as_ref().is_some_and(|existing| existing != &spec)
            && (self
                .checkpoints
                .values()
                .any(|checkpoint| checkpoint.set_id == spec.set_id)
                || self.operations.values().any(|operation| {
                    operation.volume_ids.iter().any(|volume| {
                        previous
                            .as_ref()
                            .is_some_and(|existing| existing.members.contains(volume))
                            || spec.members.contains(volume)
                    })
                }))
        {
            return Err(invalid(
                "consistency set is immutable after a checkpoint or failover; create a new version",
            ));
        }
        for member in &spec.members {
            let volume = self
                .volumes
                .get(member)
                .ok_or_else(|| invalid(format!("unknown consistency-set volume {member}")))?;
            if volume
                .consistency_set_id
                .as_deref()
                .is_some_and(|set| set != spec.set_id)
            {
                return Err(invalid(format!(
                    "volume {member} already belongs to another atomic consistency set"
                )));
            }
        }
        dependency_order(&spec)?;
        if let Some(previous) = &previous {
            for removed in previous.members.difference(&spec.members) {
                let volume = self.volumes.get_mut(removed).expect("validated old member");
                if volume.consistency_set_id.as_deref() == Some(spec.set_id.as_str()) {
                    volume.consistency_set_id = None;
                }
            }
        }
        for member in &spec.members {
            self.volumes
                .get_mut(member)
                .expect("validated member")
                .consistency_set_id = Some(spec.set_id.clone());
        }
        self.consistency_sets.insert(spec.set_id.clone(), spec);
        Ok(self.bump_revision())
    }

    pub fn observe_region(&mut self, progress: RegionalVolumeProgress) -> io::Result<()> {
        let volume = self
            .volumes
            .get(&progress.volume_id)
            .ok_or_else(|| invalid("regional observation names an unknown volume"))?;
        validate_id(&progress.region, "region")?;
        if progress.configured_replicas == 0
            || progress.required_quorum == 0
            || progress.required_quorum > progress.configured_replicas
            || progress.live_replicas > progress.configured_replicas
        {
            return Err(invalid("regional replica/quorum counts are inconsistent"));
        }
        if progress.placement_epoch > volume.placement_epoch {
            return Err(invalid(
                "regional observation is ahead of the committed custody epoch",
            ));
        }
        if progress.durable_lane_hwms.is_empty() || progress.applied_lane_hwms.is_empty() {
            return Err(invalid("regional observation must report lane HWMs"));
        }
        if progress.durable_lane_hwms.len() > MAX_LANES_PER_VOLUME
            || progress.applied_lane_hwms.len() > MAX_LANES_PER_VOLUME
            || progress.durable_failure_domains.len() > MAX_SET_MEMBERS
        {
            return Err(invalid("regional observation exceeds structural limits"));
        }
        if progress.durable_lane_hwms.keys().collect::<BTreeSet<_>>()
            != progress.applied_lane_hwms.keys().collect::<BTreeSet<_>>()
        {
            return Err(invalid(
                "regional durable/applied observations must name exactly the same lanes",
            ));
        }
        if !cuts_cover(&progress.durable_lane_hwms, &progress.applied_lane_hwms) {
            return Err(invalid("applied HWM exceeds durable HWM"));
        }
        let exists = self
            .regional_progress
            .get(&progress.volume_id)
            .is_some_and(|regions| regions.contains_key(&progress.region));
        let observations = self
            .regional_progress
            .values()
            .map(BTreeMap::len)
            .sum::<usize>();
        if observations >= MAX_REGIONAL_OBSERVATIONS && !exists {
            return Err(invalid("regional observation limit reached"));
        }
        self.regional_progress
            .entry(progress.volume_id.clone())
            .or_default()
            .insert(progress.region.clone(), progress);
        Ok(())
    }

    pub fn publish_checkpoint(&mut self, checkpoint: ConsistencyCheckpoint) -> io::Result<()> {
        validate_id(&checkpoint.checkpoint_id, "checkpoint_id")?;
        validate_id(&checkpoint.set_id, "set_id")?;
        let set = self
            .consistency_sets
            .get(&checkpoint.set_id)
            .ok_or_else(|| invalid("checkpoint names an unknown consistency set"))?;
        if checkpoint.cuts.keys().collect::<BTreeSet<_>>() != set.members.iter().collect() {
            return Err(invalid(
                "checkpoint cut does not exactly cover its consistency set",
            ));
        }
        if self
            .checkpoints
            .get(&checkpoint.checkpoint_id)
            .is_some_and(|existing| existing != &checkpoint)
        {
            return Err(invalid(
                "checkpoint id is immutable and already names a different cut",
            ));
        }
        if self.checkpoints.values().any(|existing| {
            existing.checkpoint_id != checkpoint.checkpoint_id
                && existing.set_id == checkpoint.set_id
                && existing.sequence == checkpoint.sequence
        }) {
            return Err(invalid(
                "checkpoint sequence is already present in this set",
            ));
        }
        if checkpoint
            .cuts
            .values()
            .any(|lanes| lanes.is_empty() || lanes.len() > MAX_LANES_PER_VOLUME)
        {
            return Err(invalid("checkpoint lane cut exceeds structural limits"));
        }
        if self.checkpoints.len() >= MAX_CHECKPOINTS
            && !self.checkpoints.contains_key(&checkpoint.checkpoint_id)
        {
            return Err(invalid("global checkpoint limit reached"));
        }
        self.checkpoints
            .insert(checkpoint.checkpoint_id.clone(), checkpoint);
        Ok(())
    }

    pub fn register_session(&mut self, session: ClientSession) -> io::Result<()> {
        let volume = self
            .volumes
            .get(&session.volume_id)
            .ok_or_else(|| invalid("session names an unknown volume"))?;
        validate_id(&session.session_id, "session_id")?;
        validate_id(&session.frontend, "frontend")?;
        validate_id(&session.region, "session region")?;
        if session.state == SessionState::Active
            && session.observed_placement_epoch != volume.placement_epoch
        {
            return Err(invalid("session is stale at registration"));
        }
        if self.sessions.len() >= MAX_SESSIONS && !self.sessions.contains_key(&session.session_id) {
            return Err(invalid("global client-session limit reached"));
        }
        self.sessions.insert(session.session_id.clone(), session);
        Ok(())
    }

    pub fn request_failover(&mut self, request: FailoverRequest) -> io::Result<u64> {
        if request.source_region == request.target_region {
            return Err(invalid("source and target regions must differ"));
        }
        let volumes = self.expand_atomic_scope(&request.requested_volume_ids)?;
        if let Some(existing) = self.operations.get(&request.operation_id) {
            if existing.source_region == request.source_region
                && existing.target_region == request.target_region
                && existing.volume_ids == volumes
                && existing.mode == request.mode
            {
                return Ok(self.revision);
            }
            return Err(invalid(
                "failover operation id already identifies a different request",
            ));
        }
        self.require_revision(request.expected_revision)?;
        if self.operations.len() >= MAX_OPERATIONS {
            return Err(invalid("global failover operation limit reached"));
        }
        let mut old_epochs = BTreeMap::new();
        for volume_id in &volumes {
            let volume = &self.volumes[volume_id];
            if volume.authority_region != request.source_region {
                return Err(invalid(format!(
                    "volume {volume_id} is not authoritative in the requested source region"
                )));
            }
            old_epochs.insert(volume_id.clone(), volume.placement_epoch);
            self.require_fresh_region_observation(
                volume_id,
                &request.source_region,
                request.expected_revision,
            )?;
            self.require_region_ha(volume_id, &request.target_region, request.expected_revision)?;
        }
        let set_id = self.volumes[volumes.iter().next().expect("nonempty atomic scope")]
            .consistency_set_id
            .as_ref()
            .expect("expanded atomic scope has a set");
        let quiesce_order = dependency_order(&self.consistency_sets[set_id])?;
        let resume_order = quiesce_order.iter().rev().cloned().collect();
        let phase = match request.mode {
            FailoverMode::Clean => FailoverPhase::Quiescing,
            FailoverMode::AcceptDeclaredLoss { .. } => FailoverPhase::AwaitingLossAcceptance,
        };
        self.operations.insert(
            request.operation_id.clone(),
            FailoverOperation {
                operation_id: request.operation_id,
                source_region: request.source_region,
                target_region: request.target_region,
                volume_ids: volumes,
                decision_revision: request.expected_revision,
                mode: request.mode,
                phase,
                quiesce_order,
                resume_order,
                checkpoint_id: None,
                cut: BTreeMap::new(),
                old_epochs,
                new_epochs: BTreeMap::new(),
                loss_record: None,
                blocked_reason: None,
                workload_action_ids: Vec::new(),
            },
        );
        Ok(self.bump_revision())
    }

    /// Reconcile one operation as far as current observations permit.
    pub fn reconcile(&mut self, operation_id: &str) -> io::Result<FailoverPhase> {
        let mut operation = self
            .operations
            .remove(operation_id)
            .ok_or_else(|| invalid("unknown failover operation"))?;
        let result = self.reconcile_inner(&mut operation);
        let phase = operation.phase;
        self.operations.insert(operation_id.to_string(), operation);
        result.map(|_| phase)
    }

    fn reconcile_inner(&mut self, operation: &mut FailoverOperation) -> io::Result<()> {
        loop {
            match operation.phase {
                FailoverPhase::Quiescing => {
                    if !operation.volume_ids.iter().all(|volume| {
                        self.progress(volume, &operation.source_region)
                            .is_some_and(|progress| progress.reachable && progress.quiesced)
                    }) {
                        return Ok(());
                    }
                    operation.phase = FailoverPhase::WaitingForReplica;
                }
                FailoverPhase::WaitingForReplica => {
                    let Some((checkpoint_id, cut)) = self.latest_eligible_checkpoint(
                        &operation.volume_ids,
                        &operation.source_region,
                        &operation.target_region,
                        true,
                        operation.decision_revision,
                        &operation.old_epochs,
                    )?
                    else {
                        return Ok(());
                    };
                    operation.checkpoint_id = Some(checkpoint_id);
                    operation.cut = cut;
                    operation.phase = FailoverPhase::FencingSource;
                }
                FailoverPhase::AwaitingLossAcceptance => {
                    if operation.volume_ids.iter().any(|volume| {
                        self.progress(volume, &operation.source_region)
                            .is_some_and(|progress| progress.reachable)
                    }) {
                        return Ok(());
                    }
                    let Some((checkpoint_id, cut)) = self.latest_eligible_checkpoint(
                        &operation.volume_ids,
                        &operation.source_region,
                        &operation.target_region,
                        false,
                        operation.decision_revision,
                        &operation.old_epochs,
                    )?
                    else {
                        operation.phase = FailoverPhase::Blocked;
                        operation.blocked_reason = Some(
                            "target has no application-consistent checkpoint covering the atomic scope"
                                .into(),
                        );
                        return Ok(());
                    };
                    let (reason, max_loss) = match &operation.mode {
                        FailoverMode::AcceptDeclaredLoss {
                            reason,
                            max_missing_operations_per_lane,
                        } => (reason.clone(), *max_missing_operations_per_lane),
                        FailoverMode::Clean => unreachable!(),
                    };
                    let losses = self.calculate_loss(operation, &cut, max_loss)?;
                    let record = LossRecord {
                        operation_id: operation.operation_id.clone(),
                        reason,
                        checkpoint_id: checkpoint_id.clone(),
                        losses,
                    };
                    self.loss_records.push(record.clone());
                    operation.loss_record = Some(record);
                    operation.checkpoint_id = Some(checkpoint_id);
                    operation.cut = cut;
                    operation.phase = FailoverPhase::FencingSource;
                }
                FailoverPhase::FencingSource => {
                    let declared_loss =
                        matches!(&operation.mode, FailoverMode::AcceptDeclaredLoss { .. });
                    for session in self.sessions.values_mut().filter(|session| {
                        operation.volume_ids.contains(&session.volume_id)
                            && session.state == SessionState::Active
                    }) {
                        if !declared_loss && session.supports_transparent_rebind {
                            session.observed_placement_epoch = operation.old_epochs
                                [&session.volume_id]
                                .checked_add(1)
                                .ok_or_else(|| invalid("placement epoch has no successor"))?;
                            session.fence_reason = None;
                        } else {
                            session.state = SessionState::Fenced;
                            session.fence_reason = Some(format!(
                                "global failover {} invalidated placement epoch {}",
                                operation.operation_id, session.observed_placement_epoch
                            ));
                        }
                    }
                    operation.phase = FailoverPhase::PromotingTarget;
                }
                FailoverPhase::PromotingTarget => {
                    for volume_id in &operation.volume_ids {
                        let volume = self.volumes.get_mut(volume_id).expect("validated volume");
                        volume.placement_epoch = volume
                            .placement_epoch
                            .checked_add(1)
                            .ok_or_else(|| invalid("placement epoch has no successor"))?;
                        volume.authority_region = operation.target_region.clone();
                        operation
                            .new_epochs
                            .insert(volume_id.clone(), volume.placement_epoch);
                    }
                    self.emit_workload_actions(operation);
                    operation.phase = FailoverPhase::ReconcilingWorkloads;
                    self.bump_revision();
                }
                FailoverPhase::ReconcilingWorkloads => {
                    if operation.workload_action_ids.iter().all(|id| {
                        self.workload_actions
                            .get(id)
                            .is_some_and(|action| action.acknowledged)
                    }) {
                        operation.phase = FailoverPhase::Completed;
                        self.bump_revision();
                    }
                    return Ok(());
                }
                FailoverPhase::Completed | FailoverPhase::Blocked => return Ok(()),
            }
        }
    }

    pub fn acknowledge_workload_action(
        &mut self,
        action_id: &str,
        adapter_id: &str,
    ) -> io::Result<()> {
        let action = self
            .workload_actions
            .get_mut(action_id)
            .ok_or_else(|| invalid("unknown workload action"))?;
        if action.adapter_id != adapter_id {
            return Err(invalid("workload action acknowledged by the wrong adapter"));
        }
        action.acknowledged = true;
        Ok(())
    }

    pub fn drifted_observations(&self) -> Vec<&RegionalVolumeProgress> {
        self.regional_progress
            .values()
            .flat_map(|regions| regions.values())
            .filter(|progress| progress.observed_revision < self.revision)
            .collect()
    }

    fn emit_workload_actions(&mut self, operation: &mut FailoverOperation) {
        for volume_id in &operation.volume_ids {
            for binding in &self.volumes[volume_id].workload_bindings {
                let action_id = format!(
                    "{}:{}:{}",
                    operation.operation_id, volume_id, binding.binding_id
                );
                let follow = binding.policy == WorkloadFailoverPolicy::FollowVolume;
                self.workload_actions.insert(
                    action_id.clone(),
                    WorkloadAction {
                        action_id: action_id.clone(),
                        operation_id: operation.operation_id.clone(),
                        volume_id: volume_id.clone(),
                        binding_id: binding.binding_id.clone(),
                        adapter_id: binding.adapter_id.clone(),
                        adapter_kind: binding.adapter_kind,
                        policy: binding.policy,
                        source_region: operation.source_region.clone(),
                        target_region: operation.target_region.clone(),
                        source_replicas: if follow { 0 } else { binding.source_replicas },
                        target_replicas: if follow {
                            binding.target_replicas.max(binding.source_replicas)
                        } else {
                            binding.target_replicas
                        },
                        add_source_taint: follow,
                        remove_target_taint: follow,
                        source_region_lost: matches!(
                            &operation.mode,
                            FailoverMode::AcceptDeclaredLoss { .. }
                        ),
                        acknowledged: binding.policy == WorkloadFailoverPolicy::ObserveOnly,
                    },
                );
                operation.workload_action_ids.push(action_id);
            }
        }
    }

    fn calculate_loss(
        &self,
        operation: &FailoverOperation,
        cut: &VolumeCuts,
        max_loss: u64,
    ) -> io::Result<Vec<DeclaredLaneLoss>> {
        let mut losses = Vec::new();
        for volume_id in &operation.volume_ids {
            let source = self
                .progress(volume_id, &operation.source_region)
                .ok_or_else(|| invalid(format!("missing last-known source HWM for {volume_id}")))?;
            for (lane, source_hwm) in &source.durable_lane_hwms {
                let target_hwm = cut
                    .get(volume_id)
                    .and_then(|lanes| lanes.get(lane))
                    .copied()
                    .ok_or_else(|| invalid("checkpoint omitted a source lane"))?;
                if target_hwm > *source_hwm {
                    return Err(invalid(
                        "target checkpoint is ahead of last-known source HWM",
                    ));
                }
                let missing = source_hwm - target_hwm;
                if missing > max_loss {
                    return Err(invalid(format!(
                        "declared loss limit exceeded volume={volume_id} lane={lane} missing={missing} limit={max_loss}"
                    )));
                }
                losses.push(DeclaredLaneLoss {
                    volume_id: volume_id.clone(),
                    lane: *lane,
                    last_known_source_hwm: *source_hwm,
                    accepted_target_hwm: target_hwm,
                    first_missing: (missing != 0).then_some(target_hwm + 1),
                    last_missing: (missing != 0).then_some(*source_hwm),
                });
            }
        }
        Ok(losses)
    }

    fn latest_eligible_checkpoint(
        &self,
        volumes: &BTreeSet<String>,
        source_region: &str,
        target_region: &str,
        require_source_cut: bool,
        decision_revision: u64,
        old_epochs: &BTreeMap<String, u64>,
    ) -> io::Result<Option<(String, VolumeCuts)>> {
        let set_ids: BTreeSet<&str> = volumes
            .iter()
            .filter_map(|volume| self.volumes[volume].consistency_set_id.as_deref())
            .collect();
        if set_ids.len() != 1 {
            return Err(invalid(
                "atomic failover scope must resolve to exactly one consistency set",
            ));
        }
        let set_id = *set_ids.iter().next().expect("one set");
        Ok(self
            .checkpoints
            .values()
            .filter(|checkpoint| {
                checkpoint.set_id == set_id
                    && checkpoint.application_consistent
                    && checkpoint.cuts.keys().collect::<BTreeSet<_>>() == volumes.iter().collect()
                    && volumes.iter().all(|volume| {
                        let target = self.progress(volume, target_region);
                        let source = self.progress(volume, source_region);
                        let checkpoint_cut = &checkpoint.cuts[volume];
                        target.is_some_and(|progress| {
                            progress.is_ha()
                                && progress.reachable
                                && progress.observed_revision == decision_revision
                                && progress.placement_epoch == old_epochs[volume]
                                && progress.durable_lane_hwms.keys().collect::<BTreeSet<_>>()
                                    == checkpoint_cut.keys().collect::<BTreeSet<_>>()
                                && cuts_cover(&progress.durable_lane_hwms, checkpoint_cut)
                        }) && source.is_some_and(|progress| {
                            progress.observed_revision == decision_revision
                                && progress.placement_epoch == old_epochs[volume]
                                && progress.durable_lane_hwms.keys().collect::<BTreeSet<_>>()
                                    == checkpoint_cut.keys().collect::<BTreeSet<_>>()
                                && (!require_source_cut
                                    || (progress.quiesced
                                        && cuts_cover(&progress.durable_lane_hwms, checkpoint_cut)
                                        && checkpoint_cut == &progress.durable_lane_hwms))
                        })
                    })
            })
            .max_by_key(|checkpoint| checkpoint.sequence)
            .map(|checkpoint| (checkpoint.checkpoint_id.clone(), checkpoint.cuts.clone())))
    }

    fn require_fresh_region_observation(
        &self,
        volume_id: &str,
        region: &str,
        decision_revision: u64,
    ) -> io::Result<&RegionalVolumeProgress> {
        let progress = self.progress(volume_id, region).ok_or_else(|| {
            invalid(format!(
                "missing regional progress for {volume_id} in {region}"
            ))
        })?;
        let epoch = self.volumes[volume_id].placement_epoch;
        if progress.observed_revision != decision_revision || progress.placement_epoch != epoch {
            return Err(invalid(format!(
                "stale regional progress for {volume_id} in {region}: observed_revision={} decision_revision={decision_revision} observed_epoch={} custody_epoch={epoch}",
                progress.observed_revision, progress.placement_epoch
            )));
        }
        Ok(progress)
    }

    fn require_region_ha(
        &self,
        volume_id: &str,
        region: &str,
        decision_revision: u64,
    ) -> io::Result<()> {
        let progress =
            self.require_fresh_region_observation(volume_id, region, decision_revision)?;
        if !progress.is_ha() {
            return Err(invalid(format!(
                "region {region} is not HA for {volume_id}: configured={} quorum={} live={} failure_domains={}",
                progress.configured_replicas,
                progress.required_quorum,
                progress.live_replicas,
                progress.durable_failure_domains.len()
            )));
        }
        Ok(())
    }

    fn progress(&self, volume: &str, region: &str) -> Option<&RegionalVolumeProgress> {
        self.regional_progress
            .get(volume)
            .and_then(|regions| regions.get(region))
    }

    fn expand_atomic_scope(&self, requested: &BTreeSet<String>) -> io::Result<BTreeSet<String>> {
        if requested.is_empty() {
            return Err(invalid("failover request must name a volume"));
        }
        let mut expanded = BTreeSet::new();
        for volume_id in requested {
            let volume = self
                .volumes
                .get(volume_id)
                .ok_or_else(|| invalid(format!("unknown failover volume {volume_id}")))?;
            let set_id = volume.consistency_set_id.as_ref().ok_or_else(|| {
                invalid(format!(
                    "volume {volume_id} lacks an atomic consistency set"
                ))
            })?;
            expanded.extend(self.consistency_sets[set_id].members.iter().cloned());
        }
        let set_ids: BTreeSet<&str> = expanded
            .iter()
            .filter_map(|volume| self.volumes[volume].consistency_set_id.as_deref())
            .collect();
        if set_ids.len() != 1 {
            return Err(invalid(
                "request spans independently committed consistency sets",
            ));
        }
        Ok(expanded)
    }

    fn require_revision(&self, expected: u64) -> io::Result<()> {
        if expected != self.revision {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "revision conflict: expected {expected}, current {}",
                    self.revision
                ),
            ));
        }
        Ok(())
    }

    fn bump_revision(&mut self) -> u64 {
        self.revision = self.revision.saturating_add(1);
        self.revision
    }
}

fn cuts_cover(available: &LaneCuts, required: &LaneCuts) -> bool {
    required.iter().all(|(lane, hwm)| {
        available
            .get(lane)
            .is_some_and(|available| available >= hwm)
    })
}

mod lane_cuts_json {
    use super::*;
    use serde::de::Error;

    pub fn serialize<S>(cuts: &LaneCuts, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        cuts.iter()
            .map(|(lane, hwm)| (lane.to_string(), *hwm))
            .collect::<BTreeMap<_, _>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<LaneCuts, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = BTreeMap::<String, u64>::deserialize(deserializer)?;
        let mut cuts = LaneCuts::new();
        for (key, hwm) in encoded {
            let lane = key.parse::<u32>().map_err(|_| {
                D::Error::custom(format!("lane key {key:?} is not a canonical u32"))
            })?;
            if lane.to_string() != key || cuts.insert(lane, hwm).is_some() {
                return Err(D::Error::custom(format!(
                    "lane key {key:?} is duplicate or non-canonical"
                )));
            }
        }
        Ok(cuts)
    }
}

mod volume_cuts_json {
    use super::*;

    pub fn serialize<S>(cuts: &VolumeCuts, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = cuts
            .iter()
            .map(|(volume, lanes)| {
                (
                    volume.clone(),
                    lanes
                        .iter()
                        .map(|(lane, hwm)| (lane.to_string(), *hwm))
                        .collect::<BTreeMap<_, _>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        encoded.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<VolumeCuts, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;
        let encoded = BTreeMap::<String, BTreeMap<String, u64>>::deserialize(deserializer)?;
        let mut cuts = VolumeCuts::new();
        for (volume, encoded_lanes) in encoded {
            let mut lanes = LaneCuts::new();
            for (key, hwm) in encoded_lanes {
                let lane = key.parse::<u32>().map_err(|_| {
                    D::Error::custom(format!("lane key {key:?} is not a canonical u32"))
                })?;
                if lane.to_string() != key || lanes.insert(lane, hwm).is_some() {
                    return Err(D::Error::custom(format!(
                        "lane key {key:?} is duplicate or non-canonical"
                    )));
                }
            }
            cuts.insert(volume, lanes);
        }
        Ok(cuts)
    }
}

fn dependency_order(spec: &ConsistencySetSpec) -> io::Result<Vec<String>> {
    let mut outgoing: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut indegree: BTreeMap<&str, usize> =
        spec.members.iter().map(|id| (id.as_str(), 0)).collect();
    let mut unique_edges = BTreeSet::new();
    for edge in &spec.dependencies {
        if edge.before == edge.after
            || !spec.members.contains(&edge.before)
            || !spec.members.contains(&edge.after)
        {
            return Err(invalid("dependency must connect two different set members"));
        }
        if !unique_edges.insert((edge.before.as_str(), edge.after.as_str())) {
            return Err(invalid("duplicate consistency dependency"));
        }
        outgoing
            .entry(edge.before.as_str())
            .or_default()
            .push(edge.after.as_str());
        *indegree.get_mut(edge.after.as_str()).expect("member") += 1;
    }
    let mut ready: BTreeSet<&str> = indegree
        .iter()
        .filter_map(|(id, degree)| (*degree == 0).then_some(*id))
        .collect();
    let mut order = Vec::with_capacity(spec.members.len());
    while let Some(id) = ready.pop_first() {
        order.push(id.to_string());
        for target in outgoing.get(id).into_iter().flatten() {
            let degree = indegree.get_mut(target).expect("member");
            *degree -= 1;
            if *degree == 0 {
                ready.insert(target);
            }
        }
    }
    if order.len() != spec.members.len() {
        return Err(invalid(
            "dependency graph is interlocked; quiesce/restore ordering has no valid solution",
        ));
    }
    Ok(order)
}

fn validate_id(value: &str, field: &str) -> io::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        return Err(invalid(format!("invalid {field}")));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(id: &str, policy: WorkloadFailoverPolicy, kind: AdapterKind) -> VolumeSpec {
        VolumeSpec {
            volume_id: id.into(),
            authority_region: "us".into(),
            placement_epoch: 7,
            consistency_set_id: None,
            workload_bindings: vec![WorkloadBinding {
                binding_id: format!("{id}-workload"),
                adapter_id: match kind {
                    AdapterKind::Kubernetes => "kube-us-eu".into(),
                    AdapterKind::External => "systemd-fleet".into(),
                },
                adapter_kind: kind,
                policy,
                source_replicas: 3,
                target_replicas: 0,
            }],
        }
    }

    fn progress(
        volume: &str,
        region: &str,
        hwm: u64,
        reachable: bool,
        quiesced: bool,
    ) -> RegionalVolumeProgress {
        RegionalVolumeProgress {
            volume_id: volume.into(),
            region: region.into(),
            observed_revision: 4,
            placement_epoch: 7,
            durable_lane_hwms: BTreeMap::from([(0, hwm)]),
            applied_lane_hwms: BTreeMap::from([(0, hwm)]),
            configured_replicas: 3,
            required_quorum: 2,
            live_replicas: 3,
            durable_failure_domains: BTreeSet::from(["az-a".into(), "az-b".into(), "az-c".into()]),
            reachable,
            quiesced,
        }
    }

    fn configured() -> FailoverState {
        let mut state = FailoverState::default();
        state
            .put_volume(
                0,
                volume(
                    "postgres",
                    WorkloadFailoverPolicy::FollowVolume,
                    AdapterKind::Kubernetes,
                ),
            )
            .unwrap();
        state
            .put_volume(
                1,
                volume("kafka", WorkloadFailoverPolicy::Stay, AdapterKind::External),
            )
            .unwrap();
        state
            .put_volume(
                2,
                volume(
                    "cassandra",
                    WorkloadFailoverPolicy::ObserveOnly,
                    AdapterKind::Kubernetes,
                ),
            )
            .unwrap();
        state
            .put_consistency_set(
                3,
                ConsistencySetSpec {
                    set_id: "app-stack".into(),
                    members: BTreeSet::from([
                        "postgres".into(),
                        "kafka".into(),
                        "cassandra".into(),
                    ]),
                    dependencies: vec![
                        CutDependency {
                            before: "kafka".into(),
                            after: "postgres".into(),
                        },
                        CutDependency {
                            before: "postgres".into(),
                            after: "cassandra".into(),
                        },
                    ],
                },
            )
            .unwrap();
        state
    }

    #[test]
    fn rejects_interlocked_dependency_and_overlapping_atomic_sets() {
        let mut state = FailoverState::default();
        state
            .put_volume(
                0,
                volume("a", WorkloadFailoverPolicy::Stay, AdapterKind::External),
            )
            .unwrap();
        state
            .put_volume(
                1,
                volume("b", WorkloadFailoverPolicy::Stay, AdapterKind::External),
            )
            .unwrap();
        assert!(
            state
                .put_consistency_set(
                    2,
                    ConsistencySetSpec {
                        set_id: "cycle".into(),
                        members: BTreeSet::from(["a".into(), "b".into()]),
                        dependencies: vec![
                            CutDependency {
                                before: "a".into(),
                                after: "b".into()
                            },
                            CutDependency {
                                before: "b".into(),
                                after: "a".into()
                            },
                        ],
                    }
                )
                .is_err()
        );

        state
            .put_consistency_set(
                2,
                ConsistencySetSpec {
                    set_id: "set-a".into(),
                    members: BTreeSet::from(["a".into()]),
                    dependencies: Vec::new(),
                },
            )
            .unwrap();
        assert!(
            state
                .put_consistency_set(
                    3,
                    ConsistencySetSpec {
                        set_id: "overlap".into(),
                        members: BTreeSet::from(["a".into(), "b".into()]),
                        dependencies: Vec::new(),
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn custody_membership_and_observation_freshness_cannot_be_bypassed() {
        let mut state = FailoverState::default();
        state
            .put_volume(
                0,
                volume("a", WorkloadFailoverPolicy::Stay, AdapterKind::External),
            )
            .unwrap();
        state
            .put_volume(
                1,
                volume("b", WorkloadFailoverPolicy::Stay, AdapterKind::External),
            )
            .unwrap();

        let mut injected_membership =
            volume("c", WorkloadFailoverPolicy::Stay, AdapterKind::External);
        injected_membership.consistency_set_id = Some("set-ab".into());
        assert!(state.put_volume(2, injected_membership).is_err());

        state
            .put_consistency_set(
                2,
                ConsistencySetSpec {
                    set_id: "set-ab".into(),
                    members: BTreeSet::from(["a".into(), "b".into()]),
                    dependencies: Vec::new(),
                },
            )
            .unwrap();

        let mut custody_bypass = state.volumes["a"].clone();
        custody_bypass.authority_region = "eu".into();
        custody_bypass.placement_epoch += 1;
        assert!(state.put_volume(3, custody_bypass).is_err());

        state
            .publish_checkpoint(ConsistencyCheckpoint {
                checkpoint_id: "set-ab-1".into(),
                set_id: "set-ab".into(),
                sequence: 1,
                cuts: BTreeMap::from([
                    ("a".into(), BTreeMap::from([(0, 1)])),
                    ("b".into(), BTreeMap::from([(0, 1)])),
                ]),
                application_consistent: true,
            })
            .unwrap();
        assert!(
            state
                .put_consistency_set(
                    3,
                    ConsistencySetSpec {
                        set_id: "set-ab".into(),
                        members: BTreeSet::from(["a".into(), "b".into()]),
                        dependencies: vec![CutDependency {
                            before: "a".into(),
                            after: "b".into(),
                        }],
                    },
                )
                .is_err()
        );

        for region in ["us", "eu"] {
            let mut stale = progress("a", region, 1, true, region == "us");
            stale.observed_revision = 2;
            state.observe_region(stale).unwrap();
        }
        let error = state
            .request_failover(FailoverRequest {
                operation_id: "stale-observation".into(),
                requested_volume_ids: BTreeSet::from(["a".into()]),
                source_region: "us".into(),
                target_region: "eu".into(),
                expected_revision: 3,
                mode: FailoverMode::Clean,
            })
            .unwrap_err();
        assert!(error.to_string().contains("stale regional progress"));
    }

    #[test]
    fn clean_group_failover_rebinds_capable_sessions_and_fences_legacy_frontends() {
        let mut state = configured();
        for volume in ["postgres", "kafka", "cassandra"] {
            state
                .observe_region(progress(volume, "us", 100, true, true))
                .unwrap();
            state
                .observe_region(progress(volume, "eu", 100, true, false))
                .unwrap();
        }
        state
            .publish_checkpoint(ConsistencyCheckpoint {
                checkpoint_id: "clean-100".into(),
                set_id: "app-stack".into(),
                sequence: 100,
                cuts: ["postgres", "kafka", "cassandra"]
                    .into_iter()
                    .map(|v| (v.into(), BTreeMap::from([(0, 100)])))
                    .collect(),
                application_consistent: true,
            })
            .unwrap();
        for (id, volume, frontend) in [
            ("s1", "postgres", "csi"),
            ("s2", "kafka", "libvirt"),
            ("s3", "cassandra", "linux-block"),
        ] {
            state
                .register_session(ClientSession {
                    session_id: id.into(),
                    volume_id: volume.into(),
                    frontend: frontend.into(),
                    region: "us".into(),
                    observed_placement_epoch: 7,
                    supports_transparent_rebind: frontend == "csi",
                    state: SessionState::Active,
                    fence_reason: None,
                })
                .unwrap();
        }
        let revision = state.revision;
        state
            .request_failover(FailoverRequest {
                operation_id: "clean-cut".into(),
                requested_volume_ids: BTreeSet::from(["postgres".into()]),
                source_region: "us".into(),
                target_region: "eu".into(),
                expected_revision: revision,
                mode: FailoverMode::Clean,
            })
            .unwrap();
        assert_eq!(
            state.reconcile("clean-cut").unwrap(),
            FailoverPhase::ReconcilingWorkloads
        );
        assert_eq!(
            state.operations["clean-cut"].quiesce_order,
            ["kafka", "postgres", "cassandra"]
        );
        assert_eq!(
            state.operations["clean-cut"].resume_order,
            ["cassandra", "postgres", "kafka"]
        );
        assert_eq!(state.sessions["s1"].state, SessionState::Active);
        assert_eq!(state.sessions["s1"].observed_placement_epoch, 8);
        assert!(state.sessions["s1"].fence_reason.is_none());
        assert_eq!(state.sessions["s2"].state, SessionState::Fenced);
        assert_eq!(state.sessions["s3"].state, SessionState::Fenced);
        assert!(
            state
                .volumes
                .values()
                .all(|volume| volume.authority_region == "eu" && volume.placement_epoch == 8)
        );
        let follow = state
            .workload_actions
            .values()
            .find(|a| a.volume_id == "postgres")
            .unwrap();
        assert_eq!(
            (
                follow.source_replicas,
                follow.target_replicas,
                follow.add_source_taint,
                follow.source_region_lost,
            ),
            (0, 3, true, false)
        );
        let stay = state
            .workload_actions
            .values()
            .find(|a| a.volume_id == "kafka")
            .unwrap();
        assert_eq!(
            (
                stay.source_replicas,
                stay.target_replicas,
                stay.add_source_taint
            ),
            (3, 0, false)
        );
        let pending: Vec<(String, String)> = state
            .workload_actions
            .values()
            .filter(|a| !a.acknowledged)
            .map(|a| (a.action_id.clone(), a.adapter_id.clone()))
            .collect();
        for (action, adapter) in pending {
            state
                .acknowledge_workload_action(&action, &adapter)
                .unwrap();
        }
        assert_eq!(
            state.reconcile("clean-cut").unwrap(),
            FailoverPhase::Completed
        );
    }

    #[test]
    fn disaster_cut_records_exact_booked_loss_at_latest_common_checkpoint() {
        let mut state = configured();
        for volume in ["postgres", "kafka", "cassandra"] {
            state
                .observe_region(progress(volume, "us", 120, false, false))
                .unwrap();
            state
                .observe_region(progress(volume, "eu", 100, true, false))
                .unwrap();
        }
        state
            .publish_checkpoint(ConsistencyCheckpoint {
                checkpoint_id: "remote-100".into(),
                set_id: "app-stack".into(),
                sequence: 100,
                cuts: ["postgres", "kafka", "cassandra"]
                    .into_iter()
                    .map(|v| (v.into(), BTreeMap::from([(0, 100)])))
                    .collect(),
                application_consistent: true,
            })
            .unwrap();
        let revision = state.revision;
        state
            .request_failover(FailoverRequest {
                operation_id: "godzilla".into(),
                requested_volume_ids: BTreeSet::from(["kafka".into()]),
                source_region: "us".into(),
                target_region: "eu".into(),
                expected_revision: revision,
                mode: FailoverMode::AcceptDeclaredLoss {
                    reason: "us region lost; last 200ms unavailable".into(),
                    max_missing_operations_per_lane: 20,
                },
            })
            .unwrap();
        assert_eq!(
            state.reconcile("godzilla").unwrap(),
            FailoverPhase::ReconcilingWorkloads
        );
        let loss = state.operations["godzilla"].loss_record.as_ref().unwrap();
        assert_eq!(loss.losses.len(), 3);
        assert!(
            loss.losses
                .iter()
                .all(|lane| lane.first_missing == Some(101) && lane.last_missing == Some(120))
        );
        assert!(
            state
                .workload_actions
                .values()
                .all(|action| action.source_region_lost)
        );
    }

    #[test]
    fn skewed_app_progress_selects_latest_checkpoint_common_to_every_volume() {
        let mut state = configured();
        for volume in ["postgres", "kafka", "cassandra"] {
            state
                .observe_region(progress(volume, "us", 120, false, false))
                .unwrap();
        }
        for (volume, hwm) in [("postgres", 110), ("kafka", 95), ("cassandra", 105)] {
            state
                .observe_region(progress(volume, "eu", hwm, true, false))
                .unwrap();
        }
        for checkpoint in [80, 100] {
            state
                .publish_checkpoint(ConsistencyCheckpoint {
                    checkpoint_id: format!("app-cut-{checkpoint}"),
                    set_id: "app-stack".into(),
                    sequence: checkpoint,
                    cuts: ["postgres", "kafka", "cassandra"]
                        .into_iter()
                        .map(|volume| (volume.into(), BTreeMap::from([(0, checkpoint)])))
                        .collect(),
                    application_consistent: true,
                })
                .unwrap();
        }
        state
            .request_failover(FailoverRequest {
                operation_id: "skewed-common-cut".into(),
                requested_volume_ids: BTreeSet::from(["postgres".into()]),
                source_region: "us".into(),
                target_region: "eu".into(),
                expected_revision: 4,
                mode: FailoverMode::AcceptDeclaredLoss {
                    reason: "source region destroyed".into(),
                    max_missing_operations_per_lane: 40,
                },
            })
            .unwrap();
        assert_eq!(
            state.reconcile("skewed-common-cut").unwrap(),
            FailoverPhase::ReconcilingWorkloads
        );
        let operation = &state.operations["skewed-common-cut"];
        assert_eq!(operation.checkpoint_id.as_deref(), Some("app-cut-80"));
        assert!(
            operation
                .cut
                .values()
                .all(|lanes| lanes == &BTreeMap::from([(0, 80)]))
        );
        assert_eq!(operation.loss_record.as_ref().unwrap().losses.len(), 3);
    }

    #[test]
    fn rejects_incomplete_lane_observations_and_checkpoint_id_reuse() {
        let mut state = configured();
        let mut incomplete = progress("postgres", "us", 10, true, true);
        incomplete.durable_lane_hwms.insert(1, 10);
        assert!(state.observe_region(incomplete).is_err());

        let checkpoint = ConsistencyCheckpoint {
            checkpoint_id: "immutable-cut".into(),
            set_id: "app-stack".into(),
            sequence: 1,
            cuts: ["postgres", "kafka", "cassandra"]
                .into_iter()
                .map(|volume| (volume.into(), BTreeMap::from([(0, 1)])))
                .collect(),
            application_consistent: true,
        };
        state.publish_checkpoint(checkpoint.clone()).unwrap();
        let mut changed = checkpoint;
        changed.sequence = 2;
        assert!(state.publish_checkpoint(changed).is_err());
    }

    #[test]
    fn refuses_non_ha_target_and_reports_observation_drift() {
        let mut state = configured();
        for volume in ["postgres", "kafka", "cassandra"] {
            state
                .observe_region(progress(volume, "us", 10, true, true))
                .unwrap();
            let mut target = progress(volume, "eu", 10, true, false);
            target.configured_replicas = 2;
            target.required_quorum = 1;
            target.live_replicas = 2;
            target.durable_failure_domains = BTreeSet::from(["one-rack".into()]);
            state.observe_region(target).unwrap();
        }
        let revision = state.revision;
        let error = state
            .request_failover(FailoverRequest {
                operation_id: "unsafe".into(),
                requested_volume_ids: BTreeSet::from(["postgres".into()]),
                source_region: "us".into(),
                target_region: "eu".into(),
                expected_revision: revision,
                mode: FailoverMode::Clean,
            })
            .unwrap_err();
        assert!(error.to_string().contains("is not HA"));
        state
            .put_volume(
                revision,
                volume(
                    "new-volume",
                    WorkloadFailoverPolicy::Stay,
                    AdapterKind::External,
                ),
            )
            .unwrap();
        assert_eq!(state.drifted_observations().len(), 6);
        assert!(
            state
                .put_volume(
                    0,
                    volume("stale", WorkloadFailoverPolicy::Stay, AdapterKind::External)
                )
                .is_err()
        );
    }

    #[test]
    fn json_round_trip_preserves_nested_regions_and_numeric_lane_keys() {
        let mut state = configured();
        state
            .observe_region(progress("postgres", "us", 42, true, true))
            .unwrap();
        let encoded = serde_json::to_vec(&state).unwrap();
        let decoded: FailoverState = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, state);
        assert_eq!(
            decoded.regional_progress["postgres"]["us"].durable_lane_hwms[&0],
            42
        );

        let command: GlobalFailoverCommand = serde_json::from_str(
            r#"{"command":"observe_region","progress":{"volume_id":"postgres","region":"us","observed_revision":4,"placement_epoch":7,"durable_lane_hwms":{"0":42},"applied_lane_hwms":{"0":42},"configured_replicas":3,"required_quorum":2,"live_replicas":3,"durable_failure_domains":["a","b"],"reachable":true,"quiesced":true}}"#,
        )
        .unwrap();
        assert!(matches!(
            command,
            GlobalFailoverCommand::ObserveRegion { .. }
        ));
    }
}
