//! Deadline-aware sharing of a volume's system-operations budget.
//!
//! The regional/volume HTB grants one aggregate maintenance budget after
//! foreground guarantees have been protected. This controller runs outside
//! the I/O path and divides that budget between snapshots, live migration,
//! recovery, replication catch-up, and housekeeping. Lane/copy workers only
//! read a generation mailbox at an existing batch or copy-chunk boundary.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// Read the monotonic clock used by grant activation and lease expiry. This
/// is intentionally sampled by management controllers and copy workers at a
/// chunk boundary, never by descriptor admission or completion.
pub fn monotonic_time_ns() -> io::Result<u64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let seconds = u64::try_from(value.tv_sec)
        .map_err(|_| invalid("monotonic clock returned a negative second count"))?;
    let nanoseconds = u64::try_from(value.tv_nsec)
        .map_err(|_| invalid("monotonic clock returned a negative nanosecond count"))?;
    seconds
        .checked_mul(NANOS_PER_SECOND as u64)
        .and_then(|base| base.checked_add(nanoseconds))
        .ok_or_else(|| invalid("monotonic clock overflow"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemTaskKind {
    Snapshot,
    LiveMigration,
    Restore,
    ReplicationCatchup,
    Scrub,
    Compaction,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyMode {
    /// The downstream task cannot consume capacity until this task completes.
    CompleteBeforeStart,
    /// Both stages may run concurrently. The upstream stage is still funded
    /// first when both contribute to the same deadline.
    Pipelined,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemTaskDependency {
    pub task_id: String,
    pub mode: DependencyMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemTaskSpec {
    pub id: String,
    pub kind: SystemTaskKind,
    /// Remaining logical bytes. Zero marks the task complete.
    pub remaining_bytes: u64,
    /// Average operation size used to charge both IOPS and byte capacity.
    pub average_io_bytes: u64,
    /// A non-preemptible floor, normally an RPO/compliance promise.
    pub protected_iops: u64,
    /// The ordinary provisioned system-task rate.
    pub provisioned_iops: u64,
    /// Absolute limit after deadline escalation and borrowing.
    pub ceiling_iops: u64,
    /// Observed queued/runnable demand for the next control interval.
    pub demand_iops: u64,
    pub borrow_weight: u32,
    pub active: bool,
    #[serde(default)]
    pub dependencies: Vec<SystemTaskDependency>,
}

impl SystemTaskSpec {
    fn complete(&self) -> bool {
        self.remaining_bytes == 0
    }

    fn operations_remaining(&self) -> u128 {
        div_ceil(
            u128::from(self.remaining_bytes),
            u128::from(self.average_io_bytes),
        )
    }

    fn runnable(&self, tasks: &BTreeMap<String, SystemTaskSpec>) -> bool {
        self.active
            && !self.complete()
            && self.dependencies.iter().all(|dependency| {
                dependency.mode == DependencyMode::Pipelined
                    || tasks[&dependency.task_id].complete()
            })
    }

    fn limit(&self) -> u64 {
        self.demand_iops.min(self.ceiling_iops)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveKind {
    RecoveryTime,
    RecoveryPoint,
    Compliance,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemObjective {
    pub id: String,
    pub kind: ObjectiveKind,
    /// Absolute time in the controller's monotonic clock domain.
    pub deadline_ns: u64,
    /// Completion of every terminal is required to satisfy the objective.
    pub terminal_task_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSystemBudget {
    pub volume_id: String,
    pub policy_revision: u64,
    /// Aggregate child grant received from the volume/system HTB class.
    pub granted_iops: u64,
    pub granted_bytes_per_second: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemTaskGrant {
    pub task_id: String,
    pub kind: SystemTaskKind,
    pub target_iops: u64,
    pub target_bytes_per_second: u64,
    pub average_io_bytes: u64,
    pub protected_iops: u64,
    pub provisioned_iops: u64,
    pub borrowed_iops: u64,
    pub critical_objectives: Vec<String>,
    pub blocking_critical_tasks: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectiveAssessment {
    pub objective_id: String,
    pub kind: ObjectiveKind,
    pub deadline_ns: u64,
    pub predicted_completion_ns: Option<u64>,
    /// Saturated signed value: positive is spare time, negative is late.
    pub slack_ns: i64,
    pub feasible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSystemGrantPlan {
    pub generation: u64,
    pub policy_revision: u64,
    pub volume_id: String,
    pub observed_ns: u64,
    pub effective_ns: u64,
    pub granted_iops: u64,
    pub granted_bytes_per_second: u64,
    pub assigned_iops: u64,
    pub assigned_bytes_per_second: u64,
    pub task_grants: Vec<SystemTaskGrant>,
    pub objectives: Vec<ObjectiveAssessment>,
}

#[derive(Clone, Debug)]
struct CriticalTask {
    objectives: BTreeSet<String>,
    earliest_deadline_ns: u64,
    required_iops: u64,
    blocking_tasks: u32,
}

impl Default for CriticalTask {
    fn default() -> Self {
        Self {
            objectives: BTreeSet::new(),
            earliest_deadline_ns: u64::MAX,
            required_iops: 0,
            blocking_tasks: 0,
        }
    }
}

/// Computes one deterministic system-task generation. Call it from the
/// management controller, never from descriptor admission or completion.
pub fn plan_volume_system_tasks(
    generation: u64,
    observed_ns: u64,
    effective_ns: u64,
    budget: &VolumeSystemBudget,
    task_specs: &[SystemTaskSpec],
    objectives: &[SystemObjective],
) -> io::Result<VolumeSystemGrantPlan> {
    if generation == 0 || budget.policy_revision == 0 || budget.volume_id.is_empty() {
        return Err(invalid(
            "system-task generation, policy revision, and volume id must be nonzero",
        ));
    }
    if effective_ns < observed_ns {
        return Err(invalid("system-task effective time precedes observation"));
    }
    let tasks = validate_tasks(task_specs)?;
    validate_objectives(objectives, &tasks)?;
    let critical = critical_tasks(observed_ns, objectives, &tasks)?;
    let runnable = tasks
        .iter()
        .filter(|(_, task)| task.runnable(&tasks))
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();

    let mut targets = runnable
        .iter()
        .map(|id| (id.clone(), 0u64))
        .collect::<BTreeMap<_, _>>();
    let mut remaining_iops = budget.granted_iops;
    let mut remaining_bytes = budget.granted_bytes_per_second;

    // Hard task promises are protected before all deadline escalation. The
    // aggregate parent budget has already been admitted against foreground.
    for id in &runnable {
        let task = &tasks[id];
        let floor = task.protected_iops.min(task.limit());
        grant_task(
            task,
            id,
            floor,
            &mut targets,
            &mut remaining_iops,
            &mut remaining_bytes,
        );
        if targets[id] != floor {
            return Err(invalid(format!(
                "volume {} system grant cannot protect task {}: requested_iops={} available_iops={} available_bytes_per_second={}",
                budget.volume_id, id, floor, budget.granted_iops, budget.granted_bytes_per_second
            )));
        }
    }

    // Deadline work wins before ordinary provisioned rates. An upstream task
    // sorts ahead of a dependent task, so a required snapshot receives its
    // RTO rate and a pipelined migration consumes the remainder. If that
    // snapshot is absent from the active objective, migration sorts alone and
    // can preempt its non-critical provisioned allocation.
    let mut critical_order = runnable
        .iter()
        .filter(|id| critical.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    critical_order.sort_by(|left, right| {
        let a = &critical[left];
        let b = &critical[right];
        b.blocking_tasks
            .cmp(&a.blocking_tasks)
            .then_with(|| a.earliest_deadline_ns.cmp(&b.earliest_deadline_ns))
            .then_with(|| left.cmp(right))
    });
    for id in &critical_order {
        let task = &tasks[id];
        let desired = task
            .provisioned_iops
            .max(critical[id].required_iops)
            .min(task.limit());
        grant_task(
            task,
            id,
            desired,
            &mut targets,
            &mut remaining_iops,
            &mut remaining_bytes,
        );
    }

    // Give every runnable task its ordinary system-task allocation with the
    // capacity left after active deadline work.
    weighted_fill(
        &runnable,
        &tasks,
        &mut targets,
        |task| task.provisioned_iops.min(task.limit()),
        &mut remaining_iops,
        &mut remaining_bytes,
    );
    // Finally borrow all remaining parent capacity toward per-task ceilings.
    weighted_fill(
        &runnable,
        &tasks,
        &mut targets,
        SystemTaskSpec::limit,
        &mut remaining_iops,
        &mut remaining_bytes,
    );

    let task_grants = tasks
        .values()
        .map(|task| {
            let target = targets.get(&task.id).copied().unwrap_or(0);
            let critical_task = critical.get(&task.id);
            SystemTaskGrant {
                task_id: task.id.clone(),
                kind: task.kind,
                target_iops: target,
                target_bytes_per_second: target.saturating_mul(task.average_io_bytes),
                average_io_bytes: task.average_io_bytes,
                protected_iops: task.protected_iops.min(task.limit()),
                provisioned_iops: task.provisioned_iops.min(task.limit()),
                borrowed_iops: target.saturating_sub(task.provisioned_iops.min(task.limit())),
                critical_objectives: critical_task
                    .map(|critical| critical.objectives.iter().cloned().collect())
                    .unwrap_or_default(),
                blocking_critical_tasks: critical_task
                    .map(|critical| critical.blocking_tasks)
                    .unwrap_or(0),
            }
        })
        .collect::<Vec<_>>();
    let objective_assessments = objectives
        .iter()
        .map(|objective| assess_objective(observed_ns, objective, &tasks, &targets))
        .collect::<io::Result<Vec<_>>>()?;
    let assigned_iops = task_grants.iter().map(|grant| grant.target_iops).sum();
    let assigned_bytes_per_second = task_grants
        .iter()
        .map(|grant| grant.target_bytes_per_second)
        .sum();
    Ok(VolumeSystemGrantPlan {
        generation,
        policy_revision: budget.policy_revision,
        volume_id: budget.volume_id.clone(),
        observed_ns,
        effective_ns,
        granted_iops: budget.granted_iops,
        granted_bytes_per_second: budget.granted_bytes_per_second,
        assigned_iops,
        assigned_bytes_per_second,
        task_grants,
        objectives: objective_assessments,
    })
}

fn validate_tasks(task_specs: &[SystemTaskSpec]) -> io::Result<BTreeMap<String, SystemTaskSpec>> {
    let mut tasks = BTreeMap::new();
    for task in task_specs {
        if task.id.is_empty()
            || task.average_io_bytes == 0
            || task.borrow_weight == 0
            || task.provisioned_iops < task.protected_iops
            || task.ceiling_iops < task.provisioned_iops
        {
            return Err(invalid(format!("invalid system task {}", task.id)));
        }
        if tasks.insert(task.id.clone(), task.clone()).is_some() {
            return Err(invalid(format!("duplicate system task {}", task.id)));
        }
    }
    for task in tasks.values() {
        let mut dependencies = BTreeSet::new();
        for dependency in &task.dependencies {
            if dependency.task_id == task.id
                || !tasks.contains_key(&dependency.task_id)
                || !dependencies.insert(dependency.task_id.as_str())
            {
                return Err(invalid(format!(
                    "task {} has an invalid dependency {}",
                    task.id, dependency.task_id
                )));
            }
        }
    }
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for id in tasks.keys() {
        visit_task(id, &tasks, &mut visiting, &mut visited)?;
    }
    Ok(tasks)
}

fn visit_task(
    id: &str,
    tasks: &BTreeMap<String, SystemTaskSpec>,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
) -> io::Result<()> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id.to_string()) {
        return Err(invalid("system-task dependency graph contains a cycle"));
    }
    for dependency in &tasks[id].dependencies {
        visit_task(&dependency.task_id, tasks, visiting, visited)?;
    }
    visiting.remove(id);
    visited.insert(id.to_string());
    Ok(())
}

fn validate_objectives(
    objectives: &[SystemObjective],
    tasks: &BTreeMap<String, SystemTaskSpec>,
) -> io::Result<()> {
    let mut ids = BTreeSet::new();
    for objective in objectives {
        if objective.id.is_empty()
            || objective.deadline_ns == 0
            || objective.terminal_task_ids.is_empty()
            || !ids.insert(objective.id.as_str())
            || objective
                .terminal_task_ids
                .iter()
                .any(|id| !tasks.contains_key(id))
        {
            return Err(invalid(format!(
                "invalid system objective {}",
                objective.id
            )));
        }
    }
    Ok(())
}

fn critical_tasks(
    now_ns: u64,
    objectives: &[SystemObjective],
    tasks: &BTreeMap<String, SystemTaskSpec>,
) -> io::Result<BTreeMap<String, CriticalTask>> {
    let mut critical = BTreeMap::<String, CriticalTask>::new();
    for objective in objectives {
        let mut members = BTreeSet::new();
        for terminal in &objective.terminal_task_ids {
            collect_ancestors(terminal, tasks, &mut members);
        }
        members.retain(|id| !tasks[id].complete());
        if members.is_empty() {
            continue;
        }
        let operations = members.iter().try_fold(0u128, |sum, id| {
            sum.checked_add(tasks[id].operations_remaining())
                .ok_or_else(|| invalid("system objective operation count overflow"))
        })?;
        let available_ns = objective.deadline_ns.saturating_sub(now_ns).max(1);
        let required_iops = u64::try_from(div_ceil(
            operations.saturating_mul(NANOS_PER_SECOND),
            u128::from(available_ns),
        ))
        .unwrap_or(u64::MAX);
        for id in &members {
            let entry = critical.entry(id.clone()).or_default();
            entry.objectives.insert(objective.id.clone());
            entry.earliest_deadline_ns = entry.earliest_deadline_ns.min(objective.deadline_ns);
            entry.required_iops = entry.required_iops.max(required_iops);
            entry.blocking_tasks = entry
                .blocking_tasks
                .max(count_dependent_members(id, &members, tasks));
        }
    }
    Ok(critical)
}

fn collect_ancestors(
    id: &str,
    tasks: &BTreeMap<String, SystemTaskSpec>,
    output: &mut BTreeSet<String>,
) {
    if !output.insert(id.to_string()) {
        return;
    }
    for dependency in &tasks[id].dependencies {
        collect_ancestors(&dependency.task_id, tasks, output);
    }
}

fn count_dependent_members(
    ancestor: &str,
    members: &BTreeSet<String>,
    tasks: &BTreeMap<String, SystemTaskSpec>,
) -> u32 {
    members
        .iter()
        .filter(|candidate| candidate.as_str() != ancestor)
        .filter(|candidate| task_depends_on(candidate, ancestor, tasks, &mut BTreeSet::new()))
        .count()
        .min(u32::MAX as usize) as u32
}

fn task_depends_on(
    id: &str,
    ancestor: &str,
    tasks: &BTreeMap<String, SystemTaskSpec>,
    visited: &mut BTreeSet<String>,
) -> bool {
    if !visited.insert(id.to_string()) {
        return false;
    }
    tasks[id].dependencies.iter().any(|dependency| {
        dependency.task_id == ancestor
            || task_depends_on(&dependency.task_id, ancestor, tasks, visited)
    })
}

fn grant_task(
    task: &SystemTaskSpec,
    id: &str,
    requested_target: u64,
    targets: &mut BTreeMap<String, u64>,
    remaining_iops: &mut u64,
    remaining_bytes: &mut u64,
) {
    let current = targets[id];
    let wanted = requested_target.saturating_sub(current);
    let by_bytes = *remaining_bytes / task.average_io_bytes;
    let grant = wanted.min(*remaining_iops).min(by_bytes);
    *targets.get_mut(id).expect("runnable task target exists") += grant;
    *remaining_iops -= grant;
    *remaining_bytes -= grant.saturating_mul(task.average_io_bytes);
}

fn weighted_fill<F>(
    runnable: &[String],
    tasks: &BTreeMap<String, SystemTaskSpec>,
    targets: &mut BTreeMap<String, u64>,
    limit: F,
    remaining_iops: &mut u64,
    remaining_bytes: &mut u64,
) where
    F: Fn(&SystemTaskSpec) -> u64,
{
    loop {
        let active = runnable
            .iter()
            .filter(|id| targets[*id] < limit(&tasks[*id]))
            .cloned()
            .collect::<Vec<_>>();
        if active.is_empty() || *remaining_iops == 0 || *remaining_bytes == 0 {
            return;
        }
        let total_weight = active
            .iter()
            .map(|id| u64::from(tasks[id].borrow_weight))
            .sum::<u64>();
        let before_iops = *remaining_iops;
        let before_bytes = *remaining_bytes;
        let mut progress = 0u64;
        for id in active {
            let task = &tasks[&id];
            let weight = u64::from(task.borrow_weight);
            let iops_share =
                ((u128::from(before_iops) * u128::from(weight)) / u128::from(total_weight)) as u64;
            let byte_share =
                ((u128::from(before_bytes) * u128::from(weight)) / u128::from(total_weight)) as u64;
            let share = iops_share
                .max(1)
                .min(byte_share.max(task.average_io_bytes) / task.average_io_bytes);
            let before = targets[&id];
            grant_task(
                task,
                &id,
                before.saturating_add(share).min(limit(task)),
                targets,
                remaining_iops,
                remaining_bytes,
            );
            progress = progress.saturating_add(targets[&id].saturating_sub(before));
        }
        if progress == 0 {
            return;
        }
    }
}

fn assess_objective(
    now_ns: u64,
    objective: &SystemObjective,
    tasks: &BTreeMap<String, SystemTaskSpec>,
    targets: &BTreeMap<String, u64>,
) -> io::Result<ObjectiveAssessment> {
    let mut memo = BTreeMap::new();
    let remaining_ns = objective
        .terminal_task_ids
        .iter()
        .map(|id| predicted_task_ns(id, tasks, targets, &mut memo))
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .max()
        .flatten();
    let predicted_completion_ns = remaining_ns.map(|duration| now_ns.saturating_add(duration));
    let slack = match predicted_completion_ns {
        Some(completion) => i128::from(objective.deadline_ns) - i128::from(completion),
        None => i128::MIN,
    };
    let slack_ns = slack.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    Ok(ObjectiveAssessment {
        objective_id: objective.id.clone(),
        kind: objective.kind,
        deadline_ns: objective.deadline_ns,
        predicted_completion_ns,
        slack_ns,
        feasible: predicted_completion_ns.is_some_and(|value| value <= objective.deadline_ns),
    })
}

fn predicted_task_ns(
    id: &str,
    tasks: &BTreeMap<String, SystemTaskSpec>,
    targets: &BTreeMap<String, u64>,
    memo: &mut BTreeMap<String, Option<u64>>,
) -> io::Result<Option<u64>> {
    if let Some(cached) = memo.get(id) {
        return Ok(*cached);
    }
    let task = &tasks[id];
    if task.complete() {
        memo.insert(id.to_string(), Some(0));
        return Ok(Some(0));
    }
    let rate = targets.get(id).copied().unwrap_or(0);
    if rate == 0 {
        memo.insert(id.to_string(), None);
        return Ok(None);
    }
    let own_ns = u64::try_from(div_ceil(
        task.operations_remaining().saturating_mul(NANOS_PER_SECOND),
        u128::from(rate),
    ))
    .unwrap_or(u64::MAX);
    let mut complete_before_ns = 0u64;
    let mut pipelined_ns = 0u64;
    for dependency in &task.dependencies {
        let Some(dependency_ns) = predicted_task_ns(&dependency.task_id, tasks, targets, memo)?
        else {
            memo.insert(id.to_string(), None);
            return Ok(None);
        };
        match dependency.mode {
            DependencyMode::CompleteBeforeStart => {
                complete_before_ns = complete_before_ns.max(dependency_ns)
            }
            DependencyMode::Pipelined => pipelined_ns = pipelined_ns.max(dependency_ns),
        }
    }
    let duration = complete_before_ns
        .saturating_add(own_ns)
        .max(pipelined_ns.max(own_ns));
    memo.insert(id.to_string(), Some(duration));
    Ok(Some(duration))
}

fn div_ceil(value: u128, divisor: u128) -> u128 {
    value / divisor + u128::from(value % divisor != 0)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemTaskGrantSnapshot {
    pub generation: u64,
    pub target_iops: u64,
    pub target_bytes_per_second: u64,
    pub effective_ns: u64,
    pub valid_until_ns: u64,
    pub fallback_iops: u64,
    pub fallback_bytes_per_second: u64,
}

/// Single-controller-writer seqlock. Copy and snapshot workers load this only
/// at a chunk boundary; descriptor admission never touches it.
#[repr(align(64))]
pub struct SystemTaskGrantMailbox {
    sequence: AtomicU64,
    generation: AtomicU64,
    target_iops: AtomicU64,
    target_bytes_per_second: AtomicU64,
    effective_ns: AtomicU64,
    valid_until_ns: AtomicU64,
    fallback_iops: AtomicU64,
    fallback_bytes_per_second: AtomicU64,
}

impl SystemTaskGrantMailbox {
    pub fn new(initial: SystemTaskGrantSnapshot) -> Self {
        Self {
            sequence: AtomicU64::new(2),
            generation: AtomicU64::new(initial.generation),
            target_iops: AtomicU64::new(initial.target_iops),
            target_bytes_per_second: AtomicU64::new(initial.target_bytes_per_second),
            effective_ns: AtomicU64::new(initial.effective_ns),
            valid_until_ns: AtomicU64::new(initial.valid_until_ns),
            fallback_iops: AtomicU64::new(initial.fallback_iops),
            fallback_bytes_per_second: AtomicU64::new(initial.fallback_bytes_per_second),
        }
    }

    pub fn publish(&self, grant: SystemTaskGrantSnapshot) {
        let odd = self.sequence.load(Ordering::Relaxed).wrapping_add(1) | 1;
        self.sequence.store(odd, Ordering::Release);
        self.generation.store(grant.generation, Ordering::Relaxed);
        self.target_iops.store(grant.target_iops, Ordering::Relaxed);
        self.target_bytes_per_second
            .store(grant.target_bytes_per_second, Ordering::Relaxed);
        self.effective_ns
            .store(grant.effective_ns, Ordering::Relaxed);
        self.valid_until_ns
            .store(grant.valid_until_ns, Ordering::Relaxed);
        self.fallback_iops
            .store(grant.fallback_iops, Ordering::Relaxed);
        self.fallback_bytes_per_second
            .store(grant.fallback_bytes_per_second, Ordering::Relaxed);
        self.sequence.store(odd.wrapping_add(1), Ordering::Release);
    }

    pub fn load(&self, now_ns: u64) -> SystemTaskGrantSnapshot {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let mut value = SystemTaskGrantSnapshot {
                generation: self.generation.load(Ordering::Relaxed),
                target_iops: self.target_iops.load(Ordering::Relaxed),
                target_bytes_per_second: self.target_bytes_per_second.load(Ordering::Relaxed),
                effective_ns: self.effective_ns.load(Ordering::Relaxed),
                valid_until_ns: self.valid_until_ns.load(Ordering::Relaxed),
                fallback_iops: self.fallback_iops.load(Ordering::Relaxed),
                fallback_bytes_per_second: self.fallback_bytes_per_second.load(Ordering::Relaxed),
            };
            if before != self.sequence.load(Ordering::Acquire) {
                continue;
            }
            if now_ns < value.effective_ns {
                value.target_iops = 0;
                value.target_bytes_per_second = 0;
            } else if value.valid_until_ns != 0 && now_ns >= value.valid_until_ns {
                value.target_iops = value.fallback_iops;
                value.target_bytes_per_second = value.fallback_bytes_per_second;
            }
            return value;
        }
    }
}

pub struct VolumeSystemGrantPublisher {
    mailboxes: BTreeMap<String, Arc<SystemTaskGrantMailbox>>,
}

impl VolumeSystemGrantPublisher {
    pub fn new() -> Self {
        Self {
            mailboxes: BTreeMap::new(),
        }
    }

    pub fn register(
        &mut self,
        task_id: impl Into<String>,
        mailbox: Arc<SystemTaskGrantMailbox>,
    ) -> io::Result<()> {
        let task_id = task_id.into();
        if task_id.is_empty() || self.mailboxes.insert(task_id, mailbox).is_some() {
            return Err(invalid("empty or duplicate system-task mailbox"));
        }
        Ok(())
    }

    pub fn publish(&self, plan: &VolumeSystemGrantPlan, valid_until_ns: u64) -> io::Result<()> {
        if valid_until_ns != 0 && valid_until_ns <= plan.effective_ns {
            return Err(invalid("system-task lease expires before activation"));
        }
        for grant in &plan.task_grants {
            let mailbox = self
                .mailboxes
                .get(&grant.task_id)
                .ok_or_else(|| invalid(format!("system task {} has no mailbox", grant.task_id)))?;
            mailbox.publish(SystemTaskGrantSnapshot {
                generation: plan.generation,
                target_iops: grant.target_iops,
                target_bytes_per_second: grant.target_bytes_per_second,
                effective_ns: plan.effective_ns,
                valid_until_ns,
                fallback_iops: grant.protected_iops,
                fallback_bytes_per_second: grant
                    .protected_iops
                    .saturating_mul(grant.average_io_bytes),
            });
        }
        Ok(())
    }
}

impl Default for VolumeSystemGrantPublisher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, kind: SystemTaskKind) -> SystemTaskSpec {
        SystemTaskSpec {
            id: id.into(),
            kind,
            remaining_bytes: 4_096_000_000,
            average_io_bytes: 4096,
            protected_iops: 100_000,
            provisioned_iops: 1_000_000,
            ceiling_iops: 8_000_000,
            demand_iops: 8_000_000,
            borrow_weight: 1,
            active: true,
            dependencies: Vec::new(),
        }
    }

    fn budget() -> VolumeSystemBudget {
        VolumeSystemBudget {
            volume_id: "volume-a".into(),
            policy_revision: 7,
            granted_iops: 6_000_000,
            granted_bytes_per_second: 40_000_000_000,
        }
    }

    fn grant<'a>(plan: &'a VolumeSystemGrantPlan, id: &str) -> &'a SystemTaskGrant {
        plan.task_grants
            .iter()
            .find(|grant| grant.task_id == id)
            .unwrap()
    }

    #[test]
    fn required_snapshot_is_funded_before_its_pipelined_migration() {
        let snapshot = task("snapshot", SystemTaskKind::Snapshot);
        let mut migration = task("migration", SystemTaskKind::LiveMigration);
        migration.dependencies.push(SystemTaskDependency {
            task_id: "snapshot".into(),
            mode: DependencyMode::Pipelined,
        });
        let plan = plan_volume_system_tasks(
            1,
            1_000_000_000,
            1_100_000_000,
            &budget(),
            &[snapshot, migration],
            &[SystemObjective {
                id: "recover".into(),
                kind: ObjectiveKind::RecoveryTime,
                deadline_ns: 1_500_000_000,
                terminal_task_ids: vec!["migration".into()],
            }],
        )
        .unwrap();
        assert!(grant(&plan, "snapshot").target_iops > grant(&plan, "migration").target_iops);
        assert_eq!(grant(&plan, "snapshot").blocking_critical_tasks, 1);
        assert_eq!(plan.assigned_iops, 6_000_000);
    }

    #[test]
    fn rto_migration_preempts_an_unrelated_snapshot_above_its_hard_floor() {
        let snapshot = task("snapshot", SystemTaskKind::Snapshot);
        let migration = task("migration", SystemTaskKind::LiveMigration);
        let plan = plan_volume_system_tasks(
            2,
            1_000_000_000,
            1_100_000_000,
            &budget(),
            &[snapshot, migration],
            &[SystemObjective {
                id: "move-before-outage".into(),
                kind: ObjectiveKind::RecoveryTime,
                deadline_ns: 1_250_000_000,
                terminal_task_ids: vec!["migration".into()],
            }],
        )
        .unwrap();
        assert!(grant(&plan, "migration").target_iops > grant(&plan, "snapshot").target_iops);
        assert!(grant(&plan, "snapshot").target_iops >= 100_000);
        assert_eq!(
            grant(&plan, "snapshot").critical_objectives,
            Vec::<String>::new()
        );
    }

    #[test]
    fn incomplete_completion_dependency_keeps_downstream_off_the_lanes() {
        let snapshot = task("snapshot", SystemTaskKind::Snapshot);
        let mut migration = task("migration", SystemTaskKind::LiveMigration);
        migration.dependencies.push(SystemTaskDependency {
            task_id: "snapshot".into(),
            mode: DependencyMode::CompleteBeforeStart,
        });
        let plan =
            plan_volume_system_tasks(3, 1, 2, &budget(), &[snapshot, migration], &[]).unwrap();
        assert_eq!(grant(&plan, "migration").target_iops, 0);
        assert_eq!(grant(&plan, "snapshot").target_iops, 6_000_000);
    }

    #[test]
    fn impossible_deadline_is_reported_without_stealing_foreground_capacity() {
        let mut migration = task("migration", SystemTaskKind::LiveMigration);
        migration.remaining_bytes = 1_000_000_000_000;
        let plan = plan_volume_system_tasks(
            4,
            1_000,
            2_000,
            &budget(),
            &[migration],
            &[SystemObjective {
                id: "impossible".into(),
                kind: ObjectiveKind::RecoveryTime,
                deadline_ns: 2_000,
                terminal_task_ids: vec!["migration".into()],
            }],
        )
        .unwrap();
        assert!(!plan.objectives[0].feasible);
        assert_eq!(plan.assigned_iops, budget().granted_iops);
    }

    #[test]
    fn task_mailbox_activates_and_expires_without_a_shared_lock() {
        let initial = SystemTaskGrantSnapshot {
            generation: 1,
            target_iops: 10,
            target_bytes_per_second: 40960,
            effective_ns: 0,
            valid_until_ns: 0,
            fallback_iops: 10,
            fallback_bytes_per_second: 40960,
        };
        let mailbox = SystemTaskGrantMailbox::new(initial);
        mailbox.publish(SystemTaskGrantSnapshot {
            generation: 2,
            target_iops: 100,
            target_bytes_per_second: 409600,
            effective_ns: 1000,
            valid_until_ns: 2000,
            fallback_iops: 20,
            fallback_bytes_per_second: 81920,
        });
        assert_eq!(mailbox.load(999).target_iops, 0);
        assert_eq!(mailbox.load(1000).target_iops, 100);
        assert_eq!(mailbox.load(2000).target_iops, 20);
    }
}
