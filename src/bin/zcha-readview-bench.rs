use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::hint::black_box;
use std::io;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use zcutils::ha_metadata::{
    CommittedHaEntry, DataReplica, DataReplicaRole, DurabilityPolicy, GroupConfig, HaCommand,
    HaMetadataStore, ReplicaHwm, stable_id_hash,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("zcha-readview-bench: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let operations = args
        .next()
        .map(|value| parse_u64(&value, "operations"))
        .transpose()?
        .unwrap_or(50_000_000);
    let workers = args
        .next()
        .map(|value| parse_usize(&value, "workers"))
        .transpose()?
        .unwrap_or(1);
    if operations == 0 || workers == 0 {
        return Err(invalid("operations and workers must be nonzero"));
    }
    let cpus = env::var("URING_PLAY_PIN_CPU_LIST")
        .ok()
        .map(|value| parse_cpu_list(&value))
        .transpose()?
        .unwrap_or_default();
    let strict = env_flag("URING_PLAY_TOPOLOGY_STRICT") || env_flag("URING_PLAY_TOPOLOGY_FATAL");
    if cpus.len() < workers {
        let warning = format!(
            "worker CPU pinning missing: workers={workers} supplied_cpus={}; result is non-representative",
            cpus.len()
        );
        if strict {
            return Err(invalid(warning));
        }
        eprintln!("zcha-readview-bench: WARNING: {warning}");
    }

    let path = env::temp_dir().join(format!(
        "zcha-readview-{}-{}.log",
        std::process::id(),
        unix_nanos()
    ));
    let store = HaMetadataStore::open(&path)?;
    let config = GroupConfig {
        group_id: "bench-group".into(),
        volume_id: "bench-volume".into(),
        log_id: "bench-log".into(),
        config_epoch: 1,
        placement_epoch: 1,
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
    };
    store.apply_committed(&CommittedHaEntry {
        index: 1,
        term: 1,
        command: HaCommand::ConfigureGroup {
            config: config.clone(),
        },
    })?;
    store.apply_committed(&CommittedHaEntry {
        index: 2,
        term: 2,
        command: HaCommand::GrantLease {
            group_id: config.group_id.clone(),
            leader_id: "a".into(),
            term: 2,
            config_epoch: 1,
            issued_unix_nanos: 1,
            expires_unix_nanos: u64::MAX,
            quorum_voters: vec!["a".into(), "b".into()],
        },
    })?;
    let report = |replica_id: &str, hwm: u64| ReplicaHwm {
        replica_id: replica_id.into(),
        term: 2,
        config_epoch: 1,
        log_id: config.log_id.clone(),
        lane_hwms: BTreeMap::from([(0, hwm)]),
    };
    store.apply_committed(&CommittedHaEntry {
        index: 3,
        term: 2,
        command: HaCommand::PublishHwm {
            group_id: config.group_id.clone(),
            leader_id: "a".into(),
            term: 2,
            config_epoch: 1,
            reports: vec![report("a", operations + 1), report("b", operations + 1)],
        },
    })?;
    let view = store
        .published_view(&config.group_id)
        .ok_or_else(|| invalid("missing published HA view"))?;
    let leader_hash = stable_id_hash("a");
    let barrier = Arc::new(Barrier::new(workers + 1));
    let per_worker = operations / workers as u64;
    let remainder = operations % workers as u64;
    let mut handles = Vec::with_capacity(workers);
    for worker in 0..workers {
        let worker_operations = per_worker + u64::from((worker as u64) < remainder);
        let view = Arc::clone(&view);
        let barrier = Arc::clone(&barrier);
        let cpu = cpus.get(worker).copied();
        handles.push(thread::spawn(move || -> io::Result<u64> {
            if let Some(cpu) = cpu {
                pin_current_thread(cpu)?;
            }
            barrier.wait();
            let mut authorized = 0u64;
            for hwm in 0..worker_operations {
                authorized += u64::from(black_box(view.authorizes_hash(
                    leader_hash,
                    2,
                    1,
                    2,
                    black_box(hwm),
                )));
            }
            Ok(authorized)
        }));
    }
    let lane_map = (0..workers)
        .map(|worker| match cpus.get(worker) {
            Some(cpu) => format!("lane{worker}:worker{worker}:cpu{cpu}"),
            None => format!("lane{worker}:worker{worker}:cpu-unpinned"),
        })
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "zcha-readview-topology: workers={workers} lanes={workers} per_worker_qd=1 aggregate_outstanding={workers} lane_worker_cpu_map={lane_map} memlock=not-required hugetlb=not-required completion=local-atomic-authority-check representative=no"
    );
    barrier.wait();
    let started = Instant::now();
    let mut authorized = 0u64;
    for handle in handles {
        authorized += handle
            .join()
            .map_err(|_| io::Error::other("authority worker panicked"))??;
    }
    let elapsed = started.elapsed().as_secs_f64();
    let iops = operations as f64 / elapsed.max(f64::MIN_POSITIVE);
    println!(
        "zcha-readview-result: operations={operations} authorized={authorized} seconds={elapsed:.6} authority_checks_per_second={iops:.0} five_million_gate={} scope=lease-term-config-hwm-atomic-snapshot end_to_end_block_iops_claim=no",
        if iops >= 5_000_000.0 { "pass" } else { "fail" }
    );
    drop(store);
    fs::remove_file(path)?;
    Ok(())
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .map(|value| !matches!(value.as_str(), "" | "0" | "false" | "no"))
        .unwrap_or(false)
}

fn parse_cpu_list(value: &str) -> io::Result<Vec<usize>> {
    value
        .split(',')
        .filter(|part| !part.is_empty())
        .map(|part| parse_usize(part, "CPU"))
        .collect()
}

fn parse_u64(value: &str, label: &str) -> io::Result<u64> {
    value
        .parse()
        .map_err(|_| invalid(format!("invalid {label} {value:?}")))
}

fn parse_usize(value: &str, label: &str) -> io::Result<usize> {
    value
        .parse()
        .map_err(|_| invalid(format!("invalid {label} {value:?}")))
}

fn pin_current_thread(cpu: usize) -> io::Result<()> {
    if cpu >= libc::CPU_SETSIZE as usize {
        return Err(invalid(format!("CPU {cpu} exceeds CPU_SETSIZE")));
    }
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::CPU_SET(cpu, &mut set);
    }
    let result =
        unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
