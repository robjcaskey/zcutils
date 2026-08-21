use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zcutils::TelemetryReporter;
use zcutils::volume_partition::{
    CopyMethod, IO_ALIGNMENT, MigrationLocality, PartitionDefinition, PartitionedVolume,
};

const PHASE_BASELINE: usize = 0;
const PHASE_MIGRATION: usize = 1;
const PHASE_SNAPSHOT: usize = 2;
const PHASES: usize = 3;
const SAMPLE_MASK: u64 = 1023;

struct Metrics {
    operations: [AtomicU64; PHASES],
    sampled_operations: [AtomicU64; PHASES],
    sampled_latency_ns: [AtomicU64; PHASES],
    max_sampled_latency_ns: [AtomicU64; PHASES],
}

struct AdaptiveCopyController {
    target_iops: f64,
    latency_limit_ns: f64,
    current_rate: u64,
    fixed_rate: u64,
    min_rate: u64,
    max_rate: u64,
    enabled: bool,
    last_sample: Instant,
    last_operations: u64,
    last_samples: u64,
    last_latency_ns: u64,
    adjustments: u64,
}

impl AdaptiveCopyController {
    fn new(
        baseline_iops: f64,
        baseline_average_latency_ns: f64,
        initial_rate: u64,
        min_rate: u64,
        enabled: bool,
        operations: u64,
        samples: u64,
        latency_ns: u64,
    ) -> Self {
        let fixed_rate = initial_rate;
        let initial_rate = if initial_rate == 0 {
            512 * 1024 * 1024
        } else {
            initial_rate
        };
        Self {
            target_iops: baseline_iops * 0.98,
            latency_limit_ns: baseline_average_latency_ns * 1.25,
            current_rate: initial_rate,
            fixed_rate,
            min_rate,
            max_rate: initial_rate.saturating_mul(4),
            enabled,
            last_sample: Instant::now(),
            last_operations: operations,
            last_samples: samples,
            last_latency_ns: latency_ns,
            adjustments: 0,
        }
    }

    fn next(&mut self, operations: u64, samples: u64, latency_ns: u64) -> u64 {
        if !self.enabled {
            return self.fixed_rate;
        }
        let elapsed = self.last_sample.elapsed();
        if elapsed < Duration::from_millis(100) {
            return self.current_rate;
        }
        let observed = operations.saturating_sub(self.last_operations) as f64
            / elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        let sample_delta = samples.saturating_sub(self.last_samples);
        let latency_delta = latency_ns.saturating_sub(self.last_latency_ns);
        let observed_latency_ns = if sample_delta == 0 {
            0.0
        } else {
            latency_delta as f64 / sample_delta as f64
        };
        if observed < self.target_iops
            || (observed_latency_ns != 0.0 && observed_latency_ns > self.latency_limit_ns)
        {
            self.current_rate = self
                .current_rate
                .saturating_mul(85)
                .checked_div(100)
                .unwrap_or(self.min_rate)
                .max(self.min_rate);
        } else if observed > self.target_iops * 1.01 {
            self.current_rate = self
                .current_rate
                .saturating_mul(110)
                .checked_div(100)
                .unwrap_or(self.max_rate)
                .min(self.max_rate);
        }
        self.last_sample = Instant::now();
        self.last_operations = operations;
        self.last_samples = samples;
        self.last_latency_ns = latency_ns;
        self.adjustments += 1;
        self.current_rate
    }

    fn rate_mib_s(&self) -> f64 {
        self.current_rate as f64 / (1024.0 * 1024.0)
    }
}

impl Metrics {
    fn new() -> Self {
        Self {
            operations: std::array::from_fn(|_| AtomicU64::new(0)),
            sampled_operations: std::array::from_fn(|_| AtomicU64::new(0)),
            sampled_latency_ns: std::array::from_fn(|_| AtomicU64::new(0)),
            max_sampled_latency_ns: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 6 {
        return Err(invalid(
            "usage: zcvolume-live-bench ROOT SIZE_MIB WORKERS CPU_LIST BASELINE_MS",
        ));
    }
    let root = PathBuf::from(&args[1]);
    if !root.is_absolute() {
        return Err(invalid("benchmark ROOT must be absolute"));
    }
    let size_mib = parse_u64(&args[2], "SIZE_MIB")?;
    let workers = parse_usize(&args[3], "WORKERS")?;
    let cpus = parse_cpu_list(&args[4])?;
    let baseline_ms = parse_u64(&args[5], "BASELINE_MS")?;
    let leaf_partitions = env::var("ZCVOLUME_LEAF_PARTITIONS")
        .ok()
        .map(|value| parse_usize(&value, "ZCVOLUME_LEAF_PARTITIONS"))
        .transpose()?
        .unwrap_or(1);
    let copy_rate_mib_s = env::var("ZCVOLUME_COPY_RATE_MIB_S")
        .ok()
        .map(|value| parse_u64(&value, "ZCVOLUME_COPY_RATE_MIB_S"))
        .transpose()?
        .unwrap_or(0);
    let copy_rate_bytes_s = copy_rate_mib_s
        .checked_mul(1024 * 1024)
        .ok_or_else(|| invalid("copy rate overflow"))?;
    let min_copy_rate_mib_s = env::var("ZCVOLUME_MIN_COPY_RATE_MIB_S")
        .ok()
        .map(|value| parse_u64(&value, "ZCVOLUME_MIN_COPY_RATE_MIB_S"))
        .transpose()?
        .unwrap_or(64);
    let min_copy_rate_bytes_s = min_copy_rate_mib_s
        .checked_mul(1024 * 1024)
        .ok_or_else(|| invalid("minimum copy rate overflow"))?;
    if min_copy_rate_bytes_s == 0 {
        return Err(invalid("minimum copy rate must be nonzero"));
    }
    let adaptive =
        env::var_os("ZCVOLUME_ADAPTIVE").map_or(copy_rate_bytes_s != 0, |value| value != "0");
    if adaptive && copy_rate_bytes_s != 0 && min_copy_rate_bytes_s > copy_rate_bytes_s {
        return Err(invalid(
            "minimum copy rate cannot exceed the initial adaptive copy rate",
        ));
    }
    let copy_method = match env::var("ZCVOLUME_COPY_METHOD")
        .unwrap_or_else(|_| "copy_file_range".into())
        .as_str()
    {
        "buffered" => CopyMethod::Buffered,
        "copy_file_range" | "cfr" => CopyMethod::CopyFileRange,
        value => return Err(invalid(format!("invalid ZCVOLUME_COPY_METHOD {value}"))),
    };
    if size_mib == 0
        || workers == 0
        || leaf_partitions == 0
        || baseline_ms == 0
        || cpus.len() != workers
    {
        return Err(invalid(
            "size, workers, and baseline duration must be nonzero and CPU_LIST must map every worker",
        ));
    }
    let logical_bytes = size_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| invalid("SIZE_MIB overflow"))?;
    if logical_bytes % IO_ALIGNMENT != 0 {
        return Err(invalid("volume size must be 4096 aligned"));
    }
    let partition_bytes = logical_bytes
        .checked_div(leaf_partitions as u64)
        .filter(|bytes| *bytes != 0 && *bytes % IO_ALIGNMENT == 0)
        .ok_or_else(|| invalid("volume size must divide into 4096-aligned leaf partitions"))?;
    let strict = env_flag("URING_PLAY_TOPOLOGY_STRICT") || env_flag("URING_PLAY_TOPOLOGY_FATAL");
    let allowed_cpus = allowed_cpu_list()?;
    if cpus.iter().any(|cpu| !allowed_cpus.contains(cpu)) {
        let message = format!(
            "worker CPU mapping references CPU outside process affinity: allowed={} map={}",
            cpu_set_label(&allowed_cpus),
            cpu_label(&cpus)
        );
        if strict {
            return Err(invalid(message));
        }
        eprintln!("zcvolume-live-bench: WARNING: {message}");
    }
    let worker_nodes = cpus
        .iter()
        .map(|cpu| cpu_numa_node(*cpu))
        .collect::<io::Result<Vec<_>>>()?;
    let common_node = worker_nodes.first().copied().flatten().filter(|node| {
        worker_nodes
            .iter()
            .all(|candidate| *candidate == Some(*node))
    });
    if common_node.is_none() {
        let message = format!(
            "foreground workers do not resolve to one NUMA node: mapping={}",
            cpu_node_label(&cpus, &worker_nodes)
        );
        if strict {
            return Err(invalid(message));
        }
        eprintln!("zcvolume-live-bench: WARNING: {message}");
    }
    let copy_cpu = env::var("ZCVOLUME_COPY_CPU")
        .ok()
        .map(|value| parse_usize(&value, "ZCVOLUME_COPY_CPU"))
        .transpose()?
        .or_else(|| {
            common_node.and_then(|node| {
                allowed_cpus
                    .iter()
                    .copied()
                    .find(|cpu| !cpus.contains(cpu) && cpu_numa_node(*cpu).ok() == Some(Some(node)))
            })
        })
        .unwrap_or(cpus[0]);
    if !allowed_cpus.contains(&copy_cpu) {
        return Err(invalid(format!(
            "migration copy CPU {copy_cpu} is outside process affinity"
        )));
    }
    let copy_node = cpu_numa_node(copy_cpu)?;
    if common_node.is_some() && copy_node != common_node {
        return Err(invalid(format!(
            "migration copy CPU {copy_cpu} is on node {}, foreground is on node {}",
            option_node(copy_node),
            option_node(common_node)
        )));
    }
    let migration_locality = MigrationLocality {
        preferred_cpu: Some(copy_cpu),
        expected_numa_node: common_node,
        strict,
    };
    let shared_system = shared_system_measurement();

    fs::create_dir_all(&root)?;
    let run_root = root.join(format!("zcvolume-live-{}", nonce()));
    fs::create_dir(&run_root)?;
    let partition_names = (0..leaf_partitions)
        .map(|partition| format!("data-{partition:04}"))
        .collect::<Vec<_>>();
    let destination_paths = (0..leaf_partitions)
        .map(|partition| run_root.join(format!("destination-{partition:04}.img")))
        .collect::<Vec<_>>();
    let snapshot_paths = (0..leaf_partitions)
        .map(|partition| run_root.join(format!("snapshot-{partition:04}.img")))
        .collect::<Vec<_>>();
    let definitions = partition_names
        .iter()
        .enumerate()
        .map(|(partition, partition_id)| PartitionDefinition {
            partition_id: partition_id.clone(),
            start_bytes: partition as u64 * partition_bytes,
            length_bytes: partition_bytes,
            active_path: run_root.join(format!("source-{partition:04}.img")),
        })
        .collect();
    let volume = Arc::new(PartitionedVolume::create(
        run_root.join("control"),
        "bench-volume",
        logical_bytes,
        definitions,
    )?);

    println!(
        "zcvolume_live_topology representative=true shared_system={} strict={} terminal_kind=tmpfs_or_regular_file placement_owner=userspace block_device_raid=false partition_alignment=4096 leaf_partitions={} partition_bytes={} workers={} lane_count={} per_worker_qd=1 aggregate_outstanding={} raw_transport_rtt=not_applicable_local_terminal theoretical_iops_ceiling=not_network_rtt_bound worker_cpu_map={} worker_cpu_numa_map={} lane_worker_map={} migration_copy_cpu={} migration_copy_numa_node={} numa_local={} artifact={}",
        shared_system,
        strict,
        leaf_partitions,
        partition_bytes,
        workers,
        workers,
        workers,
        cpu_label(&cpus),
        cpu_node_label(&cpus, &worker_nodes),
        lane_label(workers),
        copy_cpu,
        option_node(copy_node),
        common_node.is_some() && copy_node == common_node,
        run_root.display()
    );
    println!(
        "zcvolume_live_semantics foreground_completion=local_terminal_write sync_completion=terminal_sync_data migration_capture=per_page_dirty_generation migration_destination_witness=staged_non_witness cutover=per-partition-atomic-route-swap+quiescence request_path=native-4k-logical-placement leaf_placement=compiled-immutable-userspace-layout placement_resolution=once-per-lane-epoch layout_publication=atomic-swap block_client_placement=no copy_rate_mib_s={} min_copy_rate_mib_s={} adaptive_copy={} adaptive_target_efficiency=0.98 copy_method={:?} copy_chunk_bytes={}",
        copy_rate_mib_s,
        min_copy_rate_mib_s,
        adaptive,
        copy_method,
        4 * 1024 * 1024
    );

    let phase = Arc::new(AtomicUsize::new(PHASE_BASELINE));
    let running = Arc::new(AtomicBool::new(true));
    let metrics = Arc::new(Metrics::new());
    let barrier = Arc::new(Barrier::new(workers + 1));
    let mut handles = Vec::with_capacity(workers);
    for (worker, cpu) in cpus.iter().copied().enumerate() {
        let io = volume.io_handle();
        let phase = Arc::clone(&phase);
        let running = Arc::clone(&running);
        let metrics = Arc::clone(&metrics);
        let barrier = Arc::clone(&barrier);
        handles.push(
            thread::Builder::new()
                .name(format!("zcvol-{worker}"))
                .spawn(move || -> io::Result<()> {
                    pin_current_thread(cpu)?;
                    let pages = partition_bytes / IO_ALIGNMENT;
                    let mut random = 0x9e37_79b9_7f4a_7c15u64 ^ worker as u64;
                    let mut buffer = [0u8; IO_ALIGNMENT as usize];
                    let mut local_ops = 0u64;
                    barrier.wait();
                    while running.load(Ordering::Acquire) {
                        random = xorshift64(random);
                        let partition = random as usize % leaf_partitions;
                        random = xorshift64(random);
                        let offset =
                            partition as u64 * partition_bytes + (random % pages) * IO_ALIGNMENT;
                        let current_phase = phase.load(Ordering::Relaxed).min(PHASES - 1);
                        let sampled = local_ops & SAMPLE_MASK == 0;
                        let started = sampled.then(Instant::now);
                        if random & 1 == 0 {
                            io.read_page_at(offset, &mut buffer)?;
                        } else {
                            buffer[0..8].copy_from_slice(&random.to_le_bytes());
                            io.write_page_at(offset, &buffer)?;
                        }
                        metrics.operations[current_phase].fetch_add(1, Ordering::Relaxed);
                        if let Some(started) = started {
                            let ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                            metrics.sampled_operations[current_phase]
                                .fetch_add(1, Ordering::Relaxed);
                            metrics.sampled_latency_ns[current_phase]
                                .fetch_add(ns, Ordering::Relaxed);
                            metrics.max_sampled_latency_ns[current_phase]
                                .fetch_max(ns, Ordering::Relaxed);
                        }
                        local_ops += 1;
                    }
                    Ok(())
                })?,
        );
    }
    barrier.wait();

    let baseline_start_count = metrics.operations[PHASE_BASELINE].load(Ordering::Relaxed);
    let baseline_start = Instant::now();
    thread::sleep(Duration::from_millis(baseline_ms));
    let baseline_elapsed = baseline_start.elapsed();
    let baseline_ops = metrics.operations[PHASE_BASELINE]
        .load(Ordering::Relaxed)
        .saturating_sub(baseline_start_count);
    print_phase(
        "baseline",
        baseline_ops,
        baseline_elapsed,
        &metrics,
        PHASE_BASELINE,
    );

    phase.store(PHASE_MIGRATION, Ordering::Release);
    let migration_start_count = metrics.operations[PHASE_MIGRATION].load(Ordering::Relaxed);
    let migration_start = Instant::now();
    let mut migration_controller = AdaptiveCopyController::new(
        rate(baseline_ops, baseline_elapsed),
        phase_average_latency_ns(&metrics, PHASE_BASELINE),
        copy_rate_bytes_s,
        min_copy_rate_bytes_s,
        adaptive,
        metrics.operations[PHASE_MIGRATION].load(Ordering::Relaxed),
        metrics.sampled_operations[PHASE_MIGRATION].load(Ordering::Relaxed),
        metrics.sampled_latency_ns[PHASE_MIGRATION].load(Ordering::Relaxed),
    );
    let mut migration_base_bytes = 0u64;
    let mut migration_dirty_pages = 0u64;
    let mut migration_dirty_bytes = 0u64;
    let mut migration_cutover_generation = 0u64;
    let mut migration_cutover_fence_total_ns = 0u64;
    let mut migration_cutover_fence_max_ns = 0u64;
    for (partition, partition_id) in partition_names.iter().enumerate() {
        let migration_id = format!("live-bench-migration-{partition:04}");
        volume.begin_migration_with_locality(
            partition_id,
            &migration_id,
            &destination_paths[partition],
            migration_locality,
        )?;
        volume.copy_migration_base_controlled_with_method(
            partition_id,
            4 * 1024 * 1024,
            copy_method,
            |_| {
                migration_controller.next(
                    metrics.operations[PHASE_MIGRATION].load(Ordering::Relaxed),
                    metrics.sampled_operations[PHASE_MIGRATION].load(Ordering::Relaxed),
                    metrics.sampled_latency_ns[PHASE_MIGRATION].load(Ordering::Relaxed),
                )
            },
        )?;
        let migration = volume.commit_migration(partition_id)?;
        migration_base_bytes += migration.base_bytes_copied;
        migration_dirty_pages += migration.redo_records_replayed;
        migration_dirty_bytes += migration.redo_bytes_replayed;
        migration_cutover_generation =
            migration_cutover_generation.max(migration.cutover_generation);
        migration_cutover_fence_total_ns += migration.cutover_fence_ns;
        migration_cutover_fence_max_ns =
            migration_cutover_fence_max_ns.max(migration.cutover_fence_ns);
    }
    let migration_elapsed = migration_start.elapsed();
    let migration_ops = metrics.operations[PHASE_MIGRATION]
        .load(Ordering::Relaxed)
        .saturating_sub(migration_start_count);
    print_phase(
        "migration_active",
        migration_ops,
        migration_elapsed,
        &metrics,
        PHASE_MIGRATION,
    );
    println!(
        "zcvolume_live_migration base_bytes={} dirty_pages_replayed={} dirty_bytes_replayed={} cutover_generation={} cutover_fence_total_ms={:.3} cutover_fence_max_ms={:.3} copy_mib_s={:.1} adaptive_final_rate_mib_s={:.1} adaptive_adjustments={}",
        migration_base_bytes,
        migration_dirty_pages,
        migration_dirty_bytes,
        migration_cutover_generation,
        migration_cutover_fence_total_ns as f64 / 1_000_000.0,
        migration_cutover_fence_max_ns as f64 / 1_000_000.0,
        mib_per_second(
            migration_base_bytes + migration_dirty_bytes,
            migration_elapsed
        ),
        migration_controller.rate_mib_s(),
        migration_controller.adjustments,
    );

    phase.store(PHASE_BASELINE, Ordering::Release);
    let snapshot_baseline_start_count = metrics.operations[PHASE_BASELINE].load(Ordering::Relaxed);
    let snapshot_baseline_start = Instant::now();
    thread::sleep(Duration::from_millis(baseline_ms));
    let snapshot_baseline_elapsed = snapshot_baseline_start.elapsed();
    let snapshot_baseline_ops = metrics.operations[PHASE_BASELINE]
        .load(Ordering::Relaxed)
        .saturating_sub(snapshot_baseline_start_count);
    print_phase(
        "snapshot_baseline",
        snapshot_baseline_ops,
        snapshot_baseline_elapsed,
        &metrics,
        PHASE_BASELINE,
    );

    phase.store(PHASE_SNAPSHOT, Ordering::Release);
    let snapshot_start_count = metrics.operations[PHASE_SNAPSHOT].load(Ordering::Relaxed);
    let snapshot_start = Instant::now();
    let mut snapshot_controller = AdaptiveCopyController::new(
        rate(snapshot_baseline_ops, snapshot_baseline_elapsed),
        phase_average_latency_ns(&metrics, PHASE_BASELINE),
        copy_rate_bytes_s,
        min_copy_rate_bytes_s,
        adaptive,
        metrics.operations[PHASE_SNAPSHOT].load(Ordering::Relaxed),
        metrics.sampled_operations[PHASE_SNAPSHOT].load(Ordering::Relaxed),
        metrics.sampled_latency_ns[PHASE_SNAPSHOT].load(Ordering::Relaxed),
    );
    let mut snapshot_base_bytes = 0u64;
    let mut snapshot_dirty_pages = 0u64;
    let mut snapshot_dirty_bytes = 0u64;
    let mut snapshot_seal_generation = 0u64;
    let mut snapshot_cutover_fence_total_ns = 0u64;
    let mut snapshot_cutover_fence_max_ns = 0u64;
    for (partition, partition_id) in partition_names.iter().enumerate() {
        let snapshot_id = format!("live-bench-snapshot-{partition:04}");
        volume.begin_migration_with_locality(
            partition_id,
            &snapshot_id,
            &snapshot_paths[partition],
            migration_locality,
        )?;
        volume.copy_migration_base_controlled_with_method(
            partition_id,
            4 * 1024 * 1024,
            copy_method,
            |_| {
                snapshot_controller.next(
                    metrics.operations[PHASE_SNAPSHOT].load(Ordering::Relaxed),
                    metrics.sampled_operations[PHASE_SNAPSHOT].load(Ordering::Relaxed),
                    metrics.sampled_latency_ns[PHASE_SNAPSHOT].load(Ordering::Relaxed),
                )
            },
        )?;
        let snapshot = volume.commit_snapshot(partition_id)?;
        snapshot_base_bytes += snapshot.base_bytes_copied;
        snapshot_dirty_pages += snapshot.redo_records_replayed;
        snapshot_dirty_bytes += snapshot.redo_bytes_replayed;
        snapshot_seal_generation = snapshot_seal_generation.max(snapshot.cutover_generation);
        snapshot_cutover_fence_total_ns += snapshot.cutover_fence_ns;
        snapshot_cutover_fence_max_ns =
            snapshot_cutover_fence_max_ns.max(snapshot.cutover_fence_ns);
    }
    let snapshot_elapsed = snapshot_start.elapsed();
    let snapshot_ops = metrics.operations[PHASE_SNAPSHOT]
        .load(Ordering::Relaxed)
        .saturating_sub(snapshot_start_count);
    print_phase(
        "snapshot_active",
        snapshot_ops,
        snapshot_elapsed,
        &metrics,
        PHASE_SNAPSHOT,
    );
    println!(
        "zcvolume_live_snapshot base_bytes={} dirty_pages_replayed={} dirty_bytes_replayed={} seal_generation={} cutover_fence_total_ms={:.3} cutover_fence_max_ms={:.3} capture_mib_s={:.1} adaptive_final_rate_mib_s={:.1} adaptive_adjustments={}",
        snapshot_base_bytes,
        snapshot_dirty_pages,
        snapshot_dirty_bytes,
        snapshot_seal_generation,
        snapshot_cutover_fence_total_ns as f64 / 1_000_000.0,
        snapshot_cutover_fence_max_ns as f64 / 1_000_000.0,
        mib_per_second(snapshot_base_bytes + snapshot_dirty_bytes, snapshot_elapsed),
        snapshot_controller.rate_mib_s(),
        snapshot_controller.adjustments,
    );

    running.store(false, Ordering::Release);
    for handle in handles {
        handle
            .join()
            .map_err(|_| io::Error::other("foreground worker panicked"))??;
    }
    let recovery_started = Instant::now();
    let mut recovered = 0u64;
    for (partition, partition_id) in partition_names.iter().enumerate() {
        recovered += volume.restore_partition_from_snapshot(
            partition_id,
            &snapshot_paths[partition],
            4 * 1024 * 1024,
        )?;
    }
    let recovery_elapsed = recovery_started.elapsed();
    println!(
        "zcvolume_live_recovery bytes={} elapsed_ms={:.3} restore_mib_s={:.1} foreground_admission=fenced",
        recovered,
        recovery_elapsed.as_secs_f64() * 1000.0,
        mib_per_second(recovered, recovery_elapsed)
    );
    println!(
        "ZCVOLUME_LIVE_BENCH_PASS migration_baseline_iops={:.0} migration_iops={:.0} snapshot_baseline_iops={:.0} snapshot_iops={:.0} migration_efficiency={:.4} snapshot_efficiency={:.4} artifact={}",
        rate(baseline_ops, baseline_elapsed),
        rate(migration_ops, migration_elapsed),
        rate(snapshot_baseline_ops, snapshot_baseline_elapsed),
        rate(snapshot_ops, snapshot_elapsed),
        efficiency(
            migration_ops,
            migration_elapsed,
            baseline_ops,
            baseline_elapsed
        ),
        efficiency(
            snapshot_ops,
            snapshot_elapsed,
            snapshot_baseline_ops,
            snapshot_baseline_elapsed
        ),
        run_root.display()
    );
    let telemetry = TelemetryReporter::new();
    if telemetry.is_enabled() {
        let migration_iops = rate(migration_ops, migration_elapsed).round() as u64;
        let snapshot_iops = rate(snapshot_ops, snapshot_elapsed).round() as u64;
        let mut payload = serde_json::Map::new();
        payload.insert(
            "component".to_string(),
            serde_json::Value::String("zcvolume-live-bench".to_string()),
        );
        payload.insert(
            "phase".to_string(),
            serde_json::Value::String("result".to_string()),
        );
        payload.insert(
            "backend".to_string(),
            serde_json::Value::String("userspace-volume".to_string()),
        );
        payload.insert(
            "total_iops".to_string(),
            serde_json::Value::from(migration_iops.min(snapshot_iops)),
        );
        payload.insert(
            "size_bytes".to_string(),
            serde_json::Value::from(logical_bytes),
        );
        payload.insert(
            "active_volume_count".to_string(),
            serde_json::Value::from(0_u64),
        );
        payload.insert("ok".to_string(), serde_json::Value::Bool(true));
        telemetry.emit_event("volume_live_operation_result", payload);
        telemetry.shutdown();
    }
    Ok(())
}

fn print_phase(label: &str, operations: u64, elapsed: Duration, metrics: &Metrics, phase: usize) {
    let samples = metrics.sampled_operations[phase].load(Ordering::Relaxed);
    let sampled_ns = metrics.sampled_latency_ns[phase].load(Ordering::Relaxed);
    let average_sampled_us = if samples == 0 {
        0.0
    } else {
        sampled_ns as f64 / samples as f64 / 1000.0
    };
    println!(
        "zcvolume_live_phase phase={} elapsed_s={:.6} operations={} iops={:.0} sampled_ops={} sampled_average_us={:.3} sampled_max_us={:.3}",
        label,
        elapsed.as_secs_f64(),
        operations,
        rate(operations, elapsed),
        samples,
        average_sampled_us,
        metrics.max_sampled_latency_ns[phase].load(Ordering::Relaxed) as f64 / 1000.0
    );
}

fn phase_average_latency_ns(metrics: &Metrics, phase: usize) -> f64 {
    let samples = metrics.sampled_operations[phase].load(Ordering::Relaxed);
    if samples == 0 {
        return 0.0;
    }
    metrics.sampled_latency_ns[phase].load(Ordering::Relaxed) as f64 / samples as f64
}

fn rate(operations: u64, elapsed: Duration) -> f64 {
    operations as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE)
}

fn efficiency(
    operations: u64,
    elapsed: Duration,
    baseline_operations: u64,
    baseline_elapsed: Duration,
) -> f64 {
    rate(operations, elapsed) / rate(baseline_operations, baseline_elapsed).max(f64::MIN_POSITIVE)
}

fn mib_per_second(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64().max(f64::MIN_POSITIVE)
}

fn xorshift64(mut value: u64) -> u64 {
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    value
}

fn pin_current_thread(cpu: usize) -> io::Result<()> {
    if cpu >= libc::CPU_SETSIZE as usize {
        return Err(invalid(format!("CPU {cpu} exceeds affinity set capacity")));
    }
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        let rc = libc::pthread_setaffinity_np(
            libc::pthread_self(),
            std::mem::size_of::<libc::cpu_set_t>(),
            &set,
        );
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn allowed_cpu_list() -> io::Result<Vec<usize>> {
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    let rc = unsafe {
        libc::pthread_getaffinity_np(
            libc::pthread_self(),
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut set,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok((0..libc::CPU_SETSIZE as usize)
        .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) })
        .collect())
}

#[cfg(not(target_os = "linux"))]
fn allowed_cpu_list() -> io::Result<Vec<usize>> {
    Ok((0..thread::available_parallelism()?.get()).collect())
}

fn cpu_numa_node(cpu: usize) -> io::Result<Option<u32>> {
    #[cfg(target_os = "linux")]
    {
        let path = PathBuf::from(format!("/sys/devices/system/cpu/cpu{cpu}"));
        if !path.exists() {
            return Err(invalid(format!("CPU {cpu} does not exist")));
        }
        for entry in fs::read_dir(path)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if let Some(node) = name.strip_prefix("node") {
                if !node.is_empty() && node.bytes().all(|byte| byte.is_ascii_digit()) {
                    return node
                        .parse::<u32>()
                        .map(Some)
                        .map_err(|_| invalid(format!("invalid NUMA node name {name}")));
                }
            }
        }
    }
    let _ = cpu;
    Ok(None)
}

fn parse_cpu_list(value: &str) -> io::Result<Vec<usize>> {
    if value.is_empty() {
        return Err(invalid("CPU_LIST is empty"));
    }
    value
        .split(',')
        .map(|part| parse_usize(part, "CPU_LIST"))
        .collect()
}

fn cpu_label(cpus: &[usize]) -> String {
    cpus.iter()
        .enumerate()
        .map(|(worker, cpu)| format!("{worker}:{cpu}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn cpu_node_label(cpus: &[usize], nodes: &[Option<u32>]) -> String {
    cpus.iter()
        .zip(nodes)
        .map(|(cpu, node)| format!("{cpu}:{}", option_node(*node)))
        .collect::<Vec<_>>()
        .join(",")
}

fn cpu_set_label(cpus: &[usize]) -> String {
    cpus.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn option_node(node: Option<u32>) -> String {
    node.map_or_else(|| "unknown".into(), |node| node.to_string())
}

fn lane_label(workers: usize) -> String {
    (0..workers)
        .map(|lane| format!("{lane}:{lane}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_u64(value: &str, label: &str) -> io::Result<u64> {
    value
        .parse()
        .map_err(|_| invalid(format!("invalid {label}: {value}")))
}

fn parse_usize(value: &str, label: &str) -> io::Result<usize> {
    value
        .parse()
        .map_err(|_| invalid(format!("invalid {label}: {value}")))
}

fn env_flag(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| value != "0")
}

fn shared_system_measurement() -> bool {
    if let Some(value) = env::var_os("ZCVOLUME_SHARED_SYSTEM") {
        return value != "0";
    }
    let manifest = env::var_os("ZCUTILS_BOOTSTRAP_MANIFEST")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".local/state/zcutils/adhoc-bootstrap.env"))
        });
    let Some(manifest) = manifest else {
        return true;
    };
    let Ok(contents) = fs::read_to_string(manifest) else {
        return true;
    };
    !(contents
        .lines()
        .any(|line| line == "coordination_scope=dedicated-adhoc-instance")
        && contents
            .lines()
            .any(|line| line == "coordination_honored=true"))
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
