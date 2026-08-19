//! Live IOPS admission and accounting primitives.
//!
//! The control plane plans guarantees against every resource traversed by a
//! userspace I/O path.  The data plane receives a precomputed, lane-local
//! budget.  It never walks the resource graph, takes a lock, or contacts the
//! control plane while admitting I/O.

use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};

const PPM: u64 = 1_000_000;
const NANOS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Cpu,
    NumaMemory,
    PcieLane,
    PcieLink,
    PcieSwitch,
    NicQueue,
    Nic,
    StorageQueue,
    StorageDevice,
    NetworkLink,
    Other,
}

/// A measured capacity envelope, not a vendor-nameplate value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityResource {
    pub id: String,
    pub kind: ResourceKind,
    pub measured_iops: u64,
    pub measured_bytes_per_second: u64,
    /// Capacity retained for variance, faults, and model error.
    pub safety_margin_ppm: u32,
}

impl CapacityResource {
    pub fn usable_iops(&self) -> u64 {
        scale_ppm(
            self.measured_iops,
            PPM.saturating_sub(u64::from(self.safety_margin_ppm)),
        )
    }

    pub fn usable_bytes_per_second(&self) -> u64 {
        scale_ppm(
            self.measured_bytes_per_second,
            PPM.saturating_sub(u64::from(self.safety_margin_ppm)),
        )
    }
}

/// Cost of one logical 4 KiB operation on one shared resource.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceClaim {
    pub resource_id: String,
    /// One million means one resource operation per logical operation.
    pub iops_cost_ppm: u32,
    /// Bytes transferred on this resource per logical operation.
    pub bytes_per_io: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadClass {
    Foreground,
    Snapshot,
    LiveMigration,
    Recovery,
    Scrub,
    Compaction,
    Replication,
    OtherSystem,
}

/// Human-facing service policy. `guaranteed_iops=None` is intentionally not a
/// capacity promise; the runtime controller may protect `retention_ppm` of a
/// recent uncontended baseline, but admission of guaranteed tenants wins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadPolicy {
    pub id: String,
    pub class: WorkloadClass,
    pub guaranteed_iops: Option<u64>,
    pub burst_iops: u64,
    pub burst_seconds: u32,
    pub retention_ppm: u32,
    pub lanes: u16,
    pub path: Vec<ResourceClaim>,
}

/// A continuously reserved maintenance objective. Entries with the same
/// non-empty scenario group are alternatives, so the planner reserves their
/// maximum per resource. Different groups may overlap and are summed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceObjective {
    pub id: String,
    pub class: WorkloadClass,
    pub bytes_to_process: u64,
    pub deadline_seconds: u64,
    pub average_io_bytes: u64,
    pub amplification_ppm: u32,
    pub scenario_group: Option<String>,
    pub path: Vec<ResourceClaim>,
}

impl MaintenanceObjective {
    pub fn required_iops(&self) -> io::Result<u64> {
        if self.deadline_seconds == 0 || self.average_io_bytes == 0 {
            return Err(invalid(format!(
                "maintenance objective {} has a zero deadline or I/O size",
                self.id
            )));
        }
        let amplified = div_ceil(
            u128::from(self.bytes_to_process) * u128::from(self.amplification_ppm),
            u128::from(PPM),
        );
        let operations = div_ceil(amplified, u128::from(self.average_io_bytes));
        u64::try_from(div_ceil(operations, u128::from(self.deadline_seconds)))
            .map_err(|_| invalid(format!("maintenance objective {} overflows IOPS", self.id)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePlan {
    pub resource_id: String,
    pub usable_iops: u64,
    pub usable_bytes_per_second: u64,
    pub foreground_reserved_iops: u64,
    pub maintenance_reserved_iops: u64,
    pub foreground_reserved_bytes_per_second: u64,
    pub maintenance_reserved_bytes_per_second: u64,
    pub spare_iops: u64,
    pub spare_bytes_per_second: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadBudget {
    pub workload_id: String,
    pub guaranteed_iops: u64,
    pub opportunistic_iops: u64,
    pub burst_iops: u64,
    pub burst_seconds: u32,
    pub lanes: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionPlan {
    pub generation: u64,
    pub resources: Vec<ResourcePlan>,
    pub workloads: Vec<WorkloadBudget>,
}

#[derive(Clone, Default)]
struct Demand {
    iops: u128,
    bytes_per_second: u128,
}

/// Proves committed foreground and maintenance capacity before returning a
/// generation that can be published to lane mailboxes.
pub fn plan_admission(
    generation: u64,
    resources: &[CapacityResource],
    workloads: &[WorkloadPolicy],
    maintenance: &[MaintenanceObjective],
) -> io::Result<AdmissionPlan> {
    if generation == 0 {
        return Err(invalid("admission generation must be nonzero"));
    }
    let resource_by_id: HashMap<_, _> = resources
        .iter()
        .map(|resource| (resource.id.as_str(), resource))
        .collect();
    if resource_by_id.len() != resources.len() {
        return Err(invalid("duplicate capacity resource id"));
    }

    let mut foreground: HashMap<&str, Demand> = HashMap::new();
    for workload in workloads {
        validate_workload(workload, &resource_by_id)?;
        let guaranteed = workload.guaranteed_iops.unwrap_or(0);
        add_path_demand(&mut foreground, guaranteed, &workload.path)?;
    }

    // group -> resource -> maximum alternative demand. Objectives without a
    // group receive a unique group and therefore overlap conservatively.
    let mut scenarios: BTreeMap<String, HashMap<&str, Demand>> = BTreeMap::new();
    for objective in maintenance {
        validate_path(&objective.id, &objective.path, &resource_by_id)?;
        let required = objective.required_iops()?;
        let group = objective
            .scenario_group
            .clone()
            .unwrap_or_else(|| format!("objective:{}", objective.id));
        let mut candidate = HashMap::new();
        add_path_demand(&mut candidate, required, &objective.path)?;
        let group_demand = scenarios.entry(group).or_default();
        for (resource, demand) in candidate {
            let current = group_demand.entry(resource).or_default();
            current.iops = current.iops.max(demand.iops);
            current.bytes_per_second = current.bytes_per_second.max(demand.bytes_per_second);
        }
    }
    let mut maintenance_demand: HashMap<&str, Demand> = HashMap::new();
    for group in scenarios.values() {
        for (resource, demand) in group {
            let total = maintenance_demand.entry(resource).or_default();
            total.iops = total.iops.saturating_add(demand.iops);
            total.bytes_per_second = total
                .bytes_per_second
                .saturating_add(demand.bytes_per_second);
        }
    }

    let mut resource_plans = Vec::with_capacity(resources.len());
    for resource in resources {
        let foreground = foreground
            .get(resource.id.as_str())
            .cloned()
            .unwrap_or_default();
        let maintenance = maintenance_demand
            .get(resource.id.as_str())
            .cloned()
            .unwrap_or_default();
        let foreground_iops = demand_units(foreground.iops)?;
        let maintenance_iops = demand_units(maintenance.iops)?;
        let foreground_bytes = u64::try_from(foreground.bytes_per_second)
            .map_err(|_| invalid(format!("resource {} byte demand overflow", resource.id)))?;
        let maintenance_bytes = u64::try_from(maintenance.bytes_per_second)
            .map_err(|_| invalid(format!("resource {} byte demand overflow", resource.id)))?;
        let usable_iops = resource.usable_iops();
        let usable_bytes = resource.usable_bytes_per_second();
        let requested_iops = foreground_iops.saturating_add(maintenance_iops);
        let requested_bytes = foreground_bytes.saturating_add(maintenance_bytes);
        if requested_iops > usable_iops || requested_bytes > usable_bytes {
            return Err(invalid(format!(
                "resource {} cannot guarantee plan: iops {requested_iops}/{usable_iops}, bytes/s {requested_bytes}/{usable_bytes}",
                resource.id
            )));
        }
        resource_plans.push(ResourcePlan {
            resource_id: resource.id.clone(),
            usable_iops,
            usable_bytes_per_second: usable_bytes,
            foreground_reserved_iops: foreground_iops,
            maintenance_reserved_iops: maintenance_iops,
            foreground_reserved_bytes_per_second: foreground_bytes,
            maintenance_reserved_bytes_per_second: maintenance_bytes,
            spare_iops: usable_iops - requested_iops,
            spare_bytes_per_second: usable_bytes - requested_bytes,
        });
    }

    let workloads = workloads
        .iter()
        .map(|workload| {
            let guaranteed = workload.guaranteed_iops.unwrap_or(0);
            WorkloadBudget {
                workload_id: workload.id.clone(),
                guaranteed_iops: guaranteed,
                opportunistic_iops: workload.burst_iops.saturating_sub(guaranteed),
                burst_iops: workload.burst_iops,
                burst_seconds: workload.burst_seconds,
                lanes: workload.lanes,
            }
        })
        .collect();
    Ok(AdmissionPlan {
        generation,
        resources: resource_plans,
        workloads,
    })
}

fn validate_workload(
    workload: &WorkloadPolicy,
    resources: &HashMap<&str, &CapacityResource>,
) -> io::Result<()> {
    if workload.id.is_empty() || workload.lanes == 0 {
        return Err(invalid("workload id and lane count must be nonzero"));
    }
    if workload.retention_ppm > PPM as u32 {
        return Err(invalid(format!(
            "workload {} retention exceeds 100%",
            workload.id
        )));
    }
    let guaranteed = workload.guaranteed_iops.unwrap_or(0);
    if workload.burst_iops < guaranteed {
        return Err(invalid(format!(
            "workload {} burst IOPS is below guaranteed IOPS",
            workload.id
        )));
    }
    validate_path(&workload.id, &workload.path, resources)
}

fn validate_path(
    owner: &str,
    path: &[ResourceClaim],
    resources: &HashMap<&str, &CapacityResource>,
) -> io::Result<()> {
    if path.is_empty() {
        return Err(invalid(format!("{owner} has an empty resource path")));
    }
    for claim in path {
        if !resources.contains_key(claim.resource_id.as_str()) {
            return Err(invalid(format!(
                "{owner} references unknown resource {}",
                claim.resource_id
            )));
        }
        if claim.iops_cost_ppm == 0 && claim.bytes_per_io == 0 {
            return Err(invalid(format!(
                "{owner} has a zero-cost claim on {}",
                claim.resource_id
            )));
        }
    }
    Ok(())
}

fn add_path_demand<'a>(
    demands: &mut HashMap<&'a str, Demand>,
    logical_iops: u64,
    path: &'a [ResourceClaim],
) -> io::Result<()> {
    for claim in path {
        let demand = demands.entry(claim.resource_id.as_str()).or_default();
        demand.iops = demand
            .iops
            .saturating_add(u128::from(logical_iops) * u128::from(claim.iops_cost_ppm));
        demand.bytes_per_second = demand
            .bytes_per_second
            .saturating_add(u128::from(logical_iops) * u128::from(claim.bytes_per_io));
    }
    Ok(())
}

fn demand_units(micro_units: u128) -> io::Result<u64> {
    u64::try_from(div_ceil(micro_units, u128::from(PPM)))
        .map_err(|_| invalid("resource IOPS demand overflow"))
}

fn div_ceil(value: u128, divisor: u128) -> u128 {
    value / divisor + u128::from(value % divisor != 0)
}

fn scale_ppm(value: u64, ppm: u64) -> u64 {
    ((u128::from(value) * u128::from(ppm)) / u128::from(PPM)) as u64
}

/// Single-writer, many-reader mailbox. A lane reads it only at a local grant
/// boundary, not for each I/O. The sequence is a seqlock and is also the
/// configuration generation fence.
#[repr(align(64))]
pub struct LaneBudgetMailbox {
    sequence: AtomicU64,
    generation: AtomicU64,
    sustained_iops: AtomicU64,
    peak_iops: AtomicU64,
    burst_ops: AtomicU64,
    quantum_ops: AtomicU64,
    metric_publish_ns: AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneBudgetSnapshot {
    pub generation: u64,
    pub sustained_iops: u64,
    pub peak_iops: u64,
    /// Sustained bucket capacity. Normally one quantum plus
    /// `(peak_iops - sustained_iops) * burst_seconds` divided among lanes.
    pub burst_ops: u64,
    /// Peak bucket capacity; at least the largest admitted descriptor batch.
    pub quantum_ops: u64,
    pub metric_publish_ns: u64,
}

impl LaneBudgetMailbox {
    pub fn new(initial: LaneBudgetSnapshot) -> Self {
        Self {
            sequence: AtomicU64::new(2),
            generation: AtomicU64::new(initial.generation),
            sustained_iops: AtomicU64::new(initial.sustained_iops),
            peak_iops: AtomicU64::new(initial.peak_iops),
            burst_ops: AtomicU64::new(initial.burst_ops),
            quantum_ops: AtomicU64::new(initial.quantum_ops),
            metric_publish_ns: AtomicU64::new(initial.metric_publish_ns),
        }
    }

    /// The control plane must serialize writers to one mailbox.
    pub fn publish(&self, budget: LaneBudgetSnapshot) {
        let sequence = self.sequence.load(Ordering::Relaxed).wrapping_add(1) | 1;
        self.sequence.store(sequence, Ordering::Release);
        self.generation.store(budget.generation, Ordering::Relaxed);
        self.sustained_iops
            .store(budget.sustained_iops, Ordering::Relaxed);
        self.peak_iops.store(budget.peak_iops, Ordering::Relaxed);
        self.burst_ops.store(budget.burst_ops, Ordering::Relaxed);
        self.quantum_ops
            .store(budget.quantum_ops, Ordering::Relaxed);
        self.metric_publish_ns
            .store(budget.metric_publish_ns, Ordering::Relaxed);
        self.sequence
            .store(sequence.wrapping_add(1), Ordering::Release);
    }

    pub fn load(&self) -> LaneBudgetSnapshot {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let budget = LaneBudgetSnapshot {
                generation: self.generation.load(Ordering::Relaxed),
                sustained_iops: self.sustained_iops.load(Ordering::Relaxed),
                peak_iops: self.peak_iops.load(Ordering::Relaxed),
                burst_ops: self.burst_ops.load(Ordering::Relaxed),
                quantum_ops: self.quantum_ops.load(Ordering::Relaxed),
                metric_publish_ns: self.metric_publish_ns.load(Ordering::Relaxed),
            };
            if before == self.sequence.load(Ordering::Acquire) {
                return budget;
            }
        }
    }
}

/// Lane-owned token state. Admission is exact over time and performs no atomic
/// operation. Call once for a homogeneous descriptor/CQ batch.
pub struct LaneLimiter {
    budget: LaneBudgetSnapshot,
    last_refill_ns: u64,
    sustained_token_nanos: u128,
    peak_token_nanos: u128,
}

impl LaneLimiter {
    pub fn new(now_ns: u64, budget: LaneBudgetSnapshot) -> Self {
        Self {
            budget,
            last_refill_ns: now_ns,
            sustained_token_nanos: u128::from(budget.burst_ops) * NANOS_PER_SECOND,
            peak_token_nanos: u128::from(budget.quantum_ops) * NANOS_PER_SECOND,
        }
    }

    pub fn budget(&self) -> LaneBudgetSnapshot {
        self.budget
    }

    pub fn refresh(&mut self, now_ns: u64, mailbox: &LaneBudgetMailbox) -> bool {
        let next = mailbox.load();
        if next.generation == self.budget.generation {
            return false;
        }
        self.refill(now_ns);
        self.budget = next;
        self.sustained_token_nanos = self
            .sustained_token_nanos
            .min(u128::from(next.burst_ops) * NANOS_PER_SECOND);
        self.peak_token_nanos = self
            .peak_token_nanos
            .min(u128::from(next.quantum_ops) * NANOS_PER_SECOND);
        true
    }

    pub fn admit(&mut self, now_ns: u64, requested_ops: u32) -> u32 {
        if requested_ops == 0 {
            return 0;
        }
        if self.budget.sustained_iops == 0 || self.budget.peak_iops == 0 {
            return 0;
        }
        self.refill(now_ns);
        let sustained_available =
            (self.sustained_token_nanos / NANOS_PER_SECOND).min(u128::from(u32::MAX)) as u32;
        let peak_available =
            (self.peak_token_nanos / NANOS_PER_SECOND).min(u128::from(u32::MAX)) as u32;
        let available = sustained_available.min(peak_available);
        let admitted = requested_ops.min(available);
        let charge = u128::from(admitted) * NANOS_PER_SECOND;
        self.sustained_token_nanos -= charge;
        self.peak_token_nanos -= charge;
        admitted
    }

    pub fn nanos_until_one(&self) -> u64 {
        wait_for_token(self.sustained_token_nanos, self.budget.sustained_iops)
            .max(wait_for_token(self.peak_token_nanos, self.budget.peak_iops))
    }

    fn refill(&mut self, now_ns: u64) {
        let elapsed = now_ns.saturating_sub(self.last_refill_ns);
        self.last_refill_ns = now_ns;
        let sustained_capacity = u128::from(self.budget.burst_ops) * NANOS_PER_SECOND;
        self.sustained_token_nanos = self
            .sustained_token_nanos
            .saturating_add(u128::from(elapsed) * u128::from(self.budget.sustained_iops))
            .min(sustained_capacity);
        let peak_capacity = u128::from(self.budget.quantum_ops) * NANOS_PER_SECOND;
        self.peak_token_nanos = self
            .peak_token_nanos
            .saturating_add(u128::from(elapsed) * u128::from(self.budget.peak_iops))
            .min(peak_capacity);
    }
}

fn wait_for_token(tokens: u128, rate: u64) -> u64 {
    if tokens >= NANOS_PER_SECOND {
        return 0;
    }
    if rate == 0 {
        return u64::MAX;
    }
    u64::try_from(div_ceil(NANOS_PER_SECOND - tokens, u128::from(rate))).unwrap_or(u64::MAX)
}

/// Atomics are written only when the lane chooses to publish, normally every
/// 50-250 ms. Completion accounting itself remains ordinary lane-local adds.
#[repr(align(64))]
pub struct MetricPublication {
    sequence: AtomicU64,
    completed_ops: AtomicU64,
    completed_bytes: AtomicU64,
    throttled_ops: AtomicU64,
    published_ns: AtomicU64,
}

impl Default for MetricPublication {
    fn default() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            completed_ops: AtomicU64::new(0),
            completed_bytes: AtomicU64::new(0),
            throttled_ops: AtomicU64::new(0),
            published_ns: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneMetricSnapshot {
    pub completed_ops: u64,
    pub completed_bytes: u64,
    pub throttled_ops: u64,
    pub published_ns: u64,
}

impl MetricPublication {
    fn publish(&self, metrics: LaneMetricSnapshot) {
        let sequence = self.sequence.load(Ordering::Relaxed).wrapping_add(1) | 1;
        self.sequence.store(sequence, Ordering::Release);
        self.completed_ops
            .store(metrics.completed_ops, Ordering::Relaxed);
        self.completed_bytes
            .store(metrics.completed_bytes, Ordering::Relaxed);
        self.throttled_ops
            .store(metrics.throttled_ops, Ordering::Relaxed);
        self.published_ns
            .store(metrics.published_ns, Ordering::Relaxed);
        self.sequence
            .store(sequence.wrapping_add(1), Ordering::Release);
    }

    pub fn load(&self) -> LaneMetricSnapshot {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let result = LaneMetricSnapshot {
                completed_ops: self.completed_ops.load(Ordering::Relaxed),
                completed_bytes: self.completed_bytes.load(Ordering::Relaxed),
                throttled_ops: self.throttled_ops.load(Ordering::Relaxed),
                published_ns: self.published_ns.load(Ordering::Relaxed),
            };
            if before == self.sequence.load(Ordering::Acquire) {
                return result;
            }
        }
    }
}

#[derive(Default)]
pub struct LaneMetrics {
    completed_ops: u64,
    completed_bytes: u64,
    throttled_ops: u64,
    last_publish_ns: u64,
}

impl LaneMetrics {
    pub fn complete_batch(&mut self, operations: u32, bytes: u64) {
        self.completed_ops = self.completed_ops.saturating_add(u64::from(operations));
        self.completed_bytes = self.completed_bytes.saturating_add(bytes);
    }

    pub fn throttle_batch(&mut self, operations: u32) {
        self.throttled_ops = self.throttled_ops.saturating_add(u64::from(operations));
    }

    pub fn maybe_publish(
        &mut self,
        now_ns: u64,
        interval_ns: u64,
        publication: &MetricPublication,
    ) -> bool {
        if now_ns.saturating_sub(self.last_publish_ns) < interval_ns {
            return false;
        }
        self.last_publish_ns = now_ns;
        publication.publish(LaneMetricSnapshot {
            completed_ops: self.completed_ops,
            completed_bytes: self.completed_bytes,
            throttled_ops: self.throttled_ops,
            published_ns: now_ns,
        });
        true
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Precision {
    pub exact: bool,
    pub sample_probability: f64,
    pub relative_error_95: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionClass {
    LocalRead,
    RemoteRead,
    EarlyLocalWrite,
    RemoteAcknowledgedWrite,
    DurableWrite,
    SyncDrain,
    FuaDrain,
    Snapshot,
    LiveMigration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencyTicket {
    started_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LatencyBin {
    pub upper_bound_ns: u64,
    pub samples: u64,
    pub estimated_operations: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LatencyQuantile {
    pub quantile: f64,
    pub estimate_ns: u64,
    pub confidence_lower_ns: u64,
    pub confidence_upper_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LatencyMetricPoint {
    pub subject: String,
    pub lane: u32,
    pub completion_class: CompletionClass,
    pub interval_start_ns: u64,
    pub interval_end_ns: u64,
    pub samples: u64,
    pub estimated_operations: f64,
    pub sample_probability: f64,
    pub confidence_level: f64,
    pub quantile_rank_error: f64,
    pub histogram_significant_figures: u8,
    pub clock_source: String,
    pub clock_resolution_ns: u64,
    pub clock_read_overhead_ns: u64,
    pub overflow_samples: u64,
    pub maximum_observed_ns: u64,
    pub coordinated_omission_correction_ns: Option<u64>,
    pub bins: Vec<LatencyBin>,
    pub corrected_bins: Vec<LatencyBin>,
    pub quantiles: Vec<LatencyQuantile>,
}

/// Lane-owned latency sampler. An unsampled operation executes only a local
/// decrement and branch. Random-number generation, both clock reads, and HDR
/// histogram math occur only for selected operations. Give each lane and
/// completion class an independent seed and recorder so merged samples remain
/// stratified and no shared cache line is touched.
pub struct LaneLatencyRecorder {
    probability: f64,
    failures_until_sample: u64,
    rng: u64,
    raw: Histogram<u64>,
    corrected: Histogram<u64>,
    selected: u64,
    overflow: u64,
    maximum_observed_ns: u64,
    correction_interval_ns: Option<u64>,
    significant_figures: u8,
}

impl LaneLatencyRecorder {
    pub fn new(
        sample_probability: f64,
        seed: u64,
        lowest_ns: u64,
        highest_ns: u64,
        significant_figures: u8,
        coordinated_omission_correction_ns: Option<u64>,
    ) -> io::Result<Self> {
        if !(sample_probability > 0.0 && sample_probability <= 1.0) {
            return Err(invalid("latency sample probability must be in (0, 1]"));
        }
        if coordinated_omission_correction_ns == Some(0) {
            return Err(invalid("coordinated-omission interval must be nonzero"));
        }
        let raw = Histogram::<u64>::new_with_bounds(lowest_ns, highest_ns, significant_figures)
            .map_err(|error| invalid(format!("create latency histogram: {error}")))?;
        let corrected = Histogram::<u64>::new_from(&raw);
        let mut recorder = Self {
            probability: sample_probability,
            failures_until_sample: 0,
            rng: seed.max(1),
            raw,
            corrected,
            selected: 0,
            overflow: 0,
            maximum_observed_ns: 0,
            correction_interval_ns: coordinated_omission_correction_ns,
            significant_figures,
        };
        recorder.failures_until_sample = recorder.draw_failures();
        Ok(recorder)
    }

    /// The clock closure is never evaluated for an unsampled operation.
    #[inline]
    pub fn begin<F>(&mut self, clock_ns: F) -> Option<LatencyTicket>
    where
        F: FnOnce() -> u64,
    {
        if self.failures_until_sample != 0 {
            self.failures_until_sample -= 1;
            return None;
        }
        self.failures_until_sample = self.draw_failures();
        self.selected = self.selected.saturating_add(1);
        Some(LatencyTicket {
            started_ns: clock_ns(),
        })
    }

    /// Call only when `begin` returned a ticket, so an unsampled completion has
    /// no clock read or histogram work.
    #[inline]
    pub fn complete<F>(&mut self, ticket: LatencyTicket, clock_ns: F)
    where
        F: FnOnce() -> u64,
    {
        let latency = clock_ns().saturating_sub(ticket.started_ns);
        self.maximum_observed_ns = self.maximum_observed_ns.max(latency);
        if self.raw.record(latency).is_err() {
            self.overflow = self.overflow.saturating_add(1);
            self.raw.saturating_record(latency);
        }
        let corrected_result = match self.correction_interval_ns {
            Some(interval) => self.corrected.record_correct(latency, interval),
            None => self.corrected.record(latency),
        };
        if corrected_result.is_err() {
            self.corrected.saturating_record(latency);
        }
    }

    /// Materialization allocates and iterates bins; invoke it only after the
    /// lane has rotated this recorder out at an interval/grant boundary.
    pub fn finish(
        self,
        subject: impl Into<String>,
        lane: u32,
        completion_class: CompletionClass,
        interval_start_ns: u64,
        interval_end_ns: u64,
        clock_source: impl Into<String>,
        clock_resolution_ns: u64,
        clock_read_overhead_ns: u64,
    ) -> LatencyMetricPoint {
        let confidence_level = 0.95;
        let quantile_rank_error = dkw_rank_error(self.raw.len(), confidence_level);
        let probability = self.probability;
        let bins = latency_bins(&self.raw, probability);
        let corrected_bins = latency_bins(&self.corrected, probability);
        let quantiles = [0.5, 0.9, 0.99, 0.999, 0.9999]
            .into_iter()
            .map(|quantile| LatencyQuantile {
                quantile,
                estimate_ns: self.raw.value_at_quantile(quantile),
                confidence_lower_ns: self
                    .raw
                    .value_at_quantile((quantile - quantile_rank_error).max(0.0)),
                confidence_upper_ns: self
                    .raw
                    .value_at_quantile((quantile + quantile_rank_error).min(1.0)),
            })
            .collect();
        LatencyMetricPoint {
            subject: subject.into(),
            lane,
            completion_class,
            interval_start_ns,
            interval_end_ns,
            samples: self.raw.len(),
            estimated_operations: self.raw.len() as f64 / probability,
            sample_probability: probability,
            confidence_level,
            quantile_rank_error,
            histogram_significant_figures: self.significant_figures,
            clock_source: clock_source.into(),
            clock_resolution_ns,
            clock_read_overhead_ns,
            overflow_samples: self.overflow,
            maximum_observed_ns: self.maximum_observed_ns,
            coordinated_omission_correction_ns: self.correction_interval_ns,
            bins,
            corrected_bins,
            quantiles,
        }
    }

    fn draw_failures(&mut self) -> u64 {
        if self.probability == 1.0 {
            return 0;
        }
        self.rng = splitmix64(self.rng);
        // A midpoint conversion cannot produce either endpoint, keeping both
        // logarithms finite. Geometric(p) is the number of failures preceding
        // the next selected operation.
        let uniform = ((self.rng >> 11) as f64 + 0.5) / ((1u64 << 53) as f64);
        (uniform.ln() / (-self.probability).ln_1p())
            .floor()
            .clamp(0.0, u64::MAX as f64) as u64
    }
}

fn latency_bins(histogram: &Histogram<u64>, probability: f64) -> Vec<LatencyBin> {
    histogram
        .iter_recorded()
        .map(|entry| LatencyBin {
            upper_bound_ns: entry.value_iterated_to(),
            samples: entry.count_at_value(),
            estimated_operations: entry.count_at_value() as f64 / probability,
        })
        .collect()
}

fn dkw_rank_error(samples: u64, confidence_level: f64) -> f64 {
    if samples == 0 {
        return 1.0;
    }
    let alpha = 1.0 - confidence_level;
    ((2.0 / alpha).ln() / (2.0 * samples as f64))
        .sqrt()
        .min(1.0)
}

#[derive(Default)]
pub struct LatencyMetricsStream {
    subscribers: Mutex<Vec<SyncSender<LatencyMetricPoint>>>,
}

impl LatencyMetricsStream {
    pub fn subscribe(&self) -> Receiver<LatencyMetricPoint> {
        let (sender, receiver) = mpsc::sync_channel(8);
        self.subscribers
            .lock()
            .expect("latency subscriber mutex poisoned")
            .push(sender);
        receiver
    }

    pub fn publish_from_collector(&self, point: LatencyMetricPoint) {
        self.subscribers
            .lock()
            .expect("latency subscriber mutex poisoned")
            .retain(|subscriber| match subscriber.try_send(point.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
            });
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricPoint {
    pub subject: String,
    pub interval_start_ns: u64,
    pub interval_end_ns: u64,
    pub iops: f64,
    pub operations: u64,
    pub precision: Precision,
}

/// Fanout is used only by the collector thread. Slow subscribers are removed;
/// the data plane never sends, allocates, or locks for metric delivery.
#[derive(Default)]
pub struct MetricsStream {
    subscribers: Mutex<Vec<SyncSender<MetricPoint>>>,
}

impl MetricsStream {
    pub fn subscribe(&self) -> Receiver<MetricPoint> {
        let (sender, receiver) = mpsc::sync_channel(8);
        self.subscribers
            .lock()
            .expect("metrics subscriber mutex poisoned")
            .push(sender);
        receiver
    }

    pub fn publish_from_collector(&self, point: MetricPoint) {
        self.subscribers
            .lock()
            .expect("metrics subscriber mutex poisoned")
            .retain(|subscriber| match subscriber.try_send(point.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
            });
    }
}

/// Design-based batch sampler for high-cardinality subjects. The control plane
/// sets `sample_shift=0` below the exact threshold. At high rates, each whole
/// homogeneous batch is selected with probability `1 / 2^sample_shift`; the
/// Horvitz-Thompson total is unbiased and includes an estimated 95% relative
/// error. Sampling a batch avoids touching a per-session counter for every I/O.
pub struct AdaptiveBatchSampler {
    sample_shift: u8,
    seed: u64,
    sampled_operations: u128,
    sampled_squared_operations: u128,
}

impl AdaptiveBatchSampler {
    pub fn new(sample_shift: u8, seed: u64) -> io::Result<Self> {
        if sample_shift > 30 {
            return Err(invalid("metric sample shift must be at most 30"));
        }
        Ok(Self {
            sample_shift,
            seed,
            sampled_operations: 0,
            sampled_squared_operations: 0,
        })
    }

    pub fn observe_batch(&mut self, batch_sequence: u64, operations: u32) {
        let mask = (1u64 << self.sample_shift).saturating_sub(1);
        if splitmix64(batch_sequence ^ self.seed) & mask != 0 {
            return;
        }
        let operations = u128::from(operations);
        self.sampled_operations = self.sampled_operations.saturating_add(operations);
        self.sampled_squared_operations = self
            .sampled_squared_operations
            .saturating_add(operations.saturating_mul(operations));
    }

    pub fn finish(self, subject: impl Into<String>, start_ns: u64, end_ns: u64) -> MetricPoint {
        let expansion = 1u128 << self.sample_shift;
        let estimated = self.sampled_operations.saturating_mul(expansion);
        let operations = estimated.min(u128::from(u64::MAX)) as u64;
        let elapsed = end_ns.saturating_sub(start_ns);
        let iops = if elapsed == 0 {
            0.0
        } else {
            operations as f64 * 1_000_000_000.0 / elapsed as f64
        };
        let probability = 1.0 / expansion as f64;
        let variance = if self.sample_shift == 0 {
            0.0
        } else {
            (1.0 - probability) * self.sampled_squared_operations as f64
                / (probability * probability)
        };
        let relative_error_95 = if estimated == 0 {
            f64::INFINITY
        } else {
            1.96 * variance.sqrt() / estimated as f64
        };
        MetricPoint {
            subject: subject.into(),
            interval_start_ns: start_ns,
            interval_end_ns: end_ns,
            iops,
            operations,
            precision: Precision {
                exact: self.sample_shift == 0,
                sample_probability: probability,
                relative_error_95,
            },
        }
    }
}

/// Select exact batch accounting below `exact_below_iops`, then the smallest
/// power-of-two sampling probability that keeps the expected update rate at or
/// below `target_sample_batches_per_second`.
pub fn adaptive_sample_shift(
    estimated_iops: u64,
    average_batch_ops: u32,
    exact_below_iops: u64,
    target_sample_batches_per_second: u64,
) -> u8 {
    if estimated_iops <= exact_below_iops || average_batch_ops == 0 {
        return 0;
    }
    let batches = estimated_iops / u64::from(average_batch_ops);
    let target = target_sample_batches_per_second.max(1);
    let ratio = div_ceil(u128::from(batches), u128::from(target));
    let mut expansion = 1u128;
    let mut shift = 0u8;
    while expansion < ratio && shift < 30 {
        expansion <<= 1;
        shift += 1;
    }
    shift
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

pub fn exact_metric_point(
    subject: impl Into<String>,
    start_ns: u64,
    end_ns: u64,
    operations: u64,
) -> MetricPoint {
    let elapsed = end_ns.saturating_sub(start_ns);
    let iops = if elapsed == 0 {
        0.0
    } else {
        operations as f64 * 1_000_000_000.0 / elapsed as f64
    };
    MetricPoint {
        subject: subject.into(),
        interval_start_ns: start_ns,
        interval_end_ns: end_ns,
        iops,
        operations,
        precision: Precision {
            exact: true,
            sample_probability: 1.0,
            relative_error_95: 0.0,
        },
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(id: &str) -> ResourceClaim {
        ResourceClaim {
            resource_id: id.into(),
            iops_cost_ppm: PPM as u32,
            bytes_per_io: 4096,
        }
    }

    #[test]
    fn reserves_foreground_and_deadline_work_on_shared_pcie_path() {
        let resources = vec![CapacityResource {
            id: "pcie-root-0/lane-set-0".into(),
            kind: ResourceKind::PcieLane,
            measured_iops: 20_000_000,
            measured_bytes_per_second: 100_000_000_000,
            safety_margin_ppm: 100_000,
        }];
        let workloads = vec![WorkloadPolicy {
            id: "database".into(),
            class: WorkloadClass::Foreground,
            guaranteed_iops: Some(12_000_000),
            burst_iops: 12_000_000,
            burst_seconds: 0,
            retention_ppm: 900_000,
            lanes: 32,
            path: vec![claim("pcie-root-0/lane-set-0")],
        }];
        let maintenance = vec![MaintenanceObjective {
            id: "snapshot-100g-in-100s".into(),
            class: WorkloadClass::Snapshot,
            bytes_to_process: 100 * 1024 * 1024 * 1024,
            deadline_seconds: 100,
            average_io_bytes: 4096,
            amplification_ppm: PPM as u32,
            scenario_group: Some("bulk-maintenance".into()),
            path: vec![claim("pcie-root-0/lane-set-0")],
        }];
        let plan = plan_admission(7, &resources, &workloads, &maintenance).unwrap();
        assert_eq!(plan.resources[0].usable_iops, 18_000_000);
        assert_eq!(plan.resources[0].foreground_reserved_iops, 12_000_000);
        assert_eq!(plan.resources[0].maintenance_reserved_iops, 262_144);
        assert_eq!(plan.resources[0].spare_iops, 5_737_856);
    }

    #[test]
    fn rejects_overcommit_before_publication() {
        let resources = vec![CapacityResource {
            id: "nic0".into(),
            kind: ResourceKind::Nic,
            measured_iops: 1_000_000,
            measured_bytes_per_second: 5_000_000_000,
            safety_margin_ppm: 0,
        }];
        let workloads = vec![WorkloadPolicy {
            id: "provisioned".into(),
            class: WorkloadClass::Foreground,
            guaranteed_iops: Some(1_000_000),
            burst_iops: 5_000_000,
            burst_seconds: 10,
            retention_ppm: 900_000,
            lanes: 4,
            path: vec![claim("nic0")],
        }];
        let maintenance = vec![MaintenanceObjective {
            id: "snapshot".into(),
            class: WorkloadClass::Snapshot,
            bytes_to_process: 4096,
            deadline_seconds: 1,
            average_io_bytes: 4096,
            amplification_ppm: PPM as u32,
            scenario_group: None,
            path: vec![claim("nic0")],
        }];
        let error = plan_admission(1, &resources, &workloads, &maintenance).unwrap_err();
        assert!(error.to_string().contains("cannot guarantee plan"));
    }

    #[test]
    fn alternative_maintenance_scenarios_reserve_the_maximum() {
        let resources = vec![CapacityResource {
            id: "device".into(),
            kind: ResourceKind::StorageDevice,
            measured_iops: 1_000_000,
            measured_bytes_per_second: 10_000_000_000,
            safety_margin_ppm: 0,
        }];
        let objectives = [100_000, 250_000]
            .into_iter()
            .enumerate()
            .map(|(index, iops)| MaintenanceObjective {
                id: format!("objective-{index}"),
                class: WorkloadClass::Snapshot,
                bytes_to_process: iops * 4096,
                deadline_seconds: 1,
                average_io_bytes: 4096,
                amplification_ppm: PPM as u32,
                scenario_group: Some("one-at-a-time".into()),
                path: vec![claim("device")],
            })
            .collect::<Vec<_>>();
        let plan = plan_admission(1, &resources, &[], &objectives).unwrap();
        assert_eq!(plan.resources[0].maintenance_reserved_iops, 250_000);
    }

    #[test]
    fn lane_limiter_accounts_batches_exactly_and_refreshes_live() {
        let initial = LaneBudgetSnapshot {
            generation: 1,
            sustained_iops: 100_000,
            peak_iops: 1_000_000,
            burst_ops: 100,
            quantum_ops: 100,
            metric_publish_ns: 100_000_000,
        };
        let mailbox = LaneBudgetMailbox::new(initial);
        let mut limiter = LaneLimiter::new(0, initial);
        assert_eq!(limiter.admit(0, 128), 100);
        assert_eq!(limiter.admit(10_000, 8), 1);
        mailbox.publish(LaneBudgetSnapshot {
            generation: 2,
            sustained_iops: 1_000_000,
            peak_iops: 2_000_000,
            burst_ops: 1_000,
            quantum_ops: 1_000,
            metric_publish_ns: 50_000_000,
        });
        assert!(limiter.refresh(20_000, &mailbox));
        assert_eq!(limiter.budget().sustained_iops, 1_000_000);
        assert_eq!(limiter.admit(1_020_000, 1_000), 1_000);
    }

    #[test]
    fn completion_hot_path_is_local_until_publication() {
        let publication = MetricPublication::default();
        let mut metrics = LaneMetrics::default();
        for _ in 0..10 {
            metrics.complete_batch(256, 256 * 4096);
        }
        assert_eq!(publication.load().completed_ops, 0);
        assert!(metrics.maybe_publish(100_000_000, 100_000_000, &publication));
        assert_eq!(publication.load().completed_ops, 2560);
        assert!(!metrics.maybe_publish(100_000_001, 100_000_000, &publication));
    }

    #[test]
    fn metrics_subscription_is_off_the_data_path() {
        let stream = MetricsStream::default();
        let subscriber = stream.subscribe();
        let point = exact_metric_point("volume/a", 1_000_000_000, 1_100_000_000, 500_000);
        stream.publish_from_collector(point.clone());
        assert_eq!(subscriber.recv().unwrap(), point);
        assert_eq!(point.iops, 5_000_000.0);
        assert!(point.precision.exact);
    }

    #[test]
    fn high_rate_session_sampling_reports_precision() {
        let shift = adaptive_sample_shift(12_000_000, 32, 100_000, 10_000);
        assert_eq!(shift, 6);
        let mut sampler = AdaptiveBatchSampler::new(shift, 41).unwrap();
        for sequence in 0..100_000 {
            sampler.observe_batch(sequence, 32);
        }
        let point = sampler.finish("session/a", 0, 1_000_000_000);
        assert!(!point.precision.exact);
        assert_eq!(point.precision.sample_probability, 1.0 / 64.0);
        assert!((point.operations as i64 - 3_200_000).unsigned_abs() < 200_000);
        assert!(point.precision.relative_error_95 < 0.06);
    }

    #[test]
    fn low_rate_session_accounting_is_exact() {
        let shift = adaptive_sample_shift(99_999, 1, 100_000, 10_000);
        let mut sampler = AdaptiveBatchSampler::new(shift, 0).unwrap();
        for sequence in 0..1000 {
            sampler.observe_batch(sequence, 1);
        }
        let point = sampler.finish("session/low", 0, 1_000_000_000);
        assert_eq!(point.operations, 1000);
        assert!(point.precision.exact);
        assert_eq!(point.precision.relative_error_95, 0.0);
    }

    /// Arithmetic-only diagnostic. This is not a block, transport, or durable
    /// IOPS benchmark and must never be reported as one.
    #[test]
    #[ignore]
    fn lane_batch_admission_microbenchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        const BATCHES: u64 = 20_000_000;
        const OPS_PER_BATCH: u32 = 256;
        let budget = LaneBudgetSnapshot {
            generation: 1,
            sustained_iops: 100_000_000_000,
            peak_iops: 100_000_000_000,
            burst_ops: 1_000_000,
            quantum_ops: 1_000_000,
            metric_publish_ns: 100_000_000,
        };
        let mut limiter = LaneLimiter::new(0, budget);
        let mut now_ns = 0u64;
        let mut admitted = 0u64;
        let started = Instant::now();
        for _ in 0..BATCHES {
            now_ns = now_ns.wrapping_add(3);
            admitted = admitted.saturating_add(u64::from(
                limiter.admit(black_box(now_ns), black_box(OPS_PER_BATCH)),
            ));
        }
        let elapsed = started.elapsed().as_secs_f64();
        assert_eq!(admitted, BATCHES * u64::from(OPS_PER_BATCH));
        eprintln!(
            "lane-batch-admission-arithmetic-only batches_per_sec={:.0} logical_ops_per_sec={:.0} ns_per_batch={:.2}",
            BATCHES as f64 / elapsed,
            admitted as f64 / elapsed,
            elapsed * 1e9 / BATCHES as f64,
        );
    }

    #[test]
    fn latency_sampling_never_reads_clock_for_an_unsampled_io() {
        use std::cell::Cell;

        let mut recorder =
            LaneLatencyRecorder::new(1.0 / 1024.0, 91, 1, 1_000_000, 3, None).unwrap();
        let clocks = Cell::new(0u64);
        let mut samples = 0u64;
        for operation in 0..1_000_000u64 {
            let ticket = recorder.begin(|| {
                clocks.set(clocks.get() + 1);
                operation * 100
            });
            if let Some(ticket) = ticket {
                samples += 1;
                recorder.complete(ticket, || {
                    clocks.set(clocks.get() + 1);
                    operation * 100 + 50
                });
            }
        }
        assert_eq!(clocks.get(), samples * 2);
        let point = recorder.finish(
            "volume/a",
            0,
            CompletionClass::RemoteRead,
            0,
            1_000_000_000,
            "test-clock",
            1,
            0,
        );
        assert!((point.estimated_operations - 1_000_000.0).abs() < 100_000.0);
        assert_eq!(point.samples, samples);
    }

    #[test]
    fn latency_histogram_reports_distribution_free_quantile_bounds() {
        let mut recorder = LaneLatencyRecorder::new(1.0, 1, 1, 1_000_000, 3, Some(100)).unwrap();
        for latency in 1..=10_000u64 {
            let ticket = recorder.begin(|| 1).unwrap();
            recorder.complete(ticket, || 1 + latency);
        }
        let point = recorder.finish(
            "volume/exact",
            7,
            CompletionClass::SyncDrain,
            0,
            1_000_000,
            "monotonic_raw",
            1,
            20,
        );
        assert_eq!(point.samples, 10_000);
        assert_eq!(
            point.bins.iter().map(|bin| bin.samples).sum::<u64>(),
            10_000
        );
        assert!(
            point
                .corrected_bins
                .iter()
                .map(|bin| bin.samples)
                .sum::<u64>()
                > 10_000
        );
        assert!(point.quantile_rank_error < 0.014);
        let p99 = point
            .quantiles
            .iter()
            .find(|quantile| quantile.quantile == 0.99)
            .unwrap();
        assert!(p99.confidence_lower_ns <= p99.estimate_ns);
        assert!(p99.estimate_ns <= p99.confidence_upper_ns);
        assert_eq!(point.clock_read_overhead_ns, 20);
    }

    #[test]
    fn latency_subscription_is_collector_only_and_bounded() {
        let stream = LatencyMetricsStream::default();
        let receiver = stream.subscribe();
        let mut recorder = LaneLatencyRecorder::new(1.0, 1, 1, 1000, 2, None).unwrap();
        let ticket = recorder.begin(|| 100).unwrap();
        recorder.complete(ticket, || 200);
        let point = recorder.finish(
            "session/a",
            1,
            CompletionClass::DurableWrite,
            0,
            1000,
            "test",
            1,
            0,
        );
        stream.publish_from_collector(point.clone());
        assert_eq!(receiver.recv().unwrap(), point);
    }

    /// Sampling-decision diagnostic only; not an I/O latency benchmark.
    #[test]
    #[ignore]
    fn latency_sampling_decision_microbenchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        const OPERATIONS: u64 = 100_000_000;
        let mut recorder =
            LaneLatencyRecorder::new(1.0 / 1024.0, 17, 1, 1_000_000, 3, None).unwrap();
        let mut samples = 0u64;
        let started = Instant::now();
        for operation in 0..OPERATIONS {
            if let Some(ticket) = recorder.begin(|| black_box(operation)) {
                samples += 1;
                recorder.complete(ticket, || black_box(operation + 100));
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        eprintln!(
            "latency-sampling-decision-only operations_per_sec={:.0} ns_per_operation={:.3} samples={} probability={}",
            OPERATIONS as f64 / elapsed,
            elapsed * 1e9 / OPERATIONS as f64,
            samples,
            1.0 / 1024.0,
        );
        assert!((samples as f64 - OPERATIONS as f64 / 1024.0).abs() < 2000.0);
    }
}
