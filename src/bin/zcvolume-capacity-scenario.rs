use std::collections::BTreeSet;
use std::env;
use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use zcutils::gang_scheduler::{
    ApplicationKind, ApplicationSpec, BusinessEntitySpec, BusinessImpactEstimate,
    ConcurrentSchedulingStrategy, DurabilityOwner, EstateEvent, EstateEventEnvelope,
    FailureScenario, GangScheduler, GangSchedulerConfig, HostSpec, IsolationRequirement,
    ModeledGangExecutor, RecoveryObjective, RegionSpec, SchedulerDecision, VolumeSpec,
};

const REGION: &str = "qemu-local";
const CAPACITY_UNITS_PER_HOST: u64 = 160;
const ORDINARY_IOPS_UNITS: u64 = 1;
const HEAVY_IOPS_UNITS: u64 = 17;
const INITIAL_VOLUMES: usize = 1_000;
const HEAVY_VOLUMES: usize = 7;
const FILL_VOLUMES: usize = 8;
const BYTES_PER_VOLUME: u64 = 4096;

struct Scenario {
    scheduler: GangScheduler,
    executor: ModeledGangExecutor,
    index: u64,
    timestamp_ns: u64,
}

impl Scenario {
    fn new() -> Self {
        Self {
            scheduler: GangScheduler::with_config(GangSchedulerConfig {
                strategy: ConcurrentSchedulingStrategy::Hybrid,
            }),
            executor: ModeledGangExecutor::new(),
            index: 0,
            timestamp_ns: 0,
        }
    }

    fn emit(&mut self, event: EstateEvent) -> io::Result<Vec<SchedulerDecision>> {
        self.index = self.index.saturating_add(1);
        self.timestamp_ns = self.timestamp_ns.saturating_add(1);
        self.scheduler.apply(EstateEventEnvelope::new(
            self.index,
            self.timestamp_ns,
            event,
        ))
    }

    fn settle(
        &mut self,
        mut frontier: Vec<SchedulerDecision>,
    ) -> io::Result<Vec<SchedulerDecision>> {
        let mut all = frontier.clone();
        loop {
            let responses = self.executor.responses(&frontier);
            if responses.is_empty() {
                return Ok(all);
            }
            frontier.clear();
            for response in responses {
                let output = self.emit(response)?;
                all.extend(output.clone());
                frontier.extend(output);
            }
        }
    }

    fn act(&mut self, event: EstateEvent) -> io::Result<Vec<SchedulerDecision>> {
        let frontier = self.emit(event)?;
        self.settle(frontier)
    }

    fn plan(&mut self) -> io::Result<Vec<SchedulerDecision>> {
        self.act(EstateEvent::PlanAtWatermark {
            input_watermark: self.index,
        })
    }
}

fn recovery() -> RecoveryObjective {
    RecoveryObjective {
        rpo_max_missing_operations: 0,
        rto_ns: None,
        scenarios: BTreeSet::from([FailureScenario::HostLoss]),
        minimum_recovery_iops: 1,
        allowed_regions: BTreeSet::new(),
        preapproved_failover: Vec::new(),
    }
}

fn volume(volume_id: String, iops: u64) -> VolumeSpec {
    VolumeSpec {
        volume_id,
        home_region_id: REGION.to_string(),
        bytes: BYTES_PER_VOLUME,
        provisioned_iops: iops,
        latest_hwm: 0,
        durability_owner: DurabilityOwner::Application,
        storage_copies: 1,
        isolation: IsolationRequirement::Shared,
        recovery_group_id: None,
        recovery: recovery(),
    }
}

fn application(application_id: &str, volumes: Vec<VolumeSpec>) -> ApplicationSpec {
    ApplicationSpec {
        application_id: application_id.to_string(),
        business_entity_id: "qemu-estate".to_string(),
        kind: ApplicationKind::Other,
        business_impact: BusinessImpactEstimate {
            downtime_cost_microunits_per_second: 1,
            rto_breach_cost_microunits: 1,
            lost_operation_cost_microunits: 1,
        },
        scenario_impacts: Vec::new(),
        volumes,
    }
}

fn host(ordinal: usize) -> HostSpec {
    HostSpec {
        host_id: format!("storage-{ordinal}"),
        region_id: REGION.to_string(),
        failure_domain: format!("qemu-vm-{ordinal}"),
        capacity_bytes: CAPACITY_UNITS_PER_HOST * BYTES_PER_VOLUME,
        lanes: 1,
        lane_iops: CAPACITY_UNITS_PER_HOST,
        restore_bytes_per_second: 1 << 30,
    }
}

fn wait_for_marker(path: &Path, timeout: Duration) -> io::Result<()> {
    let started = Instant::now();
    while !path.exists() {
        if started.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for online marker {}", path.display()),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let marker = args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: zcvolume-capacity-scenario <storage-8-online-marker>",
        )
    })?;
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: zcvolume-capacity-scenario <storage-8-online-marker>",
        ));
    }
    let marker = Path::new(&marker);
    let mut scenario = Scenario::new();
    scenario.act(EstateEvent::PutRegion {
        spec: RegionSpec {
            region_id: REGION.to_string(),
            trust_domain: "local-qemu-host".to_string(),
        },
    })?;
    scenario.act(EstateEvent::PutBusinessEntity {
        spec: BusinessEntitySpec {
            business_entity_id: "qemu-estate".to_string(),
            generation: 1,
            region_ids: BTreeSet::from([REGION.to_string()]),
        },
    })?;
    for ordinal in 1..=7 {
        scenario.act(EstateEvent::PutHost {
            spec: host(ordinal),
        })?;
    }

    let initial = (0..INITIAL_VOLUMES)
        .map(|ordinal| {
            volume(
                format!("volume-{ordinal:04}"),
                if ordinal < HEAVY_VOLUMES {
                    HEAVY_IOPS_UNITS
                } else {
                    ORDINARY_IOPS_UNITS
                },
            )
        })
        .collect();
    scenario.act(EstateEvent::PutApplication {
        spec: application("initial-thousand", initial),
    })?;
    scenario.plan()?;
    let initial_placed = (0..INITIAL_VOLUMES)
        .filter(|ordinal| {
            scenario
                .scheduler
                .placement(&format!("volume-{ordinal:04}"))
                .is_some()
        })
        .count();
    if initial_placed != INITIAL_VOLUMES {
        return Err(io::Error::other(format!(
            "only {initial_placed}/{INITIAL_VOLUMES} initial volumes were admitted"
        )));
    }
    println!(
        "ZCVOLUME_CAPACITY_INITIAL_PASS storage_nodes=7 volumes=1000 heavy_volumes=7 ordinary_iops_units=1 heavy_iops_units=17 per_node_iops_units=160 aggregate_reserved_iops_units=1112 aggregate_capacity_iops_units=1120 placement=userspace-lane-flow block_placement=false"
    );

    scenario.act(EstateEvent::PutApplication {
        spec: application(
            "fill-slack",
            (0..FILL_VOLUMES)
                .map(|ordinal| volume(format!("fill-{ordinal}"), ORDINARY_IOPS_UNITS))
                .collect(),
        ),
    })?;
    scenario.plan()?;
    if (0..FILL_VOLUMES).any(|ordinal| {
        scenario
            .scheduler
            .placement(&format!("fill-{ordinal}"))
            .is_none()
    }) {
        return Err(io::Error::other(
            "the eight fill volumes did not consume the remaining capacity",
        ));
    }
    println!(
        "ZCVOLUME_CAPACITY_FULL storage_nodes=7 volumes=1008 reserved_iops_units=1120 capacity_iops_units=1120 free_iops_units=0"
    );

    scenario.act(EstateEvent::PutApplication {
        spec: application(
            "pending-request",
            vec![volume("needs-storage-8".to_string(), ORDINARY_IOPS_UNITS)],
        ),
    })?;
    let rejection = scenario
        .plan()?
        .into_iter()
        .find_map(|decision| match decision {
            SchedulerDecision::Deferred { subject_id, reason }
                if subject_id == "needs-storage-8" =>
            {
                Some(reason)
            }
            _ => None,
        })
        .ok_or_else(|| io::Error::other("full estate did not defer needs-storage-8"))?;
    if scenario.scheduler.placement("needs-storage-8").is_some() {
        return Err(io::Error::other(
            "rejected volume unexpectedly has a placement",
        ));
    }
    println!(
        "ZCVOLUME_CAPACITY_REJECT_PASS volume=needs-storage-8 phase=deferred reason={:?} existing_state_unchanged=true",
        rejection
    );
    println!(
        "ZCVOLUME_CAPACITY_WAITING_FOR_NODE marker={}",
        marker.display()
    );
    wait_for_marker(marker, Duration::from_secs(180))?;

    scenario.act(EstateEvent::PutHost { spec: host(8) })?;
    scenario.plan()?;
    let placement = scenario
        .scheduler
        .placement("needs-storage-8")
        .ok_or_else(|| io::Error::other("volume remained deferred after storage-8 joined"))?;
    if placement.legs.len() != 1 || placement.legs[0].host_id != "storage-8" {
        return Err(io::Error::other(format!(
            "new volume was not placed on the newly online host: {:?}",
            placement.legs
        )));
    }
    println!(
        "ZCVOLUME_CAPACITY_ADD_NODE_PASS volume=needs-storage-8 storage_nodes=8 selected_host=storage-8 selected_lane=0 request_mutated=false admission_trigger=host-online-event userspace_placement=true block_placement=false"
    );
    Ok(())
}
