//! Deterministic, event-reduced gang scheduling for a multi-region volume estate.
//!
//! The scheduler never reads a clock, starts a thread, performs I/O, or handles
//! individual data operations.  Timestamped events reduce authoritative estate
//! state; explicit planning events produce idempotent prepare/commit gangs for
//! parallel workers.  The same core is therefore usable by production adapters,
//! trace replay, and a virtual-time modeled executor.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io;

pub const GANG_SCHEDULER_SCHEMA_VERSION: u16 = 1;
const MAX_VOLUMES_PER_APPLICATION: usize = 16_384;
const MAX_SNAPSHOT_MEMBERS: usize = 16_384;
const MAX_DATABASE_VOLUME_MEMBERS: usize = 16_384;
const MAX_DATABASE_CONSUMERS: usize = 16_384;
const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// Versioned design envelope, not a claim about measured federal inventory.
/// The derivation and public source anchors live in
/// `docs/lane-flow-scheduler.md`. Counts include dev, staging, production, and
/// developer-sandbox deployments. Object services are applications whose
/// backing pools are ordinary managed volumes; individual objects are outside
/// the scheduler's state model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EstateScaleEnvelope {
    pub business_entities: u64,
    pub administrative_regions: u64,
    pub failure_domain_sites: u64,
    pub storage_hosts: u64,
    pub logical_applications: u64,
    pub application_environments: u64,
    pub managed_volumes: u64,
    pub logical_databases: u64,
    pub database_volume_memberships: u64,
    /// All authoritative graph edges in one logical relationship directory.
    /// Database-to-volume memberships are a subset, not an additional count.
    pub relationship_edges: u64,
    pub logical_bytes: u128,
    pub physical_bytes_with_resilience: u128,
}

pub const FEDERAL_REPATRIATION_4X_DESIGN_ENVELOPE_V1: EstateScaleEnvelope = EstateScaleEnvelope {
    business_entities: 128,
    administrative_regions: 512,
    failure_domain_sites: 1_024,
    storage_hosts: 3_500_000,
    logical_applications: 400_000,
    application_environments: 2_800_000,
    managed_volumes: 12_000_000,
    logical_databases: 8_000_000,
    database_volume_memberships: 24_000_000,
    relationship_edges: 50_000_000,
    logical_bytes: 100_000_000_000_000_000_000,
    physical_bytes_with_resilience: 350_000_000_000_000_000_000,
};

/// Selects how independent work is admitted.  Every externally visible state
/// change still uses the same prepare/commit primitive; "arena" means narrow,
/// independently abortable admission units rather than weaker consistency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrentSchedulingStrategy {
    /// Co-schedule every currently unplaced volume in an application.  This
    /// minimizes control-plane commits, but one failed worker aborts the wave.
    Gang,
    /// Admit only independent single-volume work.  Requests that explicitly
    /// require a multi-volume atomic boundary are safely deferred.
    Arena,
    /// Admit independent work through narrow arenas and retain gangs wherever
    /// recovery or consistency semantics require an atomic boundary.
    Hybrid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GangSchedulerConfig {
    pub strategy: ConcurrentSchedulingStrategy,
}

impl Default for GangSchedulerConfig {
    fn default() -> Self {
        Self {
            strategy: ConcurrentSchedulingStrategy::Hybrid,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationKind {
    CassandraJbod,
    CockroachDb,
    Postgres,
    PerformanceShardedDatabase,
    Minio,
    Kafka,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityOwner {
    Application,
    Storage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationRequirement {
    Shared,
    Reserved,
    Dedicated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureScenario {
    HostLoss,
    AvailabilityZoneLoss,
    RegionLoss,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FailoverAction {
    RestoreTo { region_id: String },
    HoldDurably,
}

/// A more-specific rule (more unavailable regions) supersedes a less-specific
/// rule.  Equal-specificity rules that resolve to different actions are
/// treated as ambiguous and no failover is attempted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreapprovedFailoverRule {
    pub unavailable_regions: BTreeSet<String>,
    pub action: FailoverAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryObjective {
    /// Maximum accepted difference between the volume source HWM and the
    /// durable HWM available in a recovery region.
    pub rpo_max_missing_operations: u64,
    /// None means this policy makes no recovery-time guarantee for the
    /// corresponding scenarios.  It is not silently converted to failover.
    pub rto_ns: Option<u64>,
    pub scenarios: BTreeSet<FailureScenario>,
    pub minimum_recovery_iops: u64,
    /// Empty means every policy-compatible region is eligible.
    pub allowed_regions: BTreeSet<String>,
    /// Optional ordered-by-specificity disaster policy. Empty preserves
    /// dynamic target selection from `allowed_regions`.
    pub preapproved_failover: Vec<PreapprovedFailoverRule>,
}

/// Fixed-point estimates in one estate-wide, current-value micro-unit.  They
/// are deliberately business quantities rather than an arbitrary priority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusinessImpactEstimate {
    pub downtime_cost_microunits_per_second: u64,
    pub rto_breach_cost_microunits: u64,
    pub lost_operation_cost_microunits: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioBusinessImpactRule {
    pub unavailable_regions: BTreeSet<String>,
    pub rto_ns: u64,
    pub impact: BusinessImpactEstimate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MuxBacklogLayer {
    pub path_id: String,
    pub role: FlowLaneRole,
    pub transport: FlowTransport,
    pub fixed_fill_latency_ns: u64,
    pub demux_bytes_per_second: u64,
    pub demux_operations_per_second: u64,
    /// Zero means payload ownership can be handed to the next stage without a
    /// CPU copy. Nonzero values are charged against `copy_bytes_per_second`.
    pub copy_passes: u8,
    pub copy_bytes_per_second: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowTransport {
    SharedArena,
    Tcp,
    Rdma,
    TerminalUserspaceIo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowLaneRole {
    Mux,
    InterRegionTransport,
    Demux,
    TerminalLeaf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowLaneAssignment {
    pub lane_id: String,
    pub role: FlowLaneRole,
    pub transport: FlowTransport,
    pub copy_passes: u8,
}

/// A flow is the end-to-end scheduling obligation. Lanes are the ordered
/// contention domains assigned to that flow; they are not interchangeable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledFlow {
    pub flow_id: String,
    pub volume_id: String,
    pub guaranteed_iops: u64,
    pub lanes: Vec<FlowLaneAssignment>,
}

/// Durable, still-multiplexed recovery work visible from one prospective
/// destination.  `bytes_pending` and `operations_pending` may fall as replay
/// advances, but its durable HWM may never regress.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableMuxBacklogObservation {
    pub volume_id: String,
    pub recovery_region_id: String,
    pub durable_hwm: u64,
    pub applied_hwm: u64,
    pub bytes_pending: u64,
    pub operations_pending: u64,
    pub layers: Vec<MuxBacklogLayer>,
    pub observed_at_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSpec {
    pub volume_id: String,
    pub home_region_id: String,
    pub bytes: u64,
    pub provisioned_iops: u64,
    pub latest_hwm: u64,
    pub durability_owner: DurabilityOwner,
    /// Copies are placed by this userspace scheduler on distinct host failure
    /// domains.  Application-owned JBOD volumes normally request one.
    pub storage_copies: u8,
    pub isolation: IsolationRequirement,
    /// Members with the same non-empty group move as one recovery gang.
    pub recovery_group_id: Option<String>,
    pub recovery: RecoveryObjective,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationSpec {
    pub application_id: String,
    pub business_entity_id: String,
    pub kind: ApplicationKind,
    pub business_impact: BusinessImpactEstimate,
    pub scenario_impacts: Vec<ScenarioBusinessImpactRule>,
    pub volumes: Vec<VolumeSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusinessEntitySpec {
    pub business_entity_id: String,
    pub generation: u64,
    pub region_ids: BTreeSet<String>,
}

/// A logical database hosted by a database-cluster application. Logical
/// databases are deliberately distinct from the physical volumes carrying
/// their ranges, WAL, and indexes: a large cluster can host thousands of
/// databases on tens of volumes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalDatabaseSpec {
    pub database_id: String,
    pub generation: u64,
    pub cluster_application_id: String,
    /// Physical cluster volumes whose loss or recovery can affect this
    /// database. This can be the whole cluster or a known shard subset.
    pub volume_ids: BTreeSet<String>,
    /// Applications whose business impact is exposed by loss of this
    /// database. Impact aggregation deduplicates applications even when they
    /// consume several databases on the same physical volume.
    pub consumer_application_ids: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalVolumeImpact {
    pub volume_id: String,
    pub owning_application_id: String,
    pub logical_database_count: u64,
    pub affected_application_ids: BTreeSet<String>,
    pub aggregate_impact_floor_microunits: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionSpec {
    pub region_id: String,
    pub trust_domain: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSpec {
    pub host_id: String,
    pub region_id: String,
    pub failure_domain: String,
    pub capacity_bytes: u64,
    pub lanes: u16,
    pub lane_iops: u64,
    pub restore_bytes_per_second: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaObservation {
    pub volume_id: String,
    pub region_id: String,
    pub durable_hwm: u64,
    pub applied_hwm: u64,
    pub observed_at_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemandBucket {
    pub application_id: String,
    pub interval_start_ns: u64,
    pub interval_end_ns: u64,
    pub demanded_iops: u64,
    pub queued_operations: u64,
    pub p995_latency_ns: u64,
    pub exact: bool,
    pub samples: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotScope {
    SameRegion,
    CrossRegion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotIntent {
    pub snapshot_id: String,
    pub volume_ids: BTreeSet<String>,
    /// Close the requested seed set over every overlapping durable
    /// consistency relationship before admission.
    pub expand_consistency_relationships: bool,
    pub scope: SnapshotScope,
    pub application_consistent: bool,
    pub deadline_ns: u64,
    pub maximum_hitch_ns: u64,
    /// Temporary read/copy budget charged at every participating source lane.
    pub operation_iops_per_volume: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeIntent {
    pub operation_id: String,
    pub volume_id: String,
    pub new_bytes: u64,
    pub deadline_ns: u64,
}

/// A durable relationship-graph edge. Overlapping groups form a transitive
/// consistency component when snapshot expansion is requested.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsistencyGroupSpec {
    pub group_id: String,
    pub generation: u64,
    pub volume_ids: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EstateEvent {
    PutRegion {
        spec: RegionSpec,
    },
    PutHost {
        spec: HostSpec,
    },
    PutApplication {
        spec: ApplicationSpec,
    },
    PutBusinessEntity {
        spec: BusinessEntitySpec,
    },
    PutLogicalDatabase {
        spec: LogicalDatabaseSpec,
    },
    PutConsistencyGroup {
        spec: ConsistencyGroupSpec,
    },
    SetRegionOnline {
        region_id: String,
        online: bool,
    },
    SetHostOnline {
        host_id: String,
        online: bool,
    },
    ObserveReplica {
        observation: ReplicaObservation,
    },
    ObserveDurableMuxBacklog {
        observation: DurableMuxBacklogObservation,
    },
    ObserveDemand {
        bucket: DemandBucket,
    },
    SetApplicationBusinessImpact {
        application_id: String,
        estimate: BusinessImpactEstimate,
    },
    SetApplicationScenarioImpacts {
        application_id: String,
        rules: Vec<ScenarioBusinessImpactRule>,
    },
    RequestSnapshot {
        intent: SnapshotIntent,
    },
    RequestResize {
        intent: ResizeIntent,
    },
    /// The caller chooses the coherent input watermark.  The scheduler never
    /// plans merely because wall-clock time passed.
    PlanAtWatermark {
        input_watermark: u64,
    },
    GangPrepared {
        plan_id: String,
        task_ids: BTreeSet<String>,
    },
    GangRejected {
        plan_id: String,
        worker_id: String,
        reason: String,
    },
    GangCommitted {
        plan_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EstateEventEnvelope {
    pub schema_version: u16,
    pub index: u64,
    pub timestamp_ns: u64,
    pub event: EstateEvent,
}

impl EstateEventEnvelope {
    pub fn new(index: u64, timestamp_ns: u64, event: EstateEvent) -> Self {
        Self {
            schema_version: GANG_SCHEDULER_SCHEMA_VERSION,
            index,
            timestamp_ns,
            event,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementLeg {
    pub host_id: String,
    pub lane: u16,
    pub failure_domain: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumePlacement {
    pub volume_id: String,
    pub region_id: String,
    pub placement_epoch: u64,
    pub legs: Vec<PlacementLeg>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationLifetime {
    Persistent,
    Gang,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceReservation {
    pub volume_id: String,
    pub host_id: String,
    pub lane: u16,
    pub iops: u64,
    pub bytes: u64,
    pub isolation: IsolationRequirement,
    pub lifetime: ReservationLifetime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GangTaskKind {
    Reserve,
    PrepareRoute,
    Restore,
    Quiesce,
    CaptureCut,
    PublishManifest,
    Resize,
    FenceSource,
    Activate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GangTask {
    pub task_id: String,
    pub worker_id: String,
    pub kind: GangTaskKind,
    pub depends_on: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GangKind {
    InitialPlacement,
    RegionalFailover,
    ConsistencySnapshot,
    VolumeResize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryTiming {
    pub target_region_id: String,
    pub target_queue_ready_ns: u64,
    pub mux_queue_ready_ns: u64,
    pub queue_start_ns: u64,
    pub pipeline_fill_ns: u64,
    pub demux_transfer_ns: u64,
    pub target_materialization_ns: u64,
    pub estimated_completion_ns: u64,
    pub deadline_ns: u64,
    pub rto_met: bool,
    pub mux_path_ids: BTreeSet<String>,
    pub transports: BTreeSet<FlowTransport>,
    pub zero_copy_lane_count: u64,
    pub copying_lane_count: u64,
    pub estimated_payload_bytes_copied: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub snapshot_id: String,
    pub scope: SnapshotScope,
    pub application_consistent: bool,
    pub cuts: BTreeMap<String, u64>,
    pub regions: BTreeSet<String>,
    pub committed_at_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum GangEffect {
    Place {
        placements: Vec<VolumePlacement>,
    },
    Snapshot {
        intent: SnapshotIntent,
        record: SnapshotRecord,
    },
    Resize {
        intent: ResizeIntent,
        old_bytes: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GangPlan {
    pub plan_id: String,
    pub kind: GangKind,
    pub basis_event_index: u64,
    pub effective_at_ns: u64,
    pub application_ids: BTreeSet<String>,
    pub volume_ids: BTreeSet<String>,
    pub reservations: Vec<ResourceReservation>,
    pub tasks: Vec<GangTask>,
    pub scheduled_flows: Vec<ScheduledFlow>,
    pub recovery_timing: Option<RecoveryTiming>,
    pub effect: GangEffect,
    pub explanation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum SchedulerDecision {
    PrepareGang { plan: GangPlan },
    CommitGang { plan_id: String },
    AbortGang { plan_id: String, reason: String },
    Deferred { subject_id: String, reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GangPhase {
    Preparing,
    Prepared,
    Committed,
    Aborted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GangRuntime {
    plan: GangPlan,
    phase: GangPhase,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RegionRuntime {
    spec: RegionSpec,
    online: bool,
    failed_at_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct HostRuntime {
    spec: HostSpec,
    online: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct VolumeRuntime {
    application_id: String,
    spec: VolumeSpec,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct EstateState {
    last_event_index: u64,
    last_timestamp_ns: u64,
    regions: BTreeMap<String, RegionRuntime>,
    hosts: BTreeMap<String, HostRuntime>,
    business_entities: BTreeMap<String, BusinessEntitySpec>,
    applications: BTreeMap<String, ApplicationSpec>,
    volumes: BTreeMap<String, VolumeRuntime>,
    logical_databases: BTreeMap<String, LogicalDatabaseSpec>,
    consistency_groups: BTreeMap<String, ConsistencyGroupSpec>,
    placements: BTreeMap<String, VolumePlacement>,
    replicas: BTreeMap<String, BTreeMap<String, ReplicaObservation>>,
    mux_backlogs: BTreeMap<String, BTreeMap<String, DurableMuxBacklogObservation>>,
    demand: BTreeMap<String, DemandBucket>,
    snapshots: BTreeMap<String, SnapshotRecord>,
    pending_snapshots: BTreeMap<String, SnapshotIntent>,
    pending_resizes: BTreeMap<String, ResizeIntent>,
    gangs: BTreeMap<String, GangRuntime>,
}

/// Stable counters for comparing admission strategies over the same replayed
/// estate trace.  These describe control-plane work, not data-path IOPS.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerAudit {
    pub planned_units: u64,
    pub committed_units: u64,
    pub aborted_units: u64,
    pub in_flight_units: u64,
    pub planned_volume_attempts: u64,
    pub committed_volume_attempts: u64,
    pub aborted_volume_attempts: u64,
    pub maximum_atomic_width: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EstateCardinality {
    pub regions: u64,
    pub hosts: u64,
    pub business_entities: u64,
    pub applications: u64,
    pub volumes: u64,
    pub logical_databases: u64,
    pub database_volume_memberships: u64,
    pub consistency_groups: u64,
    pub consistency_memberships: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisasterScenario {
    pub scenario_id: String,
    pub unavailable_regions: BTreeSet<String>,
    pub simulated_at_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisasterPreview {
    pub scenario: DisasterScenario,
    pub basis_state_hash: String,
    pub decisions: Vec<SchedulerDecision>,
}

#[derive(Clone)]
pub struct GangScheduler {
    state: EstateState,
    config: GangSchedulerConfig,
}

impl Default for GangScheduler {
    fn default() -> Self {
        Self {
            state: EstateState::default(),
            config: GangSchedulerConfig::default(),
        }
    }
}

/// Preferred name: the scheduler assigns end-to-end flows to local, mux,
/// transport, demux, and terminal lanes. `GangScheduler` remains the concrete
/// name because gang preparation is its atomic commit mechanism.
pub type LaneFlowScheduler = GangScheduler;

impl GangScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(config: GangSchedulerConfig) -> Self {
        Self {
            state: EstateState::default(),
            config,
        }
    }

    pub fn strategy(&self) -> ConcurrentSchedulingStrategy {
        self.config.strategy
    }

    pub fn apply(&mut self, envelope: EstateEventEnvelope) -> io::Result<Vec<SchedulerDecision>> {
        self.validate_envelope(&envelope)?;
        let event_index = envelope.index;
        let now_ns = envelope.timestamp_ns;
        let decisions = match envelope.event {
            EstateEvent::PutRegion { spec } => {
                validate_region(&spec)?;
                let online = self
                    .state
                    .regions
                    .get(&spec.region_id)
                    .map_or(true, |region| region.online);
                self.state.regions.insert(
                    spec.region_id.clone(),
                    RegionRuntime {
                        spec,
                        online,
                        failed_at_ns: None,
                    },
                );
                Vec::new()
            }
            EstateEvent::PutHost { spec } => {
                validate_host(&spec)?;
                if !self.state.regions.contains_key(&spec.region_id) {
                    return Err(invalid(format!(
                        "host {} references unknown region {}",
                        spec.host_id, spec.region_id
                    )));
                }
                let online = self
                    .state
                    .hosts
                    .get(&spec.host_id)
                    .map_or(true, |host| host.online);
                if let Some(existing) = self.state.hosts.get(&spec.host_id)
                    && existing.spec.region_id != spec.region_id
                {
                    return Err(invalid("host region cannot change in place"));
                }
                self.state
                    .hosts
                    .insert(spec.host_id.clone(), HostRuntime { spec, online });
                Vec::new()
            }
            EstateEvent::PutApplication { spec } => {
                self.put_application(spec)?;
                Vec::new()
            }
            EstateEvent::PutBusinessEntity { spec } => {
                self.put_business_entity(spec)?;
                Vec::new()
            }
            EstateEvent::PutLogicalDatabase { spec } => {
                self.put_logical_database(spec)?;
                Vec::new()
            }
            EstateEvent::PutConsistencyGroup { spec } => {
                self.put_consistency_group(spec)?;
                Vec::new()
            }
            EstateEvent::SetRegionOnline { region_id, online } => {
                {
                    let region = self
                        .state
                        .regions
                        .get_mut(&region_id)
                        .ok_or_else(|| invalid(format!("unknown region {region_id}")))?;
                    region.online = online;
                    region.failed_at_ns = if online { None } else { Some(now_ns) };
                }
                if online {
                    Vec::new()
                } else {
                    self.abort_gangs_for_failed_region(&region_id)
                }
            }
            EstateEvent::SetHostOnline { host_id, online } => {
                self.state
                    .hosts
                    .get_mut(&host_id)
                    .ok_or_else(|| invalid(format!("unknown host {host_id}")))?
                    .online = online;
                Vec::new()
            }
            EstateEvent::ObserveReplica { observation } => {
                self.observe_replica(observation, now_ns)?;
                Vec::new()
            }
            EstateEvent::ObserveDurableMuxBacklog { observation } => {
                self.observe_mux_backlog(observation, now_ns)?;
                Vec::new()
            }
            EstateEvent::ObserveDemand { bucket } => {
                self.observe_demand(bucket)?;
                Vec::new()
            }
            EstateEvent::SetApplicationBusinessImpact {
                application_id,
                estimate,
            } => {
                validate_business_impact(&estimate)?;
                self.state
                    .applications
                    .get_mut(&application_id)
                    .ok_or_else(|| invalid(format!("unknown application {application_id}")))?
                    .business_impact = estimate;
                Vec::new()
            }
            EstateEvent::SetApplicationScenarioImpacts {
                application_id,
                rules,
            } => {
                validate_scenario_impacts(&rules)?;
                self.state
                    .applications
                    .get_mut(&application_id)
                    .ok_or_else(|| invalid(format!("unknown application {application_id}")))?
                    .scenario_impacts = rules;
                Vec::new()
            }
            EstateEvent::RequestSnapshot { intent } => {
                self.request_snapshot(intent)?;
                Vec::new()
            }
            EstateEvent::RequestResize { intent } => {
                self.request_resize(intent)?;
                Vec::new()
            }
            EstateEvent::PlanAtWatermark { input_watermark } => {
                if input_watermark > event_index.saturating_sub(1) {
                    return Err(invalid(format!(
                        "planning watermark {input_watermark} is ahead of committed input {}",
                        event_index.saturating_sub(1)
                    )));
                }
                self.plan(event_index, now_ns)?
            }
            EstateEvent::GangPrepared { plan_id, task_ids } => {
                self.gang_prepared(&plan_id, &task_ids)?
            }
            EstateEvent::GangRejected {
                plan_id,
                worker_id,
                reason,
            } => self.gang_rejected(&plan_id, &worker_id, &reason)?,
            EstateEvent::GangCommitted { plan_id } => self.gang_committed(&plan_id, now_ns)?,
        };
        self.state.last_event_index = event_index;
        self.state.last_timestamp_ns = now_ns;
        Ok(decisions)
    }

    pub fn state_hash(&self) -> io::Result<String> {
        stable_hash(&(self.config, &self.state))
    }

    pub fn placement(&self, volume_id: &str) -> Option<&VolumePlacement> {
        self.state.placements.get(volume_id)
    }

    pub fn snapshot(&self, snapshot_id: &str) -> Option<&SnapshotRecord> {
        self.state.snapshots.get(snapshot_id)
    }

    pub fn volume_bytes(&self, volume_id: &str) -> Option<u64> {
        self.state
            .volumes
            .get(volume_id)
            .map(|volume| volume.spec.bytes)
    }

    pub fn committed_gangs(&self) -> usize {
        self.state
            .gangs
            .values()
            .filter(|gang| gang.phase == GangPhase::Committed)
            .count()
    }

    pub fn audit(&self) -> SchedulerAudit {
        let mut audit = SchedulerAudit::default();
        for gang in self.state.gangs.values() {
            let width = u64::try_from(gang.plan.volume_ids.len()).unwrap_or(u64::MAX);
            audit.planned_units = audit.planned_units.saturating_add(1);
            audit.planned_volume_attempts = audit.planned_volume_attempts.saturating_add(width);
            audit.maximum_atomic_width = audit.maximum_atomic_width.max(width);
            match gang.phase {
                GangPhase::Committed => {
                    audit.committed_units = audit.committed_units.saturating_add(1);
                    audit.committed_volume_attempts =
                        audit.committed_volume_attempts.saturating_add(width);
                }
                GangPhase::Aborted => {
                    audit.aborted_units = audit.aborted_units.saturating_add(1);
                    audit.aborted_volume_attempts =
                        audit.aborted_volume_attempts.saturating_add(width);
                }
                GangPhase::Preparing | GangPhase::Prepared => {
                    audit.in_flight_units = audit.in_flight_units.saturating_add(1);
                }
            }
        }
        audit
    }

    pub fn cardinality(&self) -> EstateCardinality {
        EstateCardinality {
            regions: u64::try_from(self.state.regions.len()).unwrap_or(u64::MAX),
            hosts: u64::try_from(self.state.hosts.len()).unwrap_or(u64::MAX),
            business_entities: u64::try_from(self.state.business_entities.len())
                .unwrap_or(u64::MAX),
            applications: u64::try_from(self.state.applications.len()).unwrap_or(u64::MAX),
            volumes: u64::try_from(self.state.volumes.len()).unwrap_or(u64::MAX),
            logical_databases: u64::try_from(self.state.logical_databases.len())
                .unwrap_or(u64::MAX),
            database_volume_memberships: self
                .state
                .logical_databases
                .values()
                .map(|database| u64::try_from(database.volume_ids.len()).unwrap_or(u64::MAX))
                .fold(0u64, u64::saturating_add),
            consistency_groups: u64::try_from(self.state.consistency_groups.len())
                .unwrap_or(u64::MAX),
            consistency_memberships: self
                .state
                .consistency_groups
                .values()
                .map(|group| u64::try_from(group.volume_ids.len()).unwrap_or(u64::MAX))
                .fold(0u64, u64::saturating_add),
        }
    }

    pub fn related_volume_closure(
        &self,
        seed_volume_ids: &BTreeSet<String>,
    ) -> io::Result<BTreeSet<String>> {
        if seed_volume_ids
            .iter()
            .any(|volume_id| !self.state.volumes.contains_key(volume_id))
        {
            return Err(invalid(
                "consistency closure seed references unknown volume",
            ));
        }
        let mut closure = seed_volume_ids.clone();
        loop {
            let mut changed = false;
            for group in self.state.consistency_groups.values() {
                if !closure.is_disjoint(&group.volume_ids) {
                    let before = closure.len();
                    closure.extend(group.volume_ids.iter().cloned());
                    changed |= closure.len() != before;
                    if closure.len() > MAX_SNAPSHOT_MEMBERS {
                        return Err(invalid("consistency closure exceeds snapshot member limit"));
                    }
                }
            }
            if !changed {
                return Ok(closure);
            }
        }
    }

    /// Reports baseline business impact exposed by each physical volume. An
    /// application is counted once per volume even if it consumes many logical
    /// databases stored there; this avoids multiplying one outage consequence
    /// by database count.
    pub fn physical_volume_impacts(&self) -> BTreeMap<String, PhysicalVolumeImpact> {
        let mut reports = self
            .state
            .volumes
            .iter()
            .map(|(volume_id, volume)| {
                let affected_application_ids = BTreeSet::from([volume.application_id.clone()]);
                (
                    volume_id.clone(),
                    PhysicalVolumeImpact {
                        volume_id: volume_id.clone(),
                        owning_application_id: volume.application_id.clone(),
                        logical_database_count: 0,
                        affected_application_ids,
                        aggregate_impact_floor_microunits: 0,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        for database in self.state.logical_databases.values() {
            for volume_id in &database.volume_ids {
                let report = reports
                    .get_mut(volume_id)
                    .expect("validated logical database volume disappeared");
                report.logical_database_count = report.logical_database_count.saturating_add(1);
                report
                    .affected_application_ids
                    .extend(database.consumer_application_ids.iter().cloned());
            }
        }
        for report in reports.values_mut() {
            report.aggregate_impact_floor_microunits = report
                .affected_application_ids
                .iter()
                .filter_map(|application_id| self.state.applications.get(application_id))
                .map(|application| business_impact_floor(&application.business_impact))
                .fold(0u128, u128::saturating_add);
        }
        reports
    }

    pub fn highest_impact_volumes(&self) -> Vec<PhysicalVolumeImpact> {
        let reports = self.physical_volume_impacts();
        let maximum = reports
            .values()
            .map(|report| report.aggregate_impact_floor_microunits)
            .max();
        reports
            .into_values()
            .filter(|report| Some(report.aggregate_impact_floor_microunits) == maximum)
            .collect()
    }

    /// Evaluates a failure set against the exact reduced estate without
    /// changing it.  This is suitable for continuously dry-running approved
    /// single-loss, multiple-loss, and capacity-exhaustion exercises.
    pub fn preview_disaster(&self, scenario: DisasterScenario) -> io::Result<DisasterPreview> {
        validate_id(&scenario.scenario_id, "scenario_id")?;
        if scenario.unavailable_regions.is_empty()
            || scenario.simulated_at_ns < self.state.last_timestamp_ns
        {
            return Err(invalid("invalid disaster preview failure set or timestamp"));
        }
        let basis_state_hash = self.state_hash()?;
        let mut trial = self.clone();
        let mut decisions = Vec::new();
        for region_id in &scenario.unavailable_regions {
            let region = trial
                .state
                .regions
                .get_mut(region_id)
                .ok_or_else(|| invalid(format!("unknown preview region {region_id}")))?;
            region.online = false;
            region.failed_at_ns = Some(scenario.simulated_at_ns);
            decisions.extend(trial.abort_gangs_for_failed_region(region_id));
        }
        decisions.extend(trial.plan(
            trial.state.last_event_index.saturating_add(1),
            scenario.simulated_at_ns,
        )?);
        Ok(DisasterPreview {
            scenario,
            basis_state_hash,
            decisions,
        })
    }

    fn validate_envelope(&self, envelope: &EstateEventEnvelope) -> io::Result<()> {
        if envelope.schema_version != GANG_SCHEDULER_SCHEMA_VERSION {
            return Err(invalid("unsupported gang scheduler schema"));
        }
        if envelope.index != self.state.last_event_index.saturating_add(1) {
            return Err(invalid(format!(
                "estate event index must advance by one expected={} actual={}",
                self.state.last_event_index.saturating_add(1),
                envelope.index
            )));
        }
        if envelope.timestamp_ns < self.state.last_timestamp_ns {
            return Err(invalid("estate event timestamp regressed"));
        }
        Ok(())
    }

    fn put_business_entity(&mut self, spec: BusinessEntitySpec) -> io::Result<()> {
        validate_id(&spec.business_entity_id, "business_entity_id")?;
        if spec.generation == 0 || spec.region_ids.is_empty() {
            return Err(invalid(
                "business entity needs a generation and at least one region",
            ));
        }
        for region_id in &spec.region_ids {
            validate_id(region_id, "business entity region_id")?;
            if !self.state.regions.contains_key(region_id) {
                return Err(invalid(format!(
                    "business entity {} references unknown region {region_id}",
                    spec.business_entity_id
                )));
            }
        }
        if let Some(existing) = self.state.business_entities.get(&spec.business_entity_id) {
            if spec.generation < existing.generation {
                return Err(invalid("business entity generation regressed"));
            }
            if spec.generation == existing.generation {
                if existing == &spec {
                    return Ok(());
                }
                return Err(invalid(
                    "business entity generation was reused with new content",
                ));
            }
            for application in self
                .state
                .applications
                .values()
                .filter(|application| application.business_entity_id == spec.business_entity_id)
            {
                if application
                    .volumes
                    .iter()
                    .any(|volume| !spec.region_ids.contains(&volume.home_region_id))
                {
                    return Err(invalid(format!(
                        "business entity {} cannot remove a region used by application {}",
                        spec.business_entity_id, application.application_id
                    )));
                }
            }
        }
        self.state
            .business_entities
            .insert(spec.business_entity_id.clone(), spec);
        Ok(())
    }

    fn put_application(&mut self, spec: ApplicationSpec) -> io::Result<()> {
        validate_id(&spec.application_id, "application_id")?;
        validate_id(&spec.business_entity_id, "application business_entity_id")?;
        if spec.volumes.is_empty() || spec.volumes.len() > MAX_VOLUMES_PER_APPLICATION {
            return Err(invalid("invalid application weight or volume count"));
        }
        validate_business_impact(&spec.business_impact)?;
        validate_scenario_impacts(&spec.scenario_impacts)?;
        if self.state.applications.contains_key(&spec.application_id) {
            return Err(invalid(format!(
                "application {} already exists",
                spec.application_id
            )));
        }
        let business_entity = self
            .state
            .business_entities
            .get(&spec.business_entity_id)
            .ok_or_else(|| {
                invalid(format!(
                    "application {} references unknown business entity {}",
                    spec.application_id, spec.business_entity_id
                ))
            })?;
        let mut ids = BTreeSet::new();
        for volume in &spec.volumes {
            validate_volume(volume)?;
            if !self.state.regions.contains_key(&volume.home_region_id) {
                return Err(invalid(format!(
                    "volume {} references unknown home region {}",
                    volume.volume_id, volume.home_region_id
                )));
            }
            if !business_entity.region_ids.contains(&volume.home_region_id) {
                return Err(invalid(format!(
                    "volume {} home region {} is outside business entity {} presence",
                    volume.volume_id, volume.home_region_id, spec.business_entity_id
                )));
            }
            if !ids.insert(volume.volume_id.clone())
                || self.state.volumes.contains_key(&volume.volume_id)
            {
                return Err(invalid(format!("duplicate volume {}", volume.volume_id)));
            }
        }
        for volume in &spec.volumes {
            self.state.volumes.insert(
                volume.volume_id.clone(),
                VolumeRuntime {
                    application_id: spec.application_id.clone(),
                    spec: volume.clone(),
                },
            );
        }
        self.state
            .applications
            .insert(spec.application_id.clone(), spec);
        Ok(())
    }

    fn put_logical_database(&mut self, spec: LogicalDatabaseSpec) -> io::Result<()> {
        validate_id(&spec.database_id, "database_id")?;
        validate_id(
            &spec.cluster_application_id,
            "database cluster_application_id",
        )?;
        if spec.generation == 0
            || spec.volume_ids.is_empty()
            || spec.volume_ids.len() > MAX_DATABASE_VOLUME_MEMBERS
            || spec.consumer_application_ids.is_empty()
            || spec.consumer_application_ids.len() > MAX_DATABASE_CONSUMERS
        {
            return Err(invalid(
                "logical database needs a generation, cluster volumes, and consumers",
            ));
        }
        let cluster = self
            .state
            .applications
            .get(&spec.cluster_application_id)
            .ok_or_else(|| invalid("logical database references unknown cluster application"))?;
        if !matches!(
            cluster.kind,
            ApplicationKind::CassandraJbod
                | ApplicationKind::CockroachDb
                | ApplicationKind::Postgres
                | ApplicationKind::PerformanceShardedDatabase
        ) {
            return Err(invalid(
                "logical database cluster application is not a database kind",
            ));
        }
        let cluster_volume_ids = cluster
            .volumes
            .iter()
            .map(|volume| volume.volume_id.as_str())
            .collect::<BTreeSet<_>>();
        if spec
            .volume_ids
            .iter()
            .any(|volume_id| !cluster_volume_ids.contains(volume_id.as_str()))
        {
            return Err(invalid(
                "logical database volume is not owned by its cluster application",
            ));
        }
        if spec
            .consumer_application_ids
            .iter()
            .any(|application_id| !self.state.applications.contains_key(application_id))
        {
            return Err(invalid(
                "logical database references an unknown consumer application",
            ));
        }
        if let Some(existing) = self.state.logical_databases.get(&spec.database_id) {
            if spec.generation < existing.generation {
                return Err(invalid("logical database generation regressed"));
            }
            if spec.generation == existing.generation {
                if existing == &spec {
                    return Ok(());
                }
                return Err(invalid(
                    "logical database generation was reused with new content",
                ));
            }
        }
        self.state
            .logical_databases
            .insert(spec.database_id.clone(), spec);
        Ok(())
    }

    fn put_consistency_group(&mut self, spec: ConsistencyGroupSpec) -> io::Result<()> {
        validate_id(&spec.group_id, "consistency group_id")?;
        if spec.generation == 0
            || spec.volume_ids.len() < 2
            || spec.volume_ids.len() > MAX_SNAPSHOT_MEMBERS
            || spec
                .volume_ids
                .iter()
                .any(|volume_id| !self.state.volumes.contains_key(volume_id))
        {
            return Err(invalid(
                "consistency group needs a generation and two or more known volumes",
            ));
        }
        if let Some(existing) = self.state.consistency_groups.get(&spec.group_id) {
            if spec.generation < existing.generation {
                return Err(invalid("consistency group generation regressed"));
            }
            if spec.generation == existing.generation {
                if existing == &spec {
                    return Ok(());
                }
                return Err(invalid(
                    "consistency group generation was reused with new content",
                ));
            }
        }
        self.state
            .consistency_groups
            .insert(spec.group_id.clone(), spec);
        Ok(())
    }

    fn observe_replica(&mut self, observation: ReplicaObservation, now_ns: u64) -> io::Result<()> {
        if !self.state.volumes.contains_key(&observation.volume_id)
            || !self.state.regions.contains_key(&observation.region_id)
        {
            return Err(invalid(
                "replica observation references unknown volume or region",
            ));
        }
        if observation.applied_hwm > observation.durable_hwm || observation.observed_at_ns > now_ns
        {
            return Err(invalid("invalid replica observation HWM or timestamp"));
        }
        let old = self
            .state
            .replicas
            .get(&observation.volume_id)
            .and_then(|regions| regions.get(&observation.region_id));
        if let Some(old) = old
            && (observation.observed_at_ns < old.observed_at_ns
                || observation.durable_hwm < old.durable_hwm
                || observation.applied_hwm < old.applied_hwm)
        {
            return Err(invalid("replica observation regressed"));
        }
        self.state
            .replicas
            .entry(observation.volume_id.clone())
            .or_default()
            .insert(observation.region_id.clone(), observation);
        Ok(())
    }

    fn observe_mux_backlog(
        &mut self,
        observation: DurableMuxBacklogObservation,
        now_ns: u64,
    ) -> io::Result<()> {
        if observation.layers.len() > 64
            || observation.applied_hwm > observation.durable_hwm
            || observation.observed_at_ns > now_ns
            || !self.state.volumes.contains_key(&observation.volume_id)
            || !self
                .state
                .regions
                .contains_key(&observation.recovery_region_id)
        {
            return Err(invalid("invalid durable mux backlog observation"));
        }
        let mut paths = BTreeSet::new();
        for layer in &observation.layers {
            validate_id(&layer.path_id, "mux path_id")?;
            if layer.demux_bytes_per_second == 0
                || layer.demux_operations_per_second == 0
                || (layer.copy_passes > 0 && layer.copy_bytes_per_second == 0)
                || !paths.insert(layer.path_id.clone())
            {
                return Err(invalid("invalid or duplicate mux backlog layer"));
            }
        }
        if let Some(old) = self
            .state
            .mux_backlogs
            .get(&observation.volume_id)
            .and_then(|regions| regions.get(&observation.recovery_region_id))
            && (observation.observed_at_ns < old.observed_at_ns
                || observation.durable_hwm < old.durable_hwm
                || observation.applied_hwm < old.applied_hwm
                || observation.bytes_pending > old.bytes_pending
                || observation.operations_pending > old.operations_pending)
        {
            return Err(invalid("durable mux backlog progress regressed"));
        }
        self.observe_replica(
            ReplicaObservation {
                volume_id: observation.volume_id.clone(),
                region_id: observation.recovery_region_id.clone(),
                durable_hwm: observation.durable_hwm,
                applied_hwm: observation.applied_hwm,
                observed_at_ns: observation.observed_at_ns,
            },
            now_ns,
        )?;
        self.state
            .mux_backlogs
            .entry(observation.volume_id.clone())
            .or_default()
            .insert(observation.recovery_region_id.clone(), observation);
        Ok(())
    }

    fn abort_gangs_for_failed_region(&mut self, region_id: &str) -> Vec<SchedulerDecision> {
        let affected = self
            .state
            .gangs
            .iter()
            .filter(|(_, gang)| matches!(gang.phase, GangPhase::Preparing | GangPhase::Prepared))
            .filter(|(_, gang)| {
                gang.plan.reservations.iter().any(|reservation| {
                    self.state
                        .hosts
                        .get(&reservation.host_id)
                        .is_some_and(|host| host.spec.region_id == region_id)
                })
            })
            .map(|(plan_id, _)| plan_id.clone())
            .collect::<Vec<_>>();
        let mut decisions = Vec::new();
        for plan_id in affected {
            match self.gang_rejected(
                &plan_id,
                &format!("region:{region_id}"),
                "region became unavailable during preparation",
            ) {
                Ok(mut aborted) => decisions.append(&mut aborted),
                Err(error) => decisions.push(SchedulerDecision::Deferred {
                    subject_id: plan_id,
                    reason: format!("failed to unwind region-loss gang: {error}"),
                }),
            }
        }
        decisions
    }

    fn observe_demand(&mut self, bucket: DemandBucket) -> io::Result<()> {
        if !self.state.applications.contains_key(&bucket.application_id)
            || bucket.interval_end_ns <= bucket.interval_start_ns
            || (!bucket.exact && bucket.samples == 0)
        {
            return Err(invalid("invalid demand bucket"));
        }
        if let Some(old) = self.state.demand.get(&bucket.application_id)
            && bucket.interval_end_ns <= old.interval_end_ns
        {
            return Err(invalid("demand bucket did not advance"));
        }
        self.state
            .demand
            .insert(bucket.application_id.clone(), bucket);
        Ok(())
    }

    fn request_snapshot(&mut self, mut intent: SnapshotIntent) -> io::Result<()> {
        validate_id(&intent.snapshot_id, "snapshot_id")?;
        if intent.volume_ids.is_empty()
            || intent.volume_ids.len() > MAX_SNAPSHOT_MEMBERS
            || intent.deadline_ns == 0
            || intent.maximum_hitch_ns == 0
            || intent.operation_iops_per_volume == 0
        {
            return Err(invalid("invalid snapshot intent"));
        }
        if intent
            .volume_ids
            .iter()
            .any(|volume| !self.state.volumes.contains_key(volume))
        {
            return Err(invalid("snapshot references unknown volume"));
        }
        if intent.expand_consistency_relationships {
            intent.volume_ids = self.related_volume_closure(&intent.volume_ids)?;
        }
        if self.state.snapshots.contains_key(&intent.snapshot_id)
            || self
                .state
                .pending_snapshots
                .contains_key(&intent.snapshot_id)
        {
            return Err(invalid("duplicate snapshot id"));
        }
        self.state
            .pending_snapshots
            .insert(intent.snapshot_id.clone(), intent);
        Ok(())
    }

    fn request_resize(&mut self, intent: ResizeIntent) -> io::Result<()> {
        validate_id(&intent.operation_id, "resize operation_id")?;
        let volume = self
            .state
            .volumes
            .get(&intent.volume_id)
            .ok_or_else(|| invalid("resize references unknown volume"))?;
        if intent.new_bytes <= volume.spec.bytes || intent.deadline_ns == 0 {
            return Err(invalid("resize must grow the volume and have a deadline"));
        }
        if self
            .state
            .pending_resizes
            .contains_key(&intent.operation_id)
        {
            return Err(invalid("duplicate resize operation"));
        }
        self.state
            .pending_resizes
            .insert(intent.operation_id.clone(), intent);
        Ok(())
    }

    fn gang_prepared(
        &mut self,
        plan_id: &str,
        task_ids: &BTreeSet<String>,
    ) -> io::Result<Vec<SchedulerDecision>> {
        let gang = self
            .state
            .gangs
            .get_mut(plan_id)
            .ok_or_else(|| invalid(format!("unknown gang {plan_id}")))?;
        if gang.phase != GangPhase::Preparing {
            return Err(invalid("only a preparing gang can become prepared"));
        }
        let expected = gang
            .plan
            .tasks
            .iter()
            .map(|task| task.task_id.clone())
            .collect::<BTreeSet<_>>();
        if &expected != task_ids {
            return Err(invalid("gang preparation did not cover its exact task set"));
        }
        gang.phase = GangPhase::Prepared;
        Ok(vec![SchedulerDecision::CommitGang {
            plan_id: plan_id.to_string(),
        }])
    }

    fn gang_rejected(
        &mut self,
        plan_id: &str,
        worker_id: &str,
        reason: &str,
    ) -> io::Result<Vec<SchedulerDecision>> {
        let gang = self
            .state
            .gangs
            .get_mut(plan_id)
            .ok_or_else(|| invalid(format!("unknown gang {plan_id}")))?;
        if !matches!(gang.phase, GangPhase::Preparing | GangPhase::Prepared) {
            return Err(invalid("committed or aborted gang cannot be rejected"));
        }
        gang.phase = GangPhase::Aborted;
        match &gang.plan.effect {
            GangEffect::Snapshot { intent, .. } => {
                self.state
                    .pending_snapshots
                    .insert(intent.snapshot_id.clone(), intent.clone());
            }
            GangEffect::Resize { intent, .. } => {
                self.state
                    .pending_resizes
                    .insert(intent.operation_id.clone(), intent.clone());
            }
            GangEffect::Place { .. } => {}
        }
        Ok(vec![SchedulerDecision::AbortGang {
            plan_id: plan_id.to_string(),
            reason: format!("worker={worker_id} reason={reason}"),
        }])
    }

    fn gang_committed(&mut self, plan_id: &str, now_ns: u64) -> io::Result<Vec<SchedulerDecision>> {
        let effect = {
            let gang = self
                .state
                .gangs
                .get_mut(plan_id)
                .ok_or_else(|| invalid(format!("unknown gang {plan_id}")))?;
            if gang.phase != GangPhase::Prepared {
                return Err(invalid("only a prepared gang can commit"));
            }
            gang.phase = GangPhase::Committed;
            gang.plan.effect.clone()
        };
        match effect {
            GangEffect::Place { placements } => {
                for placement in placements {
                    self.state
                        .placements
                        .insert(placement.volume_id.clone(), placement);
                }
            }
            GangEffect::Snapshot { mut record, .. } => {
                record.committed_at_ns = now_ns;
                self.state
                    .snapshots
                    .insert(record.snapshot_id.clone(), record);
            }
            GangEffect::Resize { intent, old_bytes } => {
                let volume = self
                    .state
                    .volumes
                    .get_mut(&intent.volume_id)
                    .ok_or_else(|| invalid("resize volume disappeared"))?;
                if volume.spec.bytes != old_bytes {
                    return Err(invalid("resize basis changed before commit"));
                }
                volume.spec.bytes = intent.new_bytes;
            }
        }
        Ok(Vec::new())
    }

    fn plan(&mut self, basis: u64, now_ns: u64) -> io::Result<Vec<SchedulerDecision>> {
        let mut decisions = Vec::new();
        let mut scratch = ResourceScratch::from_state(&self.state)?;
        let mut recovery_schedule = RecoveryScheduleScratch::from_state(&self.state);
        let busy = self.busy_volumes();
        self.plan_failovers(
            basis,
            now_ns,
            &busy,
            &mut scratch,
            &mut recovery_schedule,
            &mut decisions,
        )?;
        self.plan_initial_placements(basis, now_ns, &busy, &mut scratch, &mut decisions)?;
        self.plan_resizes(basis, now_ns, &busy, &mut scratch, &mut decisions)?;
        self.plan_snapshots(basis, now_ns, &busy, &mut scratch, &mut decisions)?;
        Ok(decisions)
    }

    fn busy_volumes(&self) -> BTreeSet<String> {
        self.state
            .gangs
            .values()
            .filter(|gang| matches!(gang.phase, GangPhase::Preparing | GangPhase::Prepared))
            .flat_map(|gang| gang.plan.volume_ids.iter().cloned())
            .collect()
    }

    fn plan_initial_placements(
        &mut self,
        basis: u64,
        now_ns: u64,
        busy: &BTreeSet<String>,
        scratch: &mut ResourceScratch,
        decisions: &mut Vec<SchedulerDecision>,
    ) -> io::Result<()> {
        let mut application_ids = self.state.applications.keys().cloned().collect::<Vec<_>>();
        application_ids.sort_by(|left, right| {
            (
                std::cmp::Reverse(business_impact_floor(
                    &self.state.applications[left].business_impact,
                )),
                left,
            )
                .cmp(&(
                    std::cmp::Reverse(business_impact_floor(
                        &self.state.applications[right].business_impact,
                    )),
                    right,
                ))
        });
        for application_id in application_ids {
            let app = &self.state.applications[&application_id];
            let volumes = app
                .volumes
                .iter()
                .filter(|volume| {
                    !self.state.placements.contains_key(&volume.volume_id)
                        && !busy.contains(&volume.volume_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            if volumes.is_empty() {
                continue;
            }
            let units = match self.config.strategy {
                ConcurrentSchedulingStrategy::Gang => vec![volumes],
                ConcurrentSchedulingStrategy::Arena | ConcurrentSchedulingStrategy::Hybrid => {
                    volumes.into_iter().map(|volume| vec![volume]).collect()
                }
            };
            for unit in units {
                let mut candidate = scratch.clone();
                let mut placements = Vec::new();
                let mut reservations = Vec::new();
                let mut failure = None;
                for volume in &unit {
                    match self.allocate_volume(volume, &volume.home_region_id, 1, &mut candidate) {
                        Ok((placement, allocated)) => {
                            placements.push(placement);
                            reservations.extend(allocated);
                        }
                        Err(error) => {
                            failure = Some(error.to_string());
                            break;
                        }
                    }
                }
                if let Some(reason) = failure {
                    let subject = if unit.len() == 1 {
                        unit[0].volume_id.clone()
                    } else {
                        application_id.clone()
                    };
                    decisions.push(SchedulerDecision::Deferred {
                        subject_id: subject,
                        reason: format!("initial placement infeasible: {reason}"),
                    });
                    continue;
                }
                *scratch = candidate;
                let description = if unit.len() == 1 {
                    format!(
                        "arena-admit volume {} for application {application_id} on a home-region userspace lane",
                        unit[0].volume_id
                    )
                } else {
                    format!(
                        "gang-place application {application_id} on home-region userspace lanes"
                    )
                };
                let plan = self.build_place_plan(
                    GangKind::InitialPlacement,
                    basis,
                    now_ns,
                    placements,
                    reservations,
                    description,
                    None,
                )?;
                self.insert_plan(plan, decisions)?;
            }
        }
        Ok(())
    }

    fn plan_failovers(
        &mut self,
        basis: u64,
        now_ns: u64,
        busy: &BTreeSet<String>,
        scratch: &mut ResourceScratch,
        recovery_schedule: &mut RecoveryScheduleScratch,
        decisions: &mut Vec<SchedulerDecision>,
    ) -> io::Result<()> {
        let mut groups: BTreeMap<(String, String), Vec<VolumeSpec>> = BTreeMap::new();
        for (volume_id, placement) in &self.state.placements {
            if busy.contains(volume_id)
                || self
                    .state
                    .regions
                    .get(&placement.region_id)
                    .is_some_and(|region| region.online)
            {
                continue;
            }
            let volume = &self.state.volumes[volume_id];
            let group = volume
                .spec
                .recovery_group_id
                .clone()
                .unwrap_or_else(|| volume_id.clone());
            groups
                .entry((volume.application_id.clone(), group))
                .or_default()
                .push(volume.spec.clone());
        }
        let unavailable_regions = self
            .state
            .regions
            .iter()
            .filter(|(_, region)| !region.online)
            .map(|(region_id, _)| region_id.clone())
            .collect::<BTreeSet<_>>();
        let mut ordered = groups
            .into_iter()
            .map(|((application, group), volumes)| {
                let source = self.state.placements[&volumes[0].volume_id]
                    .region_id
                    .clone();
                let failed_at = self.state.regions[&source].failed_at_ns.unwrap_or(now_ns);
                let base_rto = volumes
                    .iter()
                    .filter_map(|volume| volume.recovery.rto_ns)
                    .min();
                let application_spec = &self.state.applications[&application];
                let (scenario_impact, scenario_rto, scenario_error) =
                    match effective_scenario_impact(application_spec, &unavailable_regions) {
                        Ok(Some(rule)) => (&rule.impact, Some(rule.rto_ns), None),
                        Ok(None) => (&application_spec.business_impact, base_rto, None),
                        Err(reason) => (&application_spec.business_impact, base_rto, Some(reason)),
                    };
                let deadline = scenario_rto
                    .map(|rto| failed_at.saturating_add(rto))
                    .unwrap_or(u64::MAX);
                let missing_operations = volumes
                    .iter()
                    .map(|volume| {
                        self.state
                            .replicas
                            .get(&volume.volume_id)
                            .and_then(|regions| {
                                regions
                                    .values()
                                    .filter(|replica| self.state.regions[&replica.region_id].online)
                                    .map(|replica| {
                                        volume.latest_hwm.saturating_sub(replica.durable_hwm)
                                    })
                                    .min()
                            })
                            .unwrap_or(volume.latest_hwm)
                    })
                    .sum::<u64>();
                let impact = estimated_recovery_impact(
                    scenario_impact,
                    deadline.saturating_sub(failed_at),
                    missing_operations,
                );
                (
                    std::cmp::Reverse(impact),
                    deadline,
                    application,
                    group,
                    source,
                    volumes,
                    scenario_error,
                    scenario_rto.is_some(),
                )
            })
            .collect::<Vec<_>>();
        ordered.sort_by(|left, right| {
            (&left.0, &left.1, &left.2, &left.3, &left.4)
                .cmp(&(&right.0, &right.1, &right.2, &right.3, &right.4))
        });

        for (_, deadline, application_id, group_id, source, volumes, scenario_error, has_rto) in
            ordered
        {
            if let Some(reason) = scenario_error {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: format!("recovery:{application_id}:{group_id}"),
                    reason: format!("ambiguous scenario business-impact policy: {reason}"),
                });
                continue;
            }
            if self.config.strategy == ConcurrentSchedulingStrategy::Arena && volumes.len() > 1 {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: format!("recovery:{application_id}:{group_id}"),
                    reason: "arena-only scheduling cannot weaken a multi-volume recovery boundary; select hybrid or gang scheduling".into(),
                });
                continue;
            }
            if volumes.iter().any(|volume| {
                !volume
                    .recovery
                    .scenarios
                    .contains(&FailureScenario::RegionLoss)
            }) || !has_rto
            {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: format!("recovery:{application_id}:{group_id}"),
                    reason: "policy has no region-loss RTO; wait for in-region recovery".into(),
                });
                continue;
            }
            let mut selected_actions = Vec::new();
            let mut ambiguous = None;
            for volume in &volumes {
                match effective_failover_action(&volume.recovery, &unavailable_regions) {
                    Ok(action) => selected_actions.push(action.cloned()),
                    Err(reason) => {
                        ambiguous = Some(reason);
                        break;
                    }
                }
            }
            if let Some(reason) = ambiguous {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: format!("recovery:{application_id}:{group_id}"),
                    reason: format!("ambiguous pre-approved failover policy: {reason}"),
                });
                continue;
            }
            if selected_actions
                .iter()
                .any(|action| matches!(action, Some(FailoverAction::HoldDurably)))
            {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: format!("recovery:{application_id}:{group_id}"),
                    reason: "pre-approved disaster policy says to hold the durable mux backlog without materializing this recovery group".into(),
                });
                continue;
            }
            let eligible = self.eligible_recovery_regions(&volumes, &source);
            let mut ranked = eligible
                .into_iter()
                .filter_map(|region| {
                    self.estimate_recovery_timing(
                        &volumes,
                        &region,
                        now_ns,
                        deadline,
                        recovery_schedule,
                    )
                    .map(|timing| (timing.estimated_completion_ns, region, timing))
                })
                .collect::<Vec<_>>();
            ranked.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
            let mut selected = None;
            for (_, region, timing) in ranked {
                let mut candidate = scratch.clone();
                let mut placements = Vec::new();
                let mut reservations = Vec::new();
                let mut feasible = true;
                for volume in &volumes {
                    let next_epoch = self.state.placements[&volume.volume_id]
                        .placement_epoch
                        .saturating_add(1);
                    match self.allocate_volume(volume, &region, next_epoch, &mut candidate) {
                        Ok((placement, allocated)) => {
                            placements.push(placement);
                            reservations.extend(allocated);
                        }
                        Err(_) => {
                            feasible = false;
                            break;
                        }
                    }
                }
                if feasible {
                    selected = Some((candidate, placements, reservations, region, timing));
                    break;
                }
            }
            let Some((candidate, placements, reservations, region, timing)) = selected else {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: format!("recovery:{application_id}:{group_id}"),
                    reason: "no policy-eligible destination meets RPO, copy, and lane capacity requirements"
                        .into(),
                });
                continue;
            };
            *scratch = candidate;
            recovery_schedule.reserve(&timing);
            let plan = self.build_place_plan(
                GangKind::RegionalFailover,
                basis,
                now_ns,
                placements,
                reservations,
                format!(
                    "fail over recovery group {group_id} from {source} to {region}; queue_start_ns={} demux_ns={} target_materialization_ns={} estimated_completion_ns={} deadline_ns={deadline} rto_met={}",
                    timing.queue_start_ns,
                    timing.demux_transfer_ns.saturating_add(timing.pipeline_fill_ns),
                    timing.target_materialization_ns,
                    timing.estimated_completion_ns,
                    timing.rto_met,
                ),
                Some(timing),
            )?;
            self.insert_plan(plan, decisions)?;
        }
        Ok(())
    }

    fn plan_resizes(
        &mut self,
        basis: u64,
        now_ns: u64,
        busy: &BTreeSet<String>,
        scratch: &mut ResourceScratch,
        decisions: &mut Vec<SchedulerDecision>,
    ) -> io::Result<()> {
        let mut intents = self
            .state
            .pending_resizes
            .values()
            .cloned()
            .collect::<Vec<_>>();
        intents.sort_by(|left, right| {
            (left.deadline_ns, left.operation_id.as_str())
                .cmp(&(right.deadline_ns, right.operation_id.as_str()))
        });
        for intent in intents {
            if busy.contains(&intent.volume_id) {
                continue;
            }
            let Some(placement) = self.state.placements.get(&intent.volume_id).cloned() else {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: intent.operation_id.clone(),
                    reason: "volume has no active placement".into(),
                });
                continue;
            };
            let volume = &self.state.volumes[&intent.volume_id].spec;
            let old_bytes = volume.bytes;
            let isolation = volume.isolation;
            let application_id = self.state.volumes[&intent.volume_id].application_id.clone();
            let extra = intent.new_bytes.saturating_sub(old_bytes);
            let mut candidate = scratch.clone();
            let mut reservations = Vec::new();
            let mut feasible = true;
            for leg in &placement.legs {
                if candidate
                    .reserve(
                        &intent.volume_id,
                        &leg.host_id,
                        leg.lane,
                        0,
                        extra,
                        isolation,
                    )
                    .is_err()
                {
                    feasible = false;
                    break;
                }
                reservations.push(ResourceReservation {
                    volume_id: intent.volume_id.clone(),
                    host_id: leg.host_id.clone(),
                    lane: leg.lane,
                    iops: 0,
                    bytes: extra,
                    isolation,
                    lifetime: ReservationLifetime::Persistent,
                });
            }
            if !feasible {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: intent.operation_id.clone(),
                    reason: "resize would exceed a currently placed host capacity".into(),
                });
                continue;
            }
            *scratch = candidate;
            self.state.pending_resizes.remove(&intent.operation_id);
            let task_host = placement.legs[0].host_id.clone();
            let effect = GangEffect::Resize {
                intent: intent.clone(),
                old_bytes,
            };
            let scheduled_flows =
                terminal_scheduled_flows(&reservations, &format!("resize:{}", intent.operation_id));
            let plan = finish_plan(
                GangKind::VolumeResize,
                basis,
                now_ns,
                BTreeSet::from([application_id]),
                BTreeSet::from([intent.volume_id.clone()]),
                reservations,
                vec![GangTask {
                    task_id: format!("resize:{}", intent.volume_id),
                    worker_id: task_host,
                    kind: GangTaskKind::Resize,
                    depends_on: BTreeSet::new(),
                }],
                scheduled_flows,
                None,
                effect,
                format!(
                    "grow {} from {} to {} bytes",
                    intent.volume_id, old_bytes, intent.new_bytes
                ),
            )?;
            self.insert_plan(plan, decisions)?;
        }
        Ok(())
    }

    fn plan_snapshots(
        &mut self,
        basis: u64,
        now_ns: u64,
        busy: &BTreeSet<String>,
        scratch: &mut ResourceScratch,
        decisions: &mut Vec<SchedulerDecision>,
    ) -> io::Result<()> {
        let mut intents = self
            .state
            .pending_snapshots
            .values()
            .cloned()
            .collect::<Vec<_>>();
        intents.sort_by(|left, right| {
            (left.deadline_ns, left.snapshot_id.as_str())
                .cmp(&(right.deadline_ns, right.snapshot_id.as_str()))
        });
        for intent in intents {
            if intent.volume_ids.iter().any(|volume| busy.contains(volume)) {
                continue;
            }
            if self.config.strategy == ConcurrentSchedulingStrategy::Arena
                && intent.volume_ids.len() > 1
            {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: intent.snapshot_id.clone(),
                    reason: "arena-only scheduling cannot weaken a multi-volume consistency cut; select hybrid or gang scheduling".into(),
                });
                continue;
            }
            let placements = intent
                .volume_ids
                .iter()
                .map(|volume| self.state.placements.get(volume).cloned())
                .collect::<Option<Vec<_>>>();
            let Some(placements) = placements else {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: intent.snapshot_id.clone(),
                    reason: "snapshot member lacks an active placement".into(),
                });
                continue;
            };
            let regions = placements
                .iter()
                .map(|placement| placement.region_id.clone())
                .collect::<BTreeSet<_>>();
            if intent.scope == SnapshotScope::SameRegion && regions.len() != 1 {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: intent.snapshot_id.clone(),
                    reason: "same-region snapshot members span multiple active regions".into(),
                });
                continue;
            }
            if regions
                .iter()
                .any(|region| !self.state.regions[region].online)
            {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: intent.snapshot_id.clone(),
                    reason: "snapshot source region is offline".into(),
                });
                continue;
            }
            let mut candidate = scratch.clone();
            let mut reservations = Vec::new();
            let mut tasks = Vec::new();
            let mut feasible = true;
            for placement in &placements {
                let leg = &placement.legs[0];
                let volume = &self.state.volumes[&placement.volume_id].spec;
                if candidate
                    .reserve(
                        &volume.volume_id,
                        &leg.host_id,
                        leg.lane,
                        intent.operation_iops_per_volume,
                        0,
                        IsolationRequirement::Shared,
                    )
                    .is_err()
                {
                    feasible = false;
                    break;
                }
                reservations.push(ResourceReservation {
                    volume_id: volume.volume_id.clone(),
                    host_id: leg.host_id.clone(),
                    lane: leg.lane,
                    iops: intent.operation_iops_per_volume,
                    bytes: 0,
                    isolation: IsolationRequirement::Shared,
                    lifetime: ReservationLifetime::Gang,
                });
                let quiesce = format!("quiesce:{}", volume.volume_id);
                tasks.push(GangTask {
                    task_id: quiesce.clone(),
                    worker_id: leg.host_id.clone(),
                    kind: GangTaskKind::Quiesce,
                    depends_on: BTreeSet::new(),
                });
                tasks.push(GangTask {
                    task_id: format!("cut:{}", volume.volume_id),
                    worker_id: leg.host_id.clone(),
                    kind: GangTaskKind::CaptureCut,
                    depends_on: BTreeSet::from([quiesce.clone()]),
                });
            }
            if !feasible {
                decisions.push(SchedulerDecision::Deferred {
                    subject_id: intent.snapshot_id.clone(),
                    reason: "snapshot operation budget would violate a lane guarantee".into(),
                });
                continue;
            }
            *scratch = candidate;
            let cuts = intent
                .volume_ids
                .iter()
                .map(|volume| (volume.clone(), self.state.volumes[volume].spec.latest_hwm))
                .collect();
            let coordinator = placements
                .iter()
                .flat_map(|placement| placement.legs.iter().map(|leg| leg.host_id.clone()))
                .min()
                .expect("snapshot has members");
            let cut_tasks = tasks
                .iter()
                .filter(|task| task.kind == GangTaskKind::CaptureCut)
                .map(|task| task.task_id.clone())
                .collect();
            tasks.push(GangTask {
                task_id: format!("manifest:{}", intent.snapshot_id),
                worker_id: coordinator,
                kind: GangTaskKind::PublishManifest,
                depends_on: cut_tasks,
            });
            let applications = intent
                .volume_ids
                .iter()
                .map(|volume| self.state.volumes[volume].application_id.clone())
                .collect();
            let record = SnapshotRecord {
                snapshot_id: intent.snapshot_id.clone(),
                scope: intent.scope,
                application_consistent: intent.application_consistent,
                cuts,
                regions,
                committed_at_ns: 0,
            };
            let scheduled_flows = terminal_scheduled_flows(
                &reservations,
                &format!("snapshot:{}", intent.snapshot_id),
            );
            self.state.pending_snapshots.remove(&intent.snapshot_id);
            let plan = finish_plan(
                GangKind::ConsistencySnapshot,
                basis,
                now_ns,
                applications,
                intent.volume_ids.clone(),
                reservations,
                tasks,
                scheduled_flows,
                None,
                GangEffect::Snapshot {
                    intent: intent.clone(),
                    record,
                },
                format!(
                    "capture {}-volume {:?} consistency cut with maximum_hitch_ns={}",
                    intent.volume_ids.len(),
                    intent.scope,
                    intent.maximum_hitch_ns
                ),
            )?;
            self.insert_plan(plan, decisions)?;
        }
        Ok(())
    }

    fn eligible_recovery_regions(&self, volumes: &[VolumeSpec], source: &str) -> BTreeSet<String> {
        let mut eligible = self
            .state
            .regions
            .iter()
            .filter(|(id, region)| id.as_str() != source && region.online)
            .map(|(id, _)| id.clone())
            .collect::<BTreeSet<_>>();
        for volume in volumes {
            let unavailable_regions = self
                .state
                .regions
                .iter()
                .filter(|(_, region)| !region.online)
                .map(|(region_id, _)| region_id.clone())
                .collect::<BTreeSet<_>>();
            match effective_failover_action(&volume.recovery, &unavailable_regions) {
                Ok(Some(FailoverAction::RestoreTo { region_id })) => {
                    eligible.retain(|candidate| candidate == region_id);
                }
                Ok(Some(FailoverAction::HoldDurably)) | Err(_) => eligible.clear(),
                Ok(None) => {}
            }
            if !volume.recovery.allowed_regions.is_empty() {
                eligible = eligible
                    .intersection(&volume.recovery.allowed_regions)
                    .cloned()
                    .collect();
            }
            eligible.retain(|region| {
                self.state
                    .replicas
                    .get(&volume.volume_id)
                    .and_then(|regions| regions.get(region))
                    .is_some_and(|replica| {
                        volume.latest_hwm.saturating_sub(replica.durable_hwm)
                            <= volume.recovery.rpo_max_missing_operations
                    })
            });
        }
        eligible
    }

    fn estimate_recovery_timing(
        &self,
        volumes: &[VolumeSpec],
        region: &str,
        now_ns: u64,
        deadline_ns: u64,
        schedule: &RecoveryScheduleScratch,
    ) -> Option<RecoveryTiming> {
        let restore_bps = self
            .state
            .hosts
            .values()
            .filter(|host| host.online && host.spec.region_id == region)
            .map(|host| host.spec.restore_bytes_per_second)
            .sum::<u64>();
        let apply_iops = self
            .state
            .hosts
            .values()
            .filter(|host| host.online && host.spec.region_id == region)
            .map(|host| {
                host.spec
                    .lane_iops
                    .saturating_mul(u64::from(host.spec.lanes))
            })
            .sum::<u64>();
        if restore_bps == 0 || apply_iops == 0 {
            return None;
        }
        let required_recovery_iops = volumes
            .iter()
            .map(|volume| volume.recovery.minimum_recovery_iops)
            .sum::<u64>();
        if required_recovery_iops > apply_iops {
            return None;
        }
        let mut restore_bytes = 0u128;
        let mut replay_ops = 0u128;
        let mut path_work = BTreeMap::<String, (u64, u128, u128, u64, u64, u8, u64)>::new();
        let mut transports = BTreeSet::new();
        let mut zero_copy_paths = BTreeSet::new();
        let mut copying_paths = BTreeSet::new();
        let mut estimated_payload_bytes_copied = 0u128;
        for volume in volumes {
            let replica = self
                .state
                .replicas
                .get(&volume.volume_id)
                .and_then(|regions| regions.get(region))?;
            if let Some(backlog) = self
                .state
                .mux_backlogs
                .get(&volume.volume_id)
                .and_then(|regions| regions.get(region))
            {
                restore_bytes = restore_bytes.saturating_add(u128::from(backlog.bytes_pending));
                replay_ops = replay_ops.saturating_add(u128::from(backlog.operations_pending));
                for layer in &backlog.layers {
                    transports.insert(layer.transport);
                    if layer.copy_passes == 0 {
                        zero_copy_paths.insert(layer.path_id.clone());
                    } else {
                        copying_paths.insert(layer.path_id.clone());
                        estimated_payload_bytes_copied = estimated_payload_bytes_copied
                            .saturating_add(
                                u128::from(backlog.bytes_pending)
                                    .saturating_mul(u128::from(layer.copy_passes)),
                            );
                    }
                    path_work
                        .entry(layer.path_id.clone())
                        .and_modify(|work| {
                            work.0 = work.0.max(layer.fixed_fill_latency_ns);
                            work.1 = work.1.saturating_add(u128::from(backlog.bytes_pending));
                            work.2 = work
                                .2
                                .saturating_add(u128::from(backlog.operations_pending));
                            work.3 = work.3.min(layer.demux_bytes_per_second);
                            work.4 = work.4.min(layer.demux_operations_per_second);
                            work.5 = work.5.max(layer.copy_passes);
                            if layer.copy_passes > 0 {
                                work.6 = if work.6 == 0 {
                                    layer.copy_bytes_per_second
                                } else {
                                    work.6.min(layer.copy_bytes_per_second)
                                };
                            }
                        })
                        .or_insert((
                            layer.fixed_fill_latency_ns,
                            u128::from(backlog.bytes_pending),
                            u128::from(backlog.operations_pending),
                            layer.demux_bytes_per_second,
                            layer.demux_operations_per_second,
                            layer.copy_passes,
                            layer.copy_bytes_per_second,
                        ));
                }
            } else {
                if replica.applied_hwm == 0 {
                    restore_bytes = restore_bytes.saturating_add(u128::from(volume.bytes));
                }
                replay_ops = replay_ops.saturating_add(u128::from(
                    volume.latest_hwm.saturating_sub(replica.applied_hwm),
                ));
            }
        }
        let copy_ns = div_ceil(
            restore_bytes.saturating_mul(NANOS_PER_SECOND),
            u128::from(restore_bps),
        );
        let replay_ns = div_ceil(
            replay_ops.saturating_mul(NANOS_PER_SECOND),
            u128::from(apply_iops),
        );
        let target_materialization_ns = u64::try_from(copy_ns.max(replay_ns)).unwrap_or(u64::MAX);
        let mut pipeline_fill_ns = 0u64;
        let mut demux_transfer_ns = 0u64;
        let mut mux_queue_ready_ns = now_ns;
        let mux_path_ids = path_work.keys().cloned().collect::<BTreeSet<_>>();
        for (
            path_id,
            (
                fixed,
                bytes,
                operations,
                bytes_per_second,
                operations_per_second,
                copy_passes,
                copy_bytes_per_second,
            ),
        ) in &path_work
        {
            pipeline_fill_ns = pipeline_fill_ns.saturating_add(*fixed);
            let bytes_ns = div_ceil(
                bytes.saturating_mul(NANOS_PER_SECOND),
                u128::from(*bytes_per_second),
            );
            let operations_ns = div_ceil(
                operations.saturating_mul(NANOS_PER_SECOND),
                u128::from(*operations_per_second),
            );
            let copy_ns = if *copy_passes == 0 {
                0
            } else {
                div_ceil(
                    bytes
                        .saturating_mul(u128::from(*copy_passes))
                        .saturating_mul(NANOS_PER_SECOND),
                    u128::from(*copy_bytes_per_second),
                )
            };
            demux_transfer_ns = demux_transfer_ns
                .max(u64::try_from(bytes_ns.max(operations_ns).max(copy_ns)).unwrap_or(u64::MAX));
            mux_queue_ready_ns = mux_queue_ready_ns.max(
                schedule
                    .mux_ready_ns
                    .get(path_id)
                    .copied()
                    .unwrap_or(now_ns),
            );
        }
        let target_queue_ready_ns = schedule
            .target_ready_ns
            .get(region)
            .copied()
            .unwrap_or(now_ns);
        let queue_start_ns = now_ns.max(target_queue_ready_ns).max(mux_queue_ready_ns);
        let estimated_completion_ns = queue_start_ns
            .saturating_add(pipeline_fill_ns)
            .saturating_add(demux_transfer_ns.max(target_materialization_ns));
        Some(RecoveryTiming {
            target_region_id: region.to_string(),
            target_queue_ready_ns,
            mux_queue_ready_ns,
            queue_start_ns,
            pipeline_fill_ns,
            demux_transfer_ns,
            target_materialization_ns,
            estimated_completion_ns,
            deadline_ns,
            rto_met: estimated_completion_ns <= deadline_ns,
            mux_path_ids,
            transports,
            zero_copy_lane_count: u64::try_from(zero_copy_paths.len()).unwrap_or(u64::MAX),
            copying_lane_count: u64::try_from(copying_paths.len()).unwrap_or(u64::MAX),
            estimated_payload_bytes_copied,
        })
    }

    fn allocate_volume(
        &self,
        volume: &VolumeSpec,
        region: &str,
        placement_epoch: u64,
        scratch: &mut ResourceScratch,
    ) -> io::Result<(VolumePlacement, Vec<ResourceReservation>)> {
        if !self
            .state
            .regions
            .get(region)
            .is_some_and(|runtime| runtime.online)
        {
            return Err(invalid(format!("region {region} is not online")));
        }
        let mut legs = Vec::new();
        let mut reservations = Vec::new();
        let mut used_domains = BTreeSet::new();
        for _ in 0..volume.storage_copies {
            let mut candidates = Vec::new();
            for (host_id, host) in &self.state.hosts {
                if !host.online
                    || host.spec.region_id != region
                    || used_domains.contains(&host.spec.failure_domain)
                {
                    continue;
                }
                for lane in 0..host.spec.lanes {
                    if scratch.can_reserve(
                        &volume.volume_id,
                        host_id,
                        lane,
                        volume.provisioned_iops,
                        volume.bytes,
                        volume.isolation,
                    ) {
                        let use_state = &scratch.hosts[host_id];
                        let lane_use = &use_state.lanes[usize::from(lane)];
                        let utilization_ppm = if host.spec.lane_iops == 0 {
                            u64::MAX
                        } else {
                            lane_use
                                .iops
                                .saturating_mul(1_000_000)
                                .saturating_div(host.spec.lane_iops)
                        };
                        candidates.push((utilization_ppm, host_id.clone(), lane));
                    }
                }
            }
            candidates.sort();
            let Some((_, host_id, lane)) = candidates.into_iter().next() else {
                return Err(invalid(format!(
                    "no distinct host/lane in {region} can place volume {} copies={} iops={} bytes={}",
                    volume.volume_id, volume.storage_copies, volume.provisioned_iops, volume.bytes
                )));
            };
            scratch.reserve(
                &volume.volume_id,
                &host_id,
                lane,
                volume.provisioned_iops,
                volume.bytes,
                volume.isolation,
            )?;
            let failure_domain = self.state.hosts[&host_id].spec.failure_domain.clone();
            used_domains.insert(failure_domain.clone());
            legs.push(PlacementLeg {
                host_id: host_id.clone(),
                lane,
                failure_domain,
            });
            reservations.push(ResourceReservation {
                volume_id: volume.volume_id.clone(),
                host_id,
                lane,
                iops: volume.provisioned_iops,
                bytes: volume.bytes,
                isolation: volume.isolation,
                lifetime: ReservationLifetime::Persistent,
            });
        }
        Ok((
            VolumePlacement {
                volume_id: volume.volume_id.clone(),
                region_id: region.to_string(),
                placement_epoch,
                legs,
            },
            reservations,
        ))
    }

    fn build_place_plan(
        &self,
        kind: GangKind,
        basis: u64,
        now_ns: u64,
        placements: Vec<VolumePlacement>,
        reservations: Vec<ResourceReservation>,
        explanation: String,
        recovery_timing: Option<RecoveryTiming>,
    ) -> io::Result<GangPlan> {
        let mut applications = BTreeSet::new();
        let mut volumes = BTreeSet::new();
        let mut tasks = Vec::new();
        let mut scheduled_flows = Vec::new();
        for placement in &placements {
            volumes.insert(placement.volume_id.clone());
            applications.insert(
                self.state.volumes[&placement.volume_id]
                    .application_id
                    .clone(),
            );
            let volume = &self.state.volumes[&placement.volume_id].spec;
            let mut flow_lanes = Vec::new();
            if let Some(timing) = &recovery_timing
                && let Some(backlog) = self
                    .state
                    .mux_backlogs
                    .get(&placement.volume_id)
                    .and_then(|regions| regions.get(&timing.target_region_id))
            {
                flow_lanes.extend(backlog.layers.iter().map(|layer| FlowLaneAssignment {
                    lane_id: layer.path_id.clone(),
                    role: layer.role,
                    transport: layer.transport,
                    copy_passes: layer.copy_passes,
                }));
            }
            flow_lanes.extend(placement.legs.iter().map(|leg| FlowLaneAssignment {
                lane_id: format!("leaf:{}:{}", leg.host_id, leg.lane),
                role: FlowLaneRole::TerminalLeaf,
                transport: FlowTransport::TerminalUserspaceIo,
                copy_passes: 0,
            }));
            scheduled_flows.push(ScheduledFlow {
                flow_id: format!(
                    "{}:epoch:{}",
                    placement.volume_id, placement.placement_epoch
                ),
                volume_id: placement.volume_id.clone(),
                guaranteed_iops: volume.provisioned_iops,
                lanes: flow_lanes,
            });
            let mut prepared = BTreeSet::new();
            for leg in &placement.legs {
                let task_id = format!(
                    "prepare:{}:{}:{}",
                    placement.volume_id, leg.host_id, leg.lane
                );
                tasks.push(GangTask {
                    task_id: task_id.clone(),
                    worker_id: leg.host_id.clone(),
                    kind: GangTaskKind::PrepareRoute,
                    depends_on: BTreeSet::new(),
                });
                prepared.insert(task_id);
                if kind == GangKind::RegionalFailover {
                    let restore = format!("restore:{}:{}", placement.volume_id, leg.host_id);
                    tasks.push(GangTask {
                        task_id: restore.clone(),
                        worker_id: leg.host_id.clone(),
                        kind: GangTaskKind::Restore,
                        depends_on: prepared.clone(),
                    });
                    prepared.insert(restore);
                }
            }
            if kind == GangKind::RegionalFailover {
                tasks.push(GangTask {
                    task_id: format!("fence:{}", placement.volume_id),
                    worker_id: format!("controller:{}", placement.region_id),
                    kind: GangTaskKind::FenceSource,
                    depends_on: prepared.clone(),
                });
            }
            tasks.push(GangTask {
                task_id: format!(
                    "activate:{}:{}",
                    placement.volume_id, placement.placement_epoch
                ),
                worker_id: format!("controller:{}", placement.region_id),
                kind: GangTaskKind::Activate,
                depends_on: prepared,
            });
        }
        finish_plan(
            kind,
            basis,
            now_ns,
            applications,
            volumes,
            reservations,
            tasks,
            scheduled_flows,
            recovery_timing,
            GangEffect::Place { placements },
            explanation,
        )
    }

    fn insert_plan(
        &mut self,
        plan: GangPlan,
        decisions: &mut Vec<SchedulerDecision>,
    ) -> io::Result<()> {
        if self.state.gangs.contains_key(&plan.plan_id) {
            return Err(invalid("deterministic gang id collision"));
        }
        self.state.gangs.insert(
            plan.plan_id.clone(),
            GangRuntime {
                plan: plan.clone(),
                phase: GangPhase::Preparing,
            },
        );
        decisions.push(SchedulerDecision::PrepareGang { plan });
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
struct RecoveryScheduleScratch {
    target_ready_ns: BTreeMap<String, u64>,
    mux_ready_ns: BTreeMap<String, u64>,
}

impl RecoveryScheduleScratch {
    fn from_state(state: &EstateState) -> Self {
        let mut scratch = Self::default();
        for timing in state
            .gangs
            .values()
            .filter(|gang| matches!(gang.phase, GangPhase::Preparing | GangPhase::Prepared))
            .filter_map(|gang| gang.plan.recovery_timing.as_ref())
        {
            scratch
                .target_ready_ns
                .entry(timing.target_region_id.clone())
                .and_modify(|ready| *ready = (*ready).max(timing.estimated_completion_ns))
                .or_insert(timing.estimated_completion_ns);
            for path_id in &timing.mux_path_ids {
                scratch
                    .mux_ready_ns
                    .entry(path_id.clone())
                    .and_modify(|ready| *ready = (*ready).max(timing.estimated_completion_ns))
                    .or_insert(timing.estimated_completion_ns);
            }
        }
        scratch
    }

    fn reserve(&mut self, timing: &RecoveryTiming) {
        self.target_ready_ns.insert(
            timing.target_region_id.clone(),
            timing.estimated_completion_ns,
        );
        for path_id in &timing.mux_path_ids {
            self.mux_ready_ns
                .insert(path_id.clone(), timing.estimated_completion_ns);
        }
    }
}

#[derive(Clone, Default)]
struct LaneUse {
    iops: u64,
    volumes: BTreeSet<String>,
    dedicated_owner: Option<String>,
}

#[derive(Clone)]
struct HostUse {
    bytes: u64,
    lanes: Vec<LaneUse>,
}

#[derive(Clone, Default)]
struct ResourceScratch {
    hosts: BTreeMap<String, HostUse>,
    specs: BTreeMap<String, HostSpec>,
}

impl ResourceScratch {
    fn from_state(state: &EstateState) -> io::Result<Self> {
        let mut scratch = Self::default();
        for (host_id, host) in &state.hosts {
            scratch.specs.insert(host_id.clone(), host.spec.clone());
            scratch.hosts.insert(
                host_id.clone(),
                HostUse {
                    bytes: 0,
                    lanes: vec![LaneUse::default(); usize::from(host.spec.lanes)],
                },
            );
        }
        for (volume_id, placement) in &state.placements {
            let volume = &state.volumes[volume_id].spec;
            for leg in &placement.legs {
                scratch.reserve(
                    volume_id,
                    &leg.host_id,
                    leg.lane,
                    volume.provisioned_iops,
                    volume.bytes,
                    volume.isolation,
                )?;
            }
        }
        for gang in state
            .gangs
            .values()
            .filter(|gang| matches!(gang.phase, GangPhase::Preparing | GangPhase::Prepared))
        {
            for reservation in &gang.plan.reservations {
                scratch.reserve(
                    &reservation.volume_id,
                    &reservation.host_id,
                    reservation.lane,
                    reservation.iops,
                    reservation.bytes,
                    reservation.isolation,
                )?;
            }
        }
        Ok(scratch)
    }

    fn can_reserve(
        &self,
        volume_id: &str,
        host_id: &str,
        lane: u16,
        iops: u64,
        bytes: u64,
        isolation: IsolationRequirement,
    ) -> bool {
        let Some(spec) = self.specs.get(host_id) else {
            return false;
        };
        let Some(host) = self.hosts.get(host_id) else {
            return false;
        };
        let Some(lane_use) = host.lanes.get(usize::from(lane)) else {
            return false;
        };
        if host.bytes.saturating_add(bytes) > spec.capacity_bytes
            || lane_use.iops.saturating_add(iops) > spec.lane_iops
        {
            return false;
        }
        match isolation {
            IsolationRequirement::Dedicated => {
                lane_use.volumes.is_empty()
                    || (lane_use.volumes.len() == 1 && lane_use.volumes.contains(volume_id))
            }
            IsolationRequirement::Shared | IsolationRequirement::Reserved => lane_use
                .dedicated_owner
                .as_ref()
                .is_none_or(|owner| owner == volume_id),
        }
    }

    fn reserve(
        &mut self,
        volume_id: &str,
        host_id: &str,
        lane: u16,
        iops: u64,
        bytes: u64,
        isolation: IsolationRequirement,
    ) -> io::Result<()> {
        if !self.can_reserve(volume_id, host_id, lane, iops, bytes, isolation) {
            return Err(invalid(format!(
                "resource reservation exceeds host/lane capacity host={host_id} lane={lane} volume={volume_id}"
            )));
        }
        let host = self.hosts.get_mut(host_id).expect("validated host");
        host.bytes = host.bytes.saturating_add(bytes);
        let lane_use = &mut host.lanes[usize::from(lane)];
        lane_use.iops = lane_use.iops.saturating_add(iops);
        lane_use.volumes.insert(volume_id.to_string());
        if isolation == IsolationRequirement::Dedicated {
            lane_use.dedicated_owner = Some(volume_id.to_string());
        }
        Ok(())
    }
}

/// Resolves every task in a gang as one modeled parallel preparation event.
/// It starts no workers and moves no bytes; tests may reject selected workers
/// to exercise deterministic unwind and replanning.
#[derive(Clone, Debug, Default)]
pub struct ModeledGangExecutor {
    rejected_workers: BTreeMap<String, String>,
}

impl ModeledGangExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reject_worker(&mut self, worker_id: impl Into<String>, reason: impl Into<String>) {
        self.rejected_workers
            .insert(worker_id.into(), reason.into());
    }

    pub fn clear_rejections(&mut self) {
        self.rejected_workers.clear();
    }

    pub fn responses(&self, decisions: &[SchedulerDecision]) -> Vec<EstateEvent> {
        let mut responses = Vec::new();
        for decision in decisions {
            match decision {
                SchedulerDecision::PrepareGang { plan } => {
                    let rejection = plan.tasks.iter().find_map(|task| {
                        self.rejected_workers
                            .get(&task.worker_id)
                            .map(|reason| (task.worker_id.clone(), reason.clone()))
                    });
                    if let Some((worker_id, reason)) = rejection {
                        responses.push(EstateEvent::GangRejected {
                            plan_id: plan.plan_id.clone(),
                            worker_id,
                            reason,
                        });
                    } else {
                        responses.push(EstateEvent::GangPrepared {
                            plan_id: plan.plan_id.clone(),
                            task_ids: plan.tasks.iter().map(|task| task.task_id.clone()).collect(),
                        });
                    }
                }
                SchedulerDecision::CommitGang { plan_id } => {
                    responses.push(EstateEvent::GangCommitted {
                        plan_id: plan_id.clone(),
                    });
                }
                SchedulerDecision::AbortGang { .. } | SchedulerDecision::Deferred { .. } => {}
            }
        }
        responses
    }
}

fn terminal_scheduled_flows(
    reservations: &[ResourceReservation],
    flow_prefix: &str,
) -> Vec<ScheduledFlow> {
    let mut by_volume = BTreeMap::<String, (u64, Vec<FlowLaneAssignment>)>::new();
    for reservation in reservations {
        let entry = by_volume
            .entry(reservation.volume_id.clone())
            .or_insert_with(|| (0, Vec::new()));
        entry.0 = entry.0.max(reservation.iops);
        entry.1.push(FlowLaneAssignment {
            lane_id: format!("leaf:{}:{}", reservation.host_id, reservation.lane),
            role: FlowLaneRole::TerminalLeaf,
            transport: FlowTransport::TerminalUserspaceIo,
            copy_passes: 0,
        });
    }
    by_volume
        .into_iter()
        .map(|(volume_id, (guaranteed_iops, mut lanes))| {
            lanes.sort_by(|left, right| left.lane_id.cmp(&right.lane_id));
            ScheduledFlow {
                flow_id: format!("{flow_prefix}:{volume_id}"),
                volume_id,
                guaranteed_iops,
                lanes,
            }
        })
        .collect()
}

fn finish_plan(
    kind: GangKind,
    basis: u64,
    now_ns: u64,
    application_ids: BTreeSet<String>,
    volume_ids: BTreeSet<String>,
    mut reservations: Vec<ResourceReservation>,
    mut tasks: Vec<GangTask>,
    mut scheduled_flows: Vec<ScheduledFlow>,
    recovery_timing: Option<RecoveryTiming>,
    effect: GangEffect,
    explanation: String,
) -> io::Result<GangPlan> {
    reservations.sort_by(|left, right| {
        (
            &left.host_id,
            left.lane,
            &left.volume_id,
            left.iops,
            left.bytes,
        )
            .cmp(&(
                &right.host_id,
                right.lane,
                &right.volume_id,
                right.iops,
                right.bytes,
            ))
    });
    tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    scheduled_flows.sort_by(|left, right| left.flow_id.cmp(&right.flow_id));
    let identity = (
        kind,
        basis,
        &application_ids,
        &volume_ids,
        &reservations,
        &tasks,
        &scheduled_flows,
        &recovery_timing,
        &effect,
    );
    let hash = stable_hash(&identity)?;
    Ok(GangPlan {
        plan_id: format!("gang-{}", &hash[7..31]),
        kind,
        basis_event_index: basis,
        effective_at_ns: now_ns,
        application_ids,
        volume_ids,
        reservations,
        tasks,
        scheduled_flows,
        recovery_timing,
        effect,
        explanation,
    })
}

fn stable_hash<T: Serialize>(value: &T) -> io::Result<String> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| invalid(format!("serialize deterministic scheduler value: {error}")))?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
}

fn div_ceil(numerator: u128, denominator: u128) -> u128 {
    if numerator == 0 {
        0
    } else {
        1 + (numerator - 1) / denominator
    }
}

fn effective_failover_action<'a>(
    objective: &'a RecoveryObjective,
    unavailable_regions: &BTreeSet<String>,
) -> Result<Option<&'a FailoverAction>, String> {
    let matching = objective
        .preapproved_failover
        .iter()
        .filter(|rule| rule.unavailable_regions.is_subset(unavailable_regions))
        .collect::<Vec<_>>();
    let Some(maximum_specificity) = matching
        .iter()
        .map(|rule| rule.unavailable_regions.len())
        .max()
    else {
        return Ok(None);
    };
    let most_specific = matching
        .into_iter()
        .filter(|rule| rule.unavailable_regions.len() == maximum_specificity)
        .collect::<Vec<_>>();
    let action = &most_specific[0].action;
    if most_specific.iter().any(|rule| &rule.action != action) {
        return Err(format!(
            "{} rules of specificity {} resolve to different actions",
            most_specific.len(),
            maximum_specificity
        ));
    }
    Ok(Some(action))
}

fn effective_scenario_impact<'a>(
    application: &'a ApplicationSpec,
    unavailable_regions: &BTreeSet<String>,
) -> Result<Option<&'a ScenarioBusinessImpactRule>, String> {
    let matching = application
        .scenario_impacts
        .iter()
        .filter(|rule| rule.unavailable_regions.is_subset(unavailable_regions))
        .collect::<Vec<_>>();
    let Some(maximum_specificity) = matching
        .iter()
        .map(|rule| rule.unavailable_regions.len())
        .max()
    else {
        return Ok(None);
    };
    let most_specific = matching
        .into_iter()
        .filter(|rule| rule.unavailable_regions.len() == maximum_specificity)
        .collect::<Vec<_>>();
    let selected = most_specific[0];
    if most_specific
        .iter()
        .any(|rule| rule.rto_ns != selected.rto_ns || rule.impact != selected.impact)
    {
        return Err(format!(
            "{} scenario-impact rules of specificity {} disagree",
            most_specific.len(),
            maximum_specificity
        ));
    }
    Ok(Some(selected))
}

fn business_impact_floor(impact: &BusinessImpactEstimate) -> u128 {
    u128::from(impact.rto_breach_cost_microunits)
        .saturating_add(u128::from(impact.downtime_cost_microunits_per_second))
        .saturating_add(u128::from(impact.lost_operation_cost_microunits))
}

fn estimated_recovery_impact(
    impact: &BusinessImpactEstimate,
    rto_ns: u64,
    missing_operations: u64,
) -> u128 {
    let rto_seconds = div_ceil(u128::from(rto_ns), NANOS_PER_SECOND);
    u128::from(impact.rto_breach_cost_microunits)
        .saturating_add(
            u128::from(impact.downtime_cost_microunits_per_second).saturating_mul(rto_seconds),
        )
        .saturating_add(
            u128::from(impact.lost_operation_cost_microunits)
                .saturating_mul(u128::from(missing_operations)),
        )
}

fn validate_business_impact(impact: &BusinessImpactEstimate) -> io::Result<()> {
    if impact.downtime_cost_microunits_per_second == 0
        && impact.rto_breach_cost_microunits == 0
        && impact.lost_operation_cost_microunits == 0
    {
        return Err(invalid("business impact estimate cannot be entirely zero"));
    }
    Ok(())
}

fn validate_scenario_impacts(rules: &[ScenarioBusinessImpactRule]) -> io::Result<()> {
    let mut triggers = BTreeSet::new();
    for rule in rules {
        if rule.unavailable_regions.is_empty()
            || rule.rto_ns == 0
            || !triggers.insert(rule.unavailable_regions.clone())
        {
            return Err(invalid(
                "scenario-impact rules need unique non-empty failure sets and nonzero RTOs",
            ));
        }
        for region in &rule.unavailable_regions {
            validate_id(region, "scenario-impact unavailable region")?;
        }
        validate_business_impact(&rule.impact)?;
    }
    Ok(())
}

fn validate_region(spec: &RegionSpec) -> io::Result<()> {
    validate_id(&spec.region_id, "region_id")?;
    validate_id(&spec.trust_domain, "trust_domain")
}

fn validate_host(spec: &HostSpec) -> io::Result<()> {
    validate_id(&spec.host_id, "host_id")?;
    validate_id(&spec.region_id, "region_id")?;
    validate_id(&spec.failure_domain, "failure_domain")?;
    if spec.capacity_bytes == 0
        || spec.lanes == 0
        || spec.lane_iops == 0
        || spec.restore_bytes_per_second == 0
    {
        return Err(invalid(
            "host capacity, lanes, IOPS, and restore rate must be nonzero",
        ));
    }
    Ok(())
}

fn validate_volume(spec: &VolumeSpec) -> io::Result<()> {
    validate_id(&spec.volume_id, "volume_id")?;
    validate_id(&spec.home_region_id, "home_region_id")?;
    if spec.bytes == 0
        || spec.provisioned_iops == 0
        || spec.storage_copies == 0
        || spec.recovery.minimum_recovery_iops == 0
    {
        return Err(invalid(
            "volume bytes, IOPS, copies, and recovery IOPS must be nonzero",
        ));
    }
    if let Some(group) = &spec.recovery_group_id {
        validate_id(group, "recovery_group_id")?;
    }
    for region in &spec.recovery.allowed_regions {
        validate_id(region, "allowed recovery region")?;
    }
    let mut triggers = BTreeSet::new();
    for rule in &spec.recovery.preapproved_failover {
        if rule.unavailable_regions.is_empty() || !triggers.insert(rule.unavailable_regions.clone())
        {
            return Err(invalid("failover rules need unique non-empty failure sets"));
        }
        for region in &rule.unavailable_regions {
            validate_id(region, "failover unavailable region")?;
        }
        if let FailoverAction::RestoreTo { region_id } = &rule.action {
            validate_id(region_id, "failover target region")?;
            if !spec.recovery.allowed_regions.is_empty()
                && !spec.recovery.allowed_regions.contains(region_id)
            {
                return Err(invalid(
                    "pre-approved target is outside allowed recovery regions",
                ));
            }
        }
    }
    Ok(())
}

fn validate_id(value: &str, label: &str) -> io::Result<()> {
    if value.is_empty() || value.contains('\0') || value.contains('\n') {
        return Err(invalid(format!("invalid {label}")));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const TIB: u64 = 1024 * GIB;
    const SECOND: u64 = 1_000_000_000;
    const HWM: u64 = 1_000_000;

    struct Simulation {
        scheduler: GangScheduler,
        executor: ModeledGangExecutor,
        config: GangSchedulerConfig,
        index: u64,
        now_ns: u64,
        log: Vec<EstateEventEnvelope>,
        outputs: Vec<Vec<SchedulerDecision>>,
    }

    impl Simulation {
        fn new(strategy: ConcurrentSchedulingStrategy) -> Self {
            let config = GangSchedulerConfig { strategy };
            Self {
                scheduler: GangScheduler::with_config(config),
                executor: ModeledGangExecutor::new(),
                config,
                index: 0,
                now_ns: 0,
                log: Vec::new(),
                outputs: Vec::new(),
            }
        }

        fn emit_at(&mut self, timestamp_ns: u64, event: EstateEvent) -> Vec<SchedulerDecision> {
            assert!(timestamp_ns >= self.now_ns);
            self.index += 1;
            self.now_ns = timestamp_ns;
            let envelope = EstateEventEnvelope::new(self.index, timestamp_ns, event);
            let output = self.scheduler.apply(envelope.clone()).unwrap();
            self.log.push(envelope);
            self.outputs.push(output.clone());
            output
        }

        fn emit(&mut self, event: EstateEvent) -> Vec<SchedulerDecision> {
            self.emit_at(self.now_ns.saturating_add(1), event)
        }

        fn settle(&mut self, mut frontier: Vec<SchedulerDecision>) -> Vec<SchedulerDecision> {
            let mut all = frontier.clone();
            loop {
                let responses = self.executor.responses(&frontier);
                if responses.is_empty() {
                    break;
                }
                frontier.clear();
                for response in responses {
                    let output = self.emit(response);
                    all.extend(output.clone());
                    frontier.extend(output);
                }
            }
            all
        }

        fn act(&mut self, event: EstateEvent) -> Vec<SchedulerDecision> {
            let frontier = self.emit(event);
            self.settle(frontier)
        }

        fn plan_unsettled(&mut self) -> Vec<SchedulerDecision> {
            self.emit(EstateEvent::PlanAtWatermark {
                input_watermark: self.index,
            })
        }

        fn plan(&mut self) -> Vec<SchedulerDecision> {
            let frontier = self.plan_unsettled();
            self.settle(frontier)
        }

        fn assert_replays_exactly(&self) {
            let mut replay = GangScheduler::with_config(self.config);
            for (envelope, expected) in self.log.iter().cloned().zip(&self.outputs) {
                assert_eq!(&replay.apply(envelope).unwrap(), expected);
            }
            assert_eq!(
                replay.state_hash().unwrap(),
                self.scheduler.state_hash().unwrap()
            );
            assert_eq!(replay.audit(), self.scheduler.audit());
        }
    }

    fn recovery(
        rto_seconds: Option<u64>,
        rpo_operations: u64,
        allowed_regions: &[&str],
    ) -> RecoveryObjective {
        RecoveryObjective {
            rpo_max_missing_operations: rpo_operations,
            rto_ns: rto_seconds.map(|seconds| seconds * SECOND),
            scenarios: BTreeSet::from([
                FailureScenario::HostLoss,
                FailureScenario::AvailabilityZoneLoss,
                FailureScenario::RegionLoss,
            ]),
            minimum_recovery_iops: 10_000,
            allowed_regions: allowed_regions
                .iter()
                .map(|region| (*region).to_string())
                .collect(),
            preapproved_failover: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn volume(
        volume_id: impl Into<String>,
        home_region_id: &str,
        bytes: u64,
        iops: u64,
        copies: u8,
        durability_owner: DurabilityOwner,
        isolation: IsolationRequirement,
        recovery_group_id: Option<String>,
        rto_seconds: Option<u64>,
        rpo_operations: u64,
        allowed_regions: &[&str],
    ) -> VolumeSpec {
        VolumeSpec {
            volume_id: volume_id.into(),
            home_region_id: home_region_id.to_string(),
            bytes,
            provisioned_iops: iops,
            latest_hwm: HWM,
            durability_owner,
            storage_copies: copies,
            isolation,
            recovery_group_id,
            recovery: recovery(rto_seconds, rpo_operations, allowed_regions),
        }
    }

    fn host_spec(region: &str, ordinal: usize, capacity_bytes: u64, lanes: u16) -> HostSpec {
        HostSpec {
            host_id: format!("{region}-h{ordinal}"),
            region_id: region.to_string(),
            failure_domain: format!("{region}-az{ordinal}"),
            capacity_bytes,
            lanes,
            lane_iops: 2_000_000,
            restore_bytes_per_second: 20 * GIB,
        }
    }

    fn add_region(
        simulation: &mut Simulation,
        region: &str,
        host_count: usize,
        capacity_bytes: u64,
        lanes: u16,
    ) {
        simulation.act(EstateEvent::PutRegion {
            spec: RegionSpec {
                region_id: region.to_string(),
                trust_domain: format!("trust-{region}"),
            },
        });
        for ordinal in 0..host_count {
            simulation.act(EstateEvent::PutHost {
                spec: host_spec(region, ordinal, capacity_bytes, lanes),
            });
        }
    }

    fn put_app(
        simulation: &mut Simulation,
        id: &str,
        kind: ApplicationKind,
        downtime_cost_units_per_second: u32,
        volumes: Vec<VolumeSpec>,
    ) {
        const TEST_ENTITY: &str = "test-entity";
        let current_regions = simulation
            .scheduler
            .state
            .regions
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let current_entity = simulation
            .scheduler
            .state
            .business_entities
            .get(TEST_ENTITY);
        if current_entity.map(|entity| &entity.region_ids) != Some(&current_regions) {
            simulation.act(EstateEvent::PutBusinessEntity {
                spec: BusinessEntitySpec {
                    business_entity_id: TEST_ENTITY.to_string(),
                    generation: current_entity.map_or(1, |entity| entity.generation + 1),
                    region_ids: current_regions,
                },
            });
        }
        put_app_for_entity(
            simulation,
            TEST_ENTITY,
            id,
            kind,
            downtime_cost_units_per_second,
            volumes,
        );
    }

    fn put_app_for_entity(
        simulation: &mut Simulation,
        business_entity_id: &str,
        id: &str,
        kind: ApplicationKind,
        downtime_cost_units_per_second: u32,
        volumes: Vec<VolumeSpec>,
    ) {
        simulation.act(EstateEvent::PutApplication {
            spec: ApplicationSpec {
                application_id: id.to_string(),
                business_entity_id: business_entity_id.to_string(),
                kind,
                business_impact: BusinessImpactEstimate {
                    downtime_cost_microunits_per_second: u64::from(downtime_cost_units_per_second)
                        * 1_000_000,
                    rto_breach_cost_microunits: u64::from(downtime_cost_units_per_second)
                        * 1_000_000_000,
                    lost_operation_cost_microunits: u64::from(downtime_cost_units_per_second)
                        * 1_000,
                },
                scenario_impacts: Vec::new(),
                volumes,
            },
        });
    }

    #[derive(Clone)]
    struct BusinessIds {
        all: Vec<String>,
        cassandra: Vec<String>,
        cockroach: Vec<String>,
        postgres: Vec<String>,
        sharded: Vec<String>,
        minio: Vec<String>,
        kafka: Vec<String>,
    }

    fn build_business(simulation: &mut Simulation) -> BusinessIds {
        add_region(simulation, "us-east", 4, 4 * TIB, 16);
        add_region(simulation, "us-west", 4, 4 * TIB, 16);
        add_region(simulation, "eu-west", 4, 4 * TIB, 16);

        let cassandra = (0..3)
            .flat_map(|node| (0..2).map(move |disk| format!("cassandra-n{node}-jbod{disk}")))
            .collect::<Vec<_>>();
        put_app(
            simulation,
            "orders-cassandra",
            ApplicationKind::CassandraJbod,
            30,
            cassandra
                .iter()
                .map(|id| {
                    volume(
                        id,
                        "us-east",
                        64 * GIB,
                        180_000,
                        1,
                        DurabilityOwner::Application,
                        IsolationRequirement::Shared,
                        None,
                        None,
                        50_000,
                        &["us-west"],
                    )
                })
                .collect(),
        );

        let cockroach = vec![
            "cockroach-store-us-east".to_string(),
            "cockroach-store-us-west".to_string(),
            "cockroach-store-eu-west".to_string(),
        ];
        let cockroach_homes = ["us-east", "us-west", "eu-west"];
        put_app(
            simulation,
            "ledger-cockroach",
            ApplicationKind::CockroachDb,
            60,
            cockroach
                .iter()
                .zip(cockroach_homes)
                .map(|(id, home)| {
                    volume(
                        id,
                        home,
                        48 * GIB,
                        300_000,
                        1,
                        DurabilityOwner::Application,
                        IsolationRequirement::Reserved,
                        None,
                        None,
                        1_000,
                        &[],
                    )
                })
                .collect(),
        );

        let postgres = vec!["postgres-data".to_string(), "postgres-wal".to_string()];
        put_app(
            simulation,
            "billing-postgres",
            ApplicationKind::Postgres,
            100,
            postgres
                .iter()
                .map(|id| {
                    volume(
                        id,
                        "us-east",
                        if id.ends_with("wal") {
                            16 * GIB
                        } else {
                            96 * GIB
                        },
                        450_000,
                        2,
                        DurabilityOwner::Storage,
                        IsolationRequirement::Dedicated,
                        Some("billing-primary".to_string()),
                        Some(60),
                        0,
                        &["us-west"],
                    )
                })
                .collect(),
        );

        let sharded = (0..4)
            .flat_map(|shard| {
                [
                    format!("quotes-shard{shard}-data"),
                    format!("quotes-shard{shard}-wal"),
                ]
            })
            .collect::<Vec<_>>();
        put_app(
            simulation,
            "quotes-sharded-db",
            ApplicationKind::PerformanceShardedDatabase,
            180,
            sharded
                .iter()
                .map(|id| {
                    let shard = id.split('-').nth(1).unwrap();
                    volume(
                        id,
                        "us-east",
                        32 * GIB,
                        600_000,
                        2,
                        DurabilityOwner::Storage,
                        IsolationRequirement::Reserved,
                        Some(format!("{shard}-pair")),
                        Some(30),
                        0,
                        &["eu-west"],
                    )
                })
                .collect(),
        );

        let minio = (0..4)
            .map(|node| format!("minio-node{node}"))
            .collect::<Vec<_>>();
        put_app(
            simulation,
            "archive-minio",
            ApplicationKind::Minio,
            15,
            minio
                .iter()
                .map(|id| {
                    volume(
                        id,
                        "us-east",
                        128 * GIB,
                        120_000,
                        1,
                        DurabilityOwner::Application,
                        IsolationRequirement::Shared,
                        None,
                        None,
                        100_000,
                        &[],
                    )
                })
                .collect(),
        );

        let kafka = (0..3)
            .map(|broker| format!("kafka-broker{broker}"))
            .collect::<Vec<_>>();
        put_app(
            simulation,
            "events-kafka",
            ApplicationKind::Kafka,
            40,
            kafka
                .iter()
                .map(|id| {
                    volume(
                        id,
                        "us-east",
                        80 * GIB,
                        250_000,
                        1,
                        DurabilityOwner::Application,
                        IsolationRequirement::Shared,
                        None,
                        Some(300),
                        100,
                        &["ap-south"],
                    )
                })
                .collect(),
        );

        let all = cassandra
            .iter()
            .chain(&cockroach)
            .chain(&postgres)
            .chain(&sharded)
            .chain(&minio)
            .chain(&kafka)
            .cloned()
            .collect();
        BusinessIds {
            all,
            cassandra,
            cockroach,
            postgres,
            sharded,
            minio,
            kafka,
        }
    }

    fn request_snapshot(
        simulation: &mut Simulation,
        id: &str,
        members: impl IntoIterator<Item = String>,
        scope: SnapshotScope,
    ) {
        simulation.act(EstateEvent::RequestSnapshot {
            intent: SnapshotIntent {
                snapshot_id: id.to_string(),
                volume_ids: members.into_iter().collect(),
                expand_consistency_relationships: false,
                scope,
                application_consistent: true,
                deadline_ns: simulation.now_ns + 30 * SECOND,
                maximum_hitch_ns: 50_000_000,
                operation_iops_per_volume: 10_000,
            },
        });
    }

    fn observe_replica(simulation: &mut Simulation, volume_id: &str, region: &str, lag: u64) {
        simulation.act(EstateEvent::ObserveReplica {
            observation: ReplicaObservation {
                volume_id: volume_id.to_string(),
                region_id: region.to_string(),
                durable_hwm: HWM - lag,
                applied_hwm: HWM - lag - 1,
                observed_at_ns: simulation.now_ns,
            },
        });
    }

    #[test]
    fn complex_business_estate_evolves_snapshots_fails_over_resizes_and_replays() {
        let mut simulation = Simulation::new(ConcurrentSchedulingStrategy::Hybrid);
        let ids = build_business(&mut simulation);

        let placement_decisions = simulation.plan();
        assert_eq!(
            placement_decisions
                .iter()
                .filter(|decision| matches!(decision, SchedulerDecision::PrepareGang { .. }))
                .count(),
            ids.all.len()
        );
        for volume_id in &ids.all {
            assert!(
                simulation.scheduler.placement(volume_id).is_some(),
                "{volume_id}"
            );
        }

        for (application_id, demanded_iops, exact, samples) in [
            ("billing-postgres", 20_000, true, 20_000),
            ("quotes-sharded-db", 7_000_000, false, 65_536),
            ("orders-cassandra", 1_500_000, false, 32_768),
            ("ledger-cockroach", 800_000, false, 16_384),
            ("archive-minio", 90_000, true, 90_000),
            ("events-kafka", 2_000_000, false, 32_768),
        ] {
            simulation.act(EstateEvent::ObserveDemand {
                bucket: DemandBucket {
                    application_id: application_id.to_string(),
                    interval_start_ns: simulation.now_ns,
                    interval_end_ns: simulation.now_ns + 100_000_000,
                    demanded_iops,
                    queued_operations: demanded_iops / 100,
                    p995_latency_ns: 400_000,
                    exact,
                    samples,
                },
            });
        }

        request_snapshot(
            &mut simulation,
            "billing-cut",
            ids.postgres.clone(),
            SnapshotScope::SameRegion,
        );
        simulation.plan();
        assert_eq!(
            simulation
                .scheduler
                .snapshot("billing-cut")
                .unwrap()
                .cuts
                .len(),
            2
        );

        let same_region_set = ids
            .postgres
            .iter()
            .chain(&ids.kafka)
            .chain(&ids.minio)
            .cloned()
            .collect::<Vec<_>>();
        request_snapshot(
            &mut simulation,
            "commerce-expanded-cut",
            same_region_set.clone(),
            SnapshotScope::SameRegion,
        );
        simulation.plan();
        assert_eq!(
            simulation
                .scheduler
                .snapshot("commerce-expanded-cut")
                .unwrap()
                .cuts
                .len(),
            same_region_set.len()
        );

        let global_set = ids
            .postgres
            .iter()
            .chain(&ids.kafka)
            .chain(&ids.cockroach)
            .cloned()
            .collect::<Vec<_>>();
        request_snapshot(
            &mut simulation,
            "global-finance-cut",
            global_set,
            SnapshotScope::CrossRegion,
        );
        simulation.plan();
        assert_eq!(
            simulation
                .scheduler
                .snapshot("global-finance-cut")
                .unwrap()
                .regions
                .len(),
            3
        );

        add_region(&mut simulation, "ap-south", 4, 4 * TIB, 16);
        simulation.act(EstateEvent::SetApplicationBusinessImpact {
            application_id: "events-kafka".to_string(),
            estimate: BusinessImpactEstimate {
                downtime_cost_microunits_per_second: 220_000_000,
                rto_breach_cost_microunits: 220_000_000_000,
                lost_operation_cost_microunits: 220_000,
            },
        });
        simulation.act(EstateEvent::SetApplicationBusinessImpact {
            application_id: "events-kafka".to_string(),
            estimate: BusinessImpactEstimate {
                downtime_cost_microunits_per_second: 5_000_000,
                rto_breach_cost_microunits: 5_000_000_000,
                lost_operation_cost_microunits: 5_000,
            },
        });
        for volume_id in &ids.postgres {
            observe_replica(&mut simulation, volume_id, "us-west", 0);
        }
        for volume_id in &ids.sharded {
            observe_replica(&mut simulation, volume_id, "eu-west", 0);
        }
        for volume_id in &ids.kafka {
            observe_replica(&mut simulation, volume_id, "ap-south", 10);
        }

        let failure_time = simulation.now_ns + SECOND;
        simulation.emit_at(
            failure_time,
            EstateEvent::SetRegionOnline {
                region_id: "us-east".to_string(),
                online: false,
            },
        );
        let failover_frontier = simulation.plan_unsettled();
        let failover_apps = failover_frontier
            .iter()
            .filter_map(|decision| match decision {
                SchedulerDecision::PrepareGang { plan }
                    if plan.kind == GangKind::RegionalFailover =>
                {
                    Some(plan.application_ids.iter().next().unwrap().clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(&failover_apps[..4], &["quotes-sharded-db"; 4]);
        let postgres_position = failover_apps
            .iter()
            .position(|app| app == "billing-postgres")
            .unwrap();
        let kafka_position = failover_apps
            .iter()
            .position(|app| app == "events-kafka")
            .unwrap();
        assert!(postgres_position < kafka_position);
        assert!(failover_frontier.iter().any(|decision| matches!(
            decision,
            SchedulerDecision::Deferred { subject_id, reason }
                if subject_id.starts_with("recovery:orders-cassandra")
                    && reason.contains("no region-loss RTO")
        )));
        simulation.settle(failover_frontier);

        for volume_id in &ids.sharded {
            assert_eq!(
                simulation.scheduler.placement(volume_id).unwrap().region_id,
                "eu-west"
            );
        }
        for volume_id in &ids.postgres {
            assert_eq!(
                simulation.scheduler.placement(volume_id).unwrap().region_id,
                "us-west"
            );
        }
        for volume_id in &ids.kafka {
            assert_eq!(
                simulation.scheduler.placement(volume_id).unwrap().region_id,
                "ap-south"
            );
        }
        for volume_id in ids.cassandra.iter().chain(&ids.minio) {
            assert_eq!(
                simulation.scheduler.placement(volume_id).unwrap().region_id,
                "us-east"
            );
        }

        simulation.act(EstateEvent::RequestResize {
            intent: ResizeIntent {
                operation_id: "grow-postgres-data".to_string(),
                volume_id: "postgres-data".to_string(),
                new_bytes: 8 * TIB,
                deadline_ns: simulation.now_ns + 60 * SECOND,
            },
        });
        let impossible = simulation.plan();
        assert!(impossible.iter().any(|decision| matches!(
            decision,
            SchedulerDecision::Deferred { subject_id, reason }
                if subject_id == "grow-postgres-data" && reason.contains("host capacity")
        )));
        let postgres_legs = simulation
            .scheduler
            .placement("postgres-data")
            .unwrap()
            .legs
            .clone();
        for leg in postgres_legs {
            let ordinal = leg
                .host_id
                .rsplit_once('h')
                .unwrap()
                .1
                .parse::<usize>()
                .unwrap();
            simulation.act(EstateEvent::PutHost {
                spec: host_spec("us-west", ordinal, 16 * TIB, 16),
            });
        }
        simulation.plan();
        assert_eq!(
            simulation.scheduler.volume_bytes("postgres-data"),
            Some(8 * TIB)
        );
        simulation.assert_replays_exactly();
    }

    fn build_bakeoff(strategy: ConcurrentSchedulingStrategy) -> Simulation {
        let mut simulation = Simulation::new(strategy);
        add_region(&mut simulation, "home", 4, 4 * GIB, 1);
        let volumes = (0..4)
            .map(|ordinal| {
                volume(
                    format!("volume-{ordinal}"),
                    "home",
                    GIB,
                    1_900_000,
                    1,
                    DurabilityOwner::Storage,
                    IsolationRequirement::Dedicated,
                    None,
                    None,
                    0,
                    &[],
                )
            })
            .collect();
        put_app(
            &mut simulation,
            "wide-app",
            ApplicationKind::PerformanceShardedDatabase,
            100,
            volumes,
        );
        simulation
            .executor
            .reject_worker("home-h0", "injected worker loss");
        let frontier = simulation.plan_unsettled();
        simulation.settle(frontier);
        simulation
    }

    #[test]
    fn gang_arena_and_hybrid_bakeoff_preserves_semantics_and_isolates_failure() {
        let mut gang = build_bakeoff(ConcurrentSchedulingStrategy::Gang);
        let mut arena = build_bakeoff(ConcurrentSchedulingStrategy::Arena);
        let mut hybrid = build_bakeoff(ConcurrentSchedulingStrategy::Hybrid);

        let gang_failure = gang.scheduler.audit();
        let arena_failure = arena.scheduler.audit();
        let hybrid_failure = hybrid.scheduler.audit();
        assert_eq!(gang_failure.aborted_units, 1);
        assert_eq!(gang_failure.aborted_volume_attempts, 4);
        assert_eq!(gang_failure.committed_volume_attempts, 0);
        assert_eq!(arena_failure.aborted_volume_attempts, 1);
        assert_eq!(arena_failure.committed_volume_attempts, 3);
        assert_eq!(hybrid_failure, arena_failure);
        assert_eq!(gang_failure.maximum_atomic_width, 4);
        assert_eq!(arena_failure.maximum_atomic_width, 1);

        for simulation in [&mut gang, &mut arena, &mut hybrid] {
            simulation.executor.clear_rejections();
            simulation.plan();
            for ordinal in 0..4 {
                assert!(
                    simulation
                        .scheduler
                        .placement(&format!("volume-{ordinal}"))
                        .is_some()
                );
            }
        }

        let members = (0..4)
            .map(|ordinal| format!("volume-{ordinal}"))
            .collect::<Vec<_>>();
        request_snapshot(
            &mut gang,
            "coordinated-cut",
            members.clone(),
            SnapshotScope::SameRegion,
        );
        let gang_snapshot = gang.plan_unsettled();
        assert!(gang_snapshot.iter().any(|decision| matches!(
            decision,
            SchedulerDecision::PrepareGang { plan }
                if plan.kind == GangKind::ConsistencySnapshot && plan.volume_ids.len() == 4
        )));
        gang.settle(gang_snapshot);

        request_snapshot(
            &mut hybrid,
            "coordinated-cut",
            members.clone(),
            SnapshotScope::SameRegion,
        );
        let hybrid_snapshot = hybrid.plan_unsettled();
        assert!(hybrid_snapshot.iter().any(|decision| matches!(
            decision,
            SchedulerDecision::PrepareGang { plan }
                if plan.kind == GangKind::ConsistencySnapshot && plan.volume_ids.len() == 4
        )));
        hybrid.settle(hybrid_snapshot);

        request_snapshot(
            &mut arena,
            "coordinated-cut",
            members,
            SnapshotScope::SameRegion,
        );
        let arena_snapshot = arena.plan_unsettled();
        assert!(arena_snapshot.iter().any(|decision| matches!(
            decision,
            SchedulerDecision::Deferred { subject_id, reason }
                if subject_id == "coordinated-cut" && reason.contains("cannot weaken")
        )));
        assert!(arena.scheduler.snapshot("coordinated-cut").is_none());

        gang.assert_replays_exactly();
        arena.assert_replays_exactly();
        hybrid.assert_replays_exactly();
    }

    #[test]
    fn live_business_impact_change_controls_scarce_disaster_capacity() {
        let mut simulation = Simulation::new(ConcurrentSchedulingStrategy::Hybrid);
        add_region(&mut simulation, "primary", 2, 2 * GIB, 1);
        add_region(&mut simulation, "recovery", 1, 2 * GIB, 1);
        for (app, weight) in [("alpha", 200), ("beta", 10)] {
            put_app(
                &mut simulation,
                app,
                ApplicationKind::Postgres,
                weight,
                vec![volume(
                    format!("{app}-data"),
                    "primary",
                    GIB,
                    2_000_000,
                    1,
                    DurabilityOwner::Storage,
                    IsolationRequirement::Dedicated,
                    None,
                    Some(30),
                    0,
                    &["recovery"],
                )],
            );
        }
        simulation.plan();
        simulation.act(EstateEvent::SetApplicationBusinessImpact {
            application_id: "alpha".to_string(),
            estimate: BusinessImpactEstimate {
                downtime_cost_microunits_per_second: 1_000_000,
                rto_breach_cost_microunits: 1_000_000_000,
                lost_operation_cost_microunits: 1_000,
            },
        });
        simulation.act(EstateEvent::SetApplicationBusinessImpact {
            application_id: "beta".to_string(),
            estimate: BusinessImpactEstimate {
                downtime_cost_microunits_per_second: 500_000_000,
                rto_breach_cost_microunits: 500_000_000_000,
                lost_operation_cost_microunits: 500_000,
            },
        });
        observe_replica(&mut simulation, "alpha-data", "recovery", 0);
        observe_replica(&mut simulation, "beta-data", "recovery", 0);
        simulation.act(EstateEvent::SetRegionOnline {
            region_id: "primary".to_string(),
            online: false,
        });
        let frontier = simulation.plan_unsettled();
        let admitted = frontier.iter().find_map(|decision| match decision {
            SchedulerDecision::PrepareGang { plan } if plan.kind == GangKind::RegionalFailover => {
                Some(plan.application_ids.iter().next().unwrap().as_str())
            }
            _ => None,
        });
        assert_eq!(admitted, Some("beta"));
        assert!(frontier.iter().any(|decision| matches!(
            decision,
            SchedulerDecision::Deferred { subject_id, reason }
                if subject_id == "recovery:alpha:alpha-data" && reason.contains("lane capacity")
        )));
        simulation.settle(frontier);
        assert_eq!(
            simulation
                .scheduler
                .placement("beta-data")
                .unwrap()
                .region_id,
            "recovery"
        );
        assert_eq!(
            simulation
                .scheduler
                .placement("alpha-data")
                .unwrap()
                .region_id,
            "primary"
        );
        simulation.assert_replays_exactly();
    }

    #[test]
    fn zero_copy_tcp_flow_wins_and_copied_fallback_is_charged() {
        let mut simulation = Simulation::new(ConcurrentSchedulingStrategy::Hybrid);
        add_region(&mut simulation, "source", 1, 16 * GIB, 1);
        add_region(&mut simulation, "recovery-copy", 1, 16 * GIB, 1);
        add_region(&mut simulation, "recovery-zero", 1, 16 * GIB, 1);
        put_app(
            &mut simulation,
            "copy-sensitive-service",
            ApplicationKind::Postgres,
            100,
            vec![volume(
                "copy-sensitive-data",
                "source",
                4 * GIB,
                1_000_000,
                1,
                DurabilityOwner::Storage,
                IsolationRequirement::Reserved,
                None,
                Some(300),
                0,
                &["recovery-copy", "recovery-zero"],
            )],
        );
        simulation.plan();
        for (region, copy_passes, copy_bytes_per_second) in [
            ("recovery-copy", 1, 64 * 1024 * 1024),
            ("recovery-zero", 0, 0),
        ] {
            simulation.act(EstateEvent::ObserveDurableMuxBacklog {
                observation: DurableMuxBacklogObservation {
                    volume_id: "copy-sensitive-data".to_string(),
                    recovery_region_id: region.to_string(),
                    durable_hwm: HWM,
                    applied_hwm: HWM - 1_000,
                    bytes_pending: 4 * GIB,
                    operations_pending: 1_000,
                    layers: vec![MuxBacklogLayer {
                        path_id: format!("tcp-recovery-{region}"),
                        role: FlowLaneRole::InterRegionTransport,
                        transport: FlowTransport::Tcp,
                        fixed_fill_latency_ns: 1_000_000,
                        demux_bytes_per_second: 2 * GIB,
                        demux_operations_per_second: 1_000_000,
                        copy_passes,
                        copy_bytes_per_second,
                    }],
                    observed_at_ns: simulation.now_ns,
                },
            });
        }

        let copied_fallback = simulation
            .scheduler
            .preview_disaster(DisasterScenario {
                scenario_id: "zero-copy-target-also-lost".to_string(),
                unavailable_regions: BTreeSet::from([
                    "recovery-zero".to_string(),
                    "source".to_string(),
                ]),
                simulated_at_ns: simulation.now_ns + SECOND,
            })
            .unwrap();
        let copied_timing = copied_fallback
            .decisions
            .iter()
            .find_map(|decision| match decision {
                SchedulerDecision::PrepareGang { plan }
                    if plan.kind == GangKind::RegionalFailover =>
                {
                    plan.recovery_timing.as_ref()
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(copied_timing.target_region_id, "recovery-copy");
        assert_eq!(copied_timing.copying_lane_count, 1);
        assert_eq!(
            copied_timing.estimated_payload_bytes_copied,
            u128::from(4 * GIB)
        );

        simulation.act(EstateEvent::SetRegionOnline {
            region_id: "source".to_string(),
            online: false,
        });
        let live = simulation.plan_unsettled();
        let plan = live
            .iter()
            .find_map(|decision| match decision {
                SchedulerDecision::PrepareGang { plan }
                    if plan.kind == GangKind::RegionalFailover =>
                {
                    Some(plan)
                }
                _ => None,
            })
            .unwrap();
        let timing = plan.recovery_timing.as_ref().unwrap();
        assert_eq!(timing.target_region_id, "recovery-zero");
        assert_eq!(timing.zero_copy_lane_count, 1);
        assert_eq!(timing.copying_lane_count, 0);
        assert_eq!(timing.estimated_payload_bytes_copied, 0);
        assert!(timing.estimated_completion_ns < copied_timing.estimated_completion_ns);
        assert_eq!(
            plan.scheduled_flows[0].lanes[0].transport,
            FlowTransport::Tcp
        );
        assert_eq!(plan.scheduled_flows[0].lanes[0].copy_passes, 0);
        simulation.settle(live);
        simulation.assert_replays_exactly();
    }

    fn failover_rule(failed: &[&str], action: FailoverAction) -> PreapprovedFailoverRule {
        PreapprovedFailoverRule {
            unavailable_regions: failed.iter().map(|region| (*region).to_string()).collect(),
            action,
        }
    }

    fn observe_mux_backlog(
        simulation: &mut Simulation,
        volume_id: &str,
        recovery_region: &str,
        bytes_pending: u64,
        operations_pending: u64,
        applied_hwm: u64,
    ) {
        simulation.act(EstateEvent::ObserveDurableMuxBacklog {
            observation: DurableMuxBacklogObservation {
                volume_id: volume_id.to_string(),
                recovery_region_id: recovery_region.to_string(),
                durable_hwm: HWM,
                applied_hwm,
                bytes_pending,
                operations_pending,
                layers: vec![
                    MuxBacklogLayer {
                        path_id: format!("shared-interregion-mux-{recovery_region}"),
                        role: FlowLaneRole::InterRegionTransport,
                        transport: FlowTransport::Tcp,
                        fixed_fill_latency_ns: 1_000_000,
                        demux_bytes_per_second: GIB,
                        demux_operations_per_second: 1_000_000,
                        copy_passes: 0,
                        copy_bytes_per_second: 0,
                    },
                    MuxBacklogLayer {
                        path_id: format!("tenant-demux-{recovery_region}"),
                        role: FlowLaneRole::Demux,
                        transport: FlowTransport::SharedArena,
                        fixed_fill_latency_ns: 2_000_000,
                        demux_bytes_per_second: GIB / 2,
                        demux_operations_per_second: 500_000,
                        copy_passes: 0,
                        copy_bytes_per_second: 0,
                    },
                ],
                observed_at_ns: simulation.now_ns,
            },
        });
    }

    #[test]
    fn preplans_converged_recovery_capacity_and_replans_after_mid_replay_region_loss() {
        let mut simulation = Simulation::new(ConcurrentSchedulingStrategy::Hybrid);
        add_region(&mut simulation, "us-east-1", 4, 16 * GIB, 1);
        add_region(&mut simulation, "us-east-2", 3, 16 * GIB, 1);
        add_region(&mut simulation, "eu-west-1", 2, 16 * GIB, 1);

        let first_loss = failover_rule(
            &["us-east-1"],
            FailoverAction::RestoreTo {
                region_id: "us-east-2".to_string(),
            },
        );
        let critical_second_loss = failover_rule(
            &["us-east-1", "us-east-2"],
            FailoverAction::RestoreTo {
                region_id: "eu-west-1".to_string(),
            },
        );
        let very_bad_day = failover_rule(
            &["eu-west-1", "us-east-1", "us-east-2"],
            FailoverAction::HoldDurably,
        );
        let standard_second_loss =
            failover_rule(&["us-east-1", "us-east-2"], FailoverAction::HoldDurably);

        for (application_id, priority, critical) in
            [("tier-zero", 1_000, true), ("best-effort", 10, false)]
        {
            let volumes = (0..2)
                .map(|ordinal| {
                    let mut spec = volume(
                        format!("{application_id}-{ordinal}"),
                        "us-east-1",
                        8 * GIB,
                        1_900_000,
                        1,
                        DurabilityOwner::Storage,
                        IsolationRequirement::Dedicated,
                        None,
                        Some(if critical { 120 } else { 300 }),
                        0,
                        &["eu-west-1", "us-east-2"],
                    );
                    spec.recovery.preapproved_failover = if critical {
                        vec![
                            first_loss.clone(),
                            critical_second_loss.clone(),
                            very_bad_day.clone(),
                        ]
                    } else {
                        vec![
                            first_loss.clone(),
                            standard_second_loss.clone(),
                            very_bad_day.clone(),
                        ]
                    };
                    spec
                })
                .collect();
            put_app(
                &mut simulation,
                application_id,
                ApplicationKind::PerformanceShardedDatabase,
                priority,
                volumes,
            );
            simulation.act(EstateEvent::SetApplicationScenarioImpacts {
                application_id: application_id.to_string(),
                rules: vec![
                    ScenarioBusinessImpactRule {
                        unavailable_regions: BTreeSet::from(["us-east-1".to_string()]),
                        rto_ns: if critical { 60 * SECOND } else { 300 * SECOND },
                        impact: BusinessImpactEstimate {
                            downtime_cost_microunits_per_second: if critical {
                                5_000_000_000
                            } else {
                                10_000_000
                            },
                            rto_breach_cost_microunits: if critical {
                                5_000_000_000_000
                            } else {
                                10_000_000_000
                            },
                            lost_operation_cost_microunits: if critical {
                                5_000_000
                            } else {
                                10_000
                            },
                        },
                    },
                    ScenarioBusinessImpactRule {
                        unavailable_regions: BTreeSet::from([
                            "us-east-1".to_string(),
                            "us-east-2".to_string(),
                        ]),
                        rto_ns: 3 * 60 * 60 * SECOND,
                        impact: BusinessImpactEstimate {
                            downtime_cost_microunits_per_second: if critical {
                                50_000_000
                            } else {
                                1_000_000
                            },
                            rto_breach_cost_microunits: if critical {
                                50_000_000_000
                            } else {
                                1_000_000_000
                            },
                            lost_operation_cost_microunits: if critical {
                                5_000_000
                            } else {
                                10_000
                            },
                        },
                    },
                ],
            });
        }
        simulation.plan();

        for application_id in ["tier-zero", "best-effort"] {
            for ordinal in 0..2 {
                let volume_id = format!("{application_id}-{ordinal}");
                observe_mux_backlog(
                    &mut simulation,
                    &volume_id,
                    "us-east-2",
                    8 * GIB,
                    1_000,
                    HWM - 1_000,
                );
                if application_id == "tier-zero" {
                    observe_mux_backlog(
                        &mut simulation,
                        &volume_id,
                        "eu-west-1",
                        8 * GIB,
                        1_000,
                        HWM - 1_000,
                    );
                }
            }
        }

        let state_before_preview = simulation.scheduler.state_hash().unwrap();
        let single_loss = simulation
            .scheduler
            .preview_disaster(DisasterScenario {
                scenario_id: "single-loss".to_string(),
                unavailable_regions: BTreeSet::from(["us-east-1".to_string()]),
                simulated_at_ns: simulation.now_ns + SECOND,
            })
            .unwrap();
        let single_plans = single_loss
            .decisions
            .iter()
            .filter_map(|decision| match decision {
                SchedulerDecision::PrepareGang { plan }
                    if plan.kind == GangKind::RegionalFailover =>
                {
                    Some(plan)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(single_plans.len(), 3);
        assert!(
            single_plans
                .iter()
                .all(|plan| plan.recovery_timing.as_ref().unwrap().target_region_id == "us-east-2")
        );
        assert!(single_plans.iter().take(2).all(|plan| {
            plan.recovery_timing.as_ref().unwrap().deadline_ns
                - single_loss.scenario.simulated_at_ns
                == 60 * SECOND
        }));
        assert!(single_loss.decisions.iter().any(|decision| matches!(
            decision,
            SchedulerDecision::Deferred { subject_id, reason }
                if subject_id.starts_with("recovery:best-effort")
                    && reason.contains("lane capacity")
        )));
        let completions = single_plans
            .iter()
            .map(|plan| {
                plan.recovery_timing
                    .as_ref()
                    .unwrap()
                    .estimated_completion_ns
            })
            .collect::<Vec<_>>();
        assert!(completions.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(
            simulation.scheduler.state_hash().unwrap(),
            state_before_preview
        );

        let double_loss = simulation
            .scheduler
            .preview_disaster(DisasterScenario {
                scenario_id: "double-loss".to_string(),
                unavailable_regions: BTreeSet::from([
                    "us-east-1".to_string(),
                    "us-east-2".to_string(),
                ]),
                simulated_at_ns: simulation.now_ns + SECOND,
            })
            .unwrap();
        assert_eq!(
            double_loss
                .decisions
                .iter()
                .filter(|decision| matches!(
                    decision,
                    SchedulerDecision::PrepareGang { plan }
                        if plan.kind == GangKind::RegionalFailover
                            && plan.recovery_timing.as_ref().unwrap().target_region_id == "eu-west-1"
                ))
                .count(),
            2
        );
        assert!(double_loss.decisions.iter().all(|decision| match decision {
            SchedulerDecision::PrepareGang { plan } if plan.kind == GangKind::RegionalFailover => {
                plan.recovery_timing.as_ref().unwrap().deadline_ns
                    - double_loss.scenario.simulated_at_ns
                    == 3 * 60 * 60 * SECOND
            }
            _ => true,
        }));
        assert_eq!(
            double_loss
                .decisions
                .iter()
                .filter(|decision| matches!(
                    decision,
                    SchedulerDecision::Deferred { reason, .. }
                        if reason.contains("hold the durable mux backlog")
                ))
                .count(),
            2
        );

        let worst_case = simulation
            .scheduler
            .preview_disaster(DisasterScenario {
                scenario_id: "very-bad-day".to_string(),
                unavailable_regions: BTreeSet::from([
                    "eu-west-1".to_string(),
                    "us-east-1".to_string(),
                    "us-east-2".to_string(),
                ]),
                simulated_at_ns: simulation.now_ns + SECOND,
            })
            .unwrap();
        assert!(!worst_case.decisions.iter().any(|decision| matches!(
            decision,
            SchedulerDecision::PrepareGang { plan }
                if plan.kind == GangKind::RegionalFailover
        )));

        simulation.emit_at(
            simulation.now_ns + SECOND,
            EstateEvent::SetRegionOnline {
                region_id: "us-east-1".to_string(),
                online: false,
            },
        );
        let first_wave = simulation.plan_unsettled();
        let first_committed = first_wave
            .iter()
            .find(|decision| {
                matches!(
                    decision,
                    SchedulerDecision::PrepareGang { plan }
                        if plan.kind == GangKind::RegionalFailover
                )
            })
            .unwrap()
            .clone();
        simulation.settle(vec![first_committed]);
        assert_eq!(
            simulation
                .scheduler
                .placement("tier-zero-0")
                .unwrap()
                .region_id,
            "us-east-2"
        );
        assert_eq!(
            simulation
                .scheduler
                .placement("tier-zero-1")
                .unwrap()
                .region_id,
            "us-east-1"
        );

        observe_mux_backlog(
            &mut simulation,
            "tier-zero-1",
            "us-east-2",
            4 * GIB,
            500,
            HWM - 500,
        );
        observe_mux_backlog(
            &mut simulation,
            "tier-zero-1",
            "eu-west-1",
            4 * GIB,
            500,
            HWM - 500,
        );
        let aborts = simulation.act(EstateEvent::SetRegionOnline {
            region_id: "us-east-2".to_string(),
            online: false,
        });
        assert!(aborts.iter().any(|decision| matches!(
            decision,
            SchedulerDecision::AbortGang { reason, .. }
                if reason.contains("region became unavailable")
        )));

        let second_wave = simulation.plan_unsettled();
        let eu_plans = second_wave
            .iter()
            .filter_map(|decision| match decision {
                SchedulerDecision::PrepareGang { plan }
                    if plan.kind == GangKind::RegionalFailover =>
                {
                    Some(plan)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(eu_plans.len(), 2);
        assert!(eu_plans.iter().all(|plan| {
            let timing = plan.recovery_timing.as_ref().unwrap();
            timing.target_region_id == "eu-west-1"
                && timing.pipeline_fill_ns > 0
                && timing.demux_transfer_ns > 0
                && timing.estimated_completion_ns <= timing.deadline_ns
                && timing.mux_path_ids.len() == 2
                && timing.zero_copy_lane_count == 2
                && timing.copying_lane_count == 0
                && timing.estimated_payload_bytes_copied == 0
                && timing.transports.contains(&FlowTransport::Tcp)
                && !timing.transports.contains(&FlowTransport::Rdma)
                && plan.scheduled_flows.iter().all(|flow| {
                    flow.lanes.iter().any(|lane| {
                        lane.role == FlowLaneRole::InterRegionTransport
                            && lane.transport == FlowTransport::Tcp
                            && lane.copy_passes == 0
                    }) && flow.lanes.last().is_some_and(|lane| {
                        lane.role == FlowLaneRole::TerminalLeaf
                            && lane.transport == FlowTransport::TerminalUserspaceIo
                    })
                })
        }));
        assert!(
            eu_plans[1].recovery_timing.as_ref().unwrap().queue_start_ns
                >= eu_plans[0]
                    .recovery_timing
                    .as_ref()
                    .unwrap()
                    .estimated_completion_ns
        );
        assert_eq!(
            second_wave
                .iter()
                .filter(|decision| matches!(
                    decision,
                    SchedulerDecision::Deferred { reason, .. }
                        if reason.contains("hold the durable mux backlog")
                ))
                .count(),
            2
        );
        simulation.settle(second_wave);
        for ordinal in 0..2 {
            assert_eq!(
                simulation
                    .scheduler
                    .placement(&format!("tier-zero-{ordinal}"))
                    .unwrap()
                    .region_id,
                "eu-west-1"
            );
            assert_eq!(
                simulation
                    .scheduler
                    .placement(&format!("best-effort-{ordinal}"))
                    .unwrap()
                    .region_id,
                "us-east-1"
            );
        }
        simulation.assert_replays_exactly();
    }

    #[test]
    fn one_thousand_interrelated_apps_model_two_thousand_legacy_databases() {
        const LEGACY_DATABASES: usize = 2_000;
        const LEGACY_POOL_VOLUMES: usize = 200;
        const LEGACY_APPLICATIONS: usize = 400;
        const MODERN_APPLICATIONS: usize = 595;
        const DATABASE_MEMBERSHIPS: usize = 6_000;

        let mut simulation = Simulation::new(ConcurrentSchedulingStrategy::Hybrid);
        add_region(&mut simulation, "scale-a", 32, 2 * TIB, 64);
        add_region(&mut simulation, "scale-b", 32, 2 * TIB, 64);

        let legacy_cluster_id = "legacy-dev-database-cluster";
        let legacy_pool_volume_ids = (0..LEGACY_POOL_VOLUMES)
            .map(|ordinal| format!("legacy-dev-db-pool-{ordinal:03}"))
            .collect::<Vec<_>>();
        put_app(
            &mut simulation,
            legacy_cluster_id,
            ApplicationKind::Postgres,
            10,
            legacy_pool_volume_ids
                .iter()
                .map(|volume_id| {
                    volume(
                        volume_id,
                        "scale-a",
                        GIB,
                        2_000,
                        1,
                        DurabilityOwner::Storage,
                        IsolationRequirement::Shared,
                        Some("legacy-dev-recovery".to_string()),
                        Some(3_600),
                        100_000,
                        &[],
                    )
                })
                .collect(),
        );

        let production_domains = ["identity", "orders", "payments", "fulfillment"];
        let mut dedicated_database_volume_ids = Vec::new();
        for (ordinal, domain) in production_domains.iter().enumerate() {
            let application_id = format!("{domain}-production-database");
            let volume_id = format!("{application_id}-owned-volume");
            put_app(
                &mut simulation,
                &application_id,
                ApplicationKind::CockroachDb,
                5_000,
                vec![volume(
                    &volume_id,
                    if ordinal % 2 == 0 {
                        "scale-a"
                    } else {
                        "scale-b"
                    },
                    8 * GIB,
                    40_000,
                    2,
                    DurabilityOwner::Storage,
                    IsolationRequirement::Dedicated,
                    None,
                    Some(120),
                    100,
                    &[],
                )],
            );
            dedicated_database_volume_ids.push(volume_id);
        }

        let service_domains = [
            "identity",
            "catalog",
            "orders",
            "payments",
            "fulfillment",
            "risk",
            "support",
            "analytics",
        ];
        let mut legacy_application_ids = Vec::new();
        let mut legacy_application_volume_ids = Vec::new();
        for ordinal in 0..LEGACY_APPLICATIONS {
            let domain = service_domains[ordinal % service_domains.len()];
            let application_id = format!("legacy-dev-{domain}-{ordinal:03}");
            let volume_id = format!("{application_id}-owned-volume");
            put_app(
                &mut simulation,
                &application_id,
                ApplicationKind::Other,
                1 + u32::try_from(ordinal % 5).unwrap(),
                vec![volume(
                    &volume_id,
                    if ordinal % 2 == 0 {
                        "scale-a"
                    } else {
                        "scale-b"
                    },
                    GIB / 8,
                    500,
                    1,
                    DurabilityOwner::Storage,
                    IsolationRequirement::Shared,
                    None,
                    Some(7_200),
                    1_000_000,
                    &[],
                )],
            );
            legacy_application_ids.push(application_id);
            legacy_application_volume_ids.push(volume_id);
        }

        let mut modern_application_volume_ids = Vec::new();
        for ordinal in 0..MODERN_APPLICATIONS {
            let domain = service_domains[ordinal % service_domains.len()];
            let application_id = format!("modern-{domain}-{ordinal:03}");
            let volume_id = format!("{application_id}-owned-volume");
            let kind = match ordinal % 5 {
                0 => ApplicationKind::Postgres,
                1 => ApplicationKind::Kafka,
                2 => ApplicationKind::Minio,
                3 => ApplicationKind::CassandraJbod,
                _ => ApplicationKind::Other,
            };
            let impact = if ordinal % 100 == 0 {
                2_000
            } else {
                50 + u32::try_from(ordinal % 50).unwrap()
            };
            put_app(
                &mut simulation,
                &application_id,
                kind,
                impact,
                vec![volume(
                    &volume_id,
                    if ordinal % 2 == 0 {
                        "scale-a"
                    } else {
                        "scale-b"
                    },
                    GIB / 4,
                    10_000,
                    2,
                    DurabilityOwner::Storage,
                    if impact >= 2_000 {
                        IsolationRequirement::Dedicated
                    } else {
                        IsolationRequirement::Reserved
                    },
                    None,
                    Some(300),
                    10_000,
                    &[],
                )],
            );
            modern_application_volume_ids.push(volume_id);
        }

        // Produce varied fan-out in [10, 50] while totaling exactly three
        // physical memberships for each of the 2,000 logical databases.
        let mut database_counts_per_volume = (0..LEGACY_POOL_VOLUMES)
            .map(|ordinal| 10 + ordinal % 41)
            .collect::<Vec<_>>();
        let initial_memberships = database_counts_per_volume.iter().sum::<usize>();
        let mut deficit = DATABASE_MEMBERSHIPS - initial_memberships;
        for count in &mut database_counts_per_volume {
            if deficit == 0 {
                break;
            }
            if *count < 50 {
                *count += 1;
                deficit -= 1;
            }
        }
        assert_eq!(deficit, 0);
        assert_eq!(database_counts_per_volume.iter().min(), Some(&10));
        assert_eq!(database_counts_per_volume.iter().max(), Some(&50));
        assert_eq!(
            database_counts_per_volume.iter().sum::<usize>(),
            DATABASE_MEMBERSHIPS
        );

        let mut database_volume_ids = vec![BTreeSet::<String>::new(); LEGACY_DATABASES];
        let mut cursor = 0usize;
        for (volume_id, database_count) in legacy_pool_volume_ids
            .iter()
            .zip(&database_counts_per_volume)
        {
            for offset in 0..*database_count {
                database_volume_ids[(cursor + offset) % LEGACY_DATABASES].insert(volume_id.clone());
            }
            cursor += database_count;
        }
        assert_eq!(cursor, DATABASE_MEMBERSHIPS);
        assert!(database_volume_ids.iter().all(|volumes| volumes.len() == 3));

        for (database, volume_ids) in database_volume_ids.iter().enumerate() {
            simulation.act(EstateEvent::PutLogicalDatabase {
                spec: LogicalDatabaseSpec {
                    database_id: format!("legacy-dev-db-{database:04}"),
                    generation: 1,
                    cluster_application_id: legacy_cluster_id.to_string(),
                    volume_ids: volume_ids.clone(),
                    consumer_application_ids: BTreeSet::from([legacy_application_ids
                        [database / 5]
                        .clone()]),
                },
            });
        }

        simulation.act(EstateEvent::PutConsistencyGroup {
            spec: ConsistencyGroupSpec {
                group_id: "legacy-dev-cluster-internal".to_string(),
                generation: 1,
                volume_ids: legacy_pool_volume_ids.iter().cloned().collect(),
            },
        });
        for application in 0..LEGACY_APPLICATIONS {
            let mut members = BTreeSet::from([legacy_application_volume_ids[application].clone()]);
            for database in application * 5..application * 5 + 5 {
                members.extend(database_volume_ids[database].iter().cloned());
            }
            simulation.act(EstateEvent::PutConsistencyGroup {
                spec: ConsistencyGroupSpec {
                    group_id: format!(
                        "{}-legacy-database-cut",
                        legacy_application_ids[application]
                    ),
                    generation: 1,
                    volume_ids: members,
                },
            });
        }

        for (ordinal, volume_id) in modern_application_volume_ids.iter().enumerate() {
            simulation.act(EstateEvent::PutConsistencyGroup {
                spec: ConsistencyGroupSpec {
                    group_id: format!("modern-service-{ordinal:03}-database-cut"),
                    generation: 1,
                    volume_ids: BTreeSet::from([
                        volume_id.clone(),
                        dedicated_database_volume_ids[ordinal % 4].clone(),
                    ]),
                },
            });
        }
        for (workflow, left, right) in [
            ("checkout-revenue-close", 0usize, 1usize),
            ("identity-fulfillment-audit", 2usize, 3usize),
        ] {
            simulation.act(EstateEvent::PutConsistencyGroup {
                spec: ConsistencyGroupSpec {
                    group_id: workflow.to_string(),
                    generation: 1,
                    volume_ids: BTreeSet::from([
                        dedicated_database_volume_ids[left].clone(),
                        dedicated_database_volume_ids[right].clone(),
                    ]),
                },
            });
        }

        let all_volume_ids = legacy_pool_volume_ids
            .iter()
            .chain(&dedicated_database_volume_ids)
            .chain(&legacy_application_volume_ids)
            .chain(&modern_application_volume_ids)
            .cloned()
            .collect::<BTreeSet<_>>();
        let cardinality = simulation.scheduler.cardinality();
        assert_eq!(cardinality.business_entities, 1);
        assert_eq!(cardinality.applications, 1_000);
        assert_eq!(cardinality.volumes, 1_199);
        assert_eq!(cardinality.logical_databases, 2_000);
        assert_eq!(cardinality.database_volume_memberships, 6_000);
        assert_eq!(cardinality.consistency_groups, 998);
        assert_eq!(all_volume_ids.len(), 1_199);

        let legacy_closure = simulation
            .scheduler
            .related_volume_closure(&BTreeSet::from([legacy_pool_volume_ids[0].clone()]))
            .unwrap();
        assert_eq!(legacy_closure.len(), 600);
        let first_modern_domain = simulation
            .scheduler
            .related_volume_closure(&BTreeSet::from([dedicated_database_volume_ids[0].clone()]))
            .unwrap();
        assert_eq!(first_modern_domain.len(), 300);

        let impact_reports = simulation.scheduler.physical_volume_impacts();
        let legacy_database_fanouts = legacy_pool_volume_ids
            .iter()
            .map(|volume_id| impact_reports[volume_id].logical_database_count)
            .collect::<Vec<_>>();
        assert_eq!(legacy_database_fanouts.iter().min(), Some(&10));
        assert_eq!(legacy_database_fanouts.iter().max(), Some(&50));
        let highest = simulation.scheduler.highest_impact_volumes();
        assert_eq!(highest.len(), 4);
        assert!(
            highest
                .iter()
                .all(|report| dedicated_database_volume_ids.contains(&report.volume_id))
        );
        let maximum_legacy_impact = legacy_pool_volume_ids
            .iter()
            .map(|volume_id| impact_reports[volume_id].aggregate_impact_floor_microunits)
            .max()
            .unwrap();
        assert!(highest[0].aggregate_impact_floor_microunits > maximum_legacy_impact);

        let placement_output = simulation.plan();
        assert_eq!(
            placement_output
                .iter()
                .filter(|decision| matches!(decision, SchedulerDecision::PrepareGang { .. }))
                .count(),
            all_volume_ids.len()
        );
        assert!(
            all_volume_ids
                .iter()
                .all(|volume_id| simulation.scheduler.placement(volume_id).is_some())
        );

        simulation.act(EstateEvent::PutConsistencyGroup {
            spec: ConsistencyGroupSpec {
                group_id: "estate-wide-coordinated-release".to_string(),
                generation: 1,
                volume_ids: std::iter::once(legacy_pool_volume_ids[0].clone())
                    .chain(dedicated_database_volume_ids.iter().cloned())
                    .collect(),
            },
        });
        simulation.act(EstateEvent::RequestSnapshot {
            intent: SnapshotIntent {
                snapshot_id: "all-1000-applications-cut".to_string(),
                volume_ids: BTreeSet::from([legacy_pool_volume_ids[0].clone()]),
                expand_consistency_relationships: true,
                scope: SnapshotScope::CrossRegion,
                application_consistent: true,
                deadline_ns: simulation.now_ns + 900 * SECOND,
                maximum_hitch_ns: 3 * SECOND,
                operation_iops_per_volume: 100,
            },
        });
        simulation.plan();
        let global = simulation
            .scheduler
            .snapshot("all-1000-applications-cut")
            .unwrap();
        assert_eq!(global.cuts.len(), all_volume_ids.len());
        assert_eq!(global.regions.len(), 2);
        assert_eq!(simulation.scheduler.cardinality().consistency_groups, 999);
        assert_eq!(
            simulation.scheduler.audit().maximum_atomic_width,
            all_volume_ids.len() as u64
        );
        simulation.assert_replays_exactly();
    }
}
