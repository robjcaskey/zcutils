use std::collections::{BTreeMap, HashMap};
use std::process::Command;
use zcutils::dirty_pool::ZcDirtyHwmTracker;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadSource {
    Dirty,
    Reduced,
    Missing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReadResult {
    source: ReadSource,
    value: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirtyEntry {
    seq: u64,
    value: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WalRecord {
    logical: u64,
    value: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WalExtent {
    start: u64,
    records: Vec<WalRecord>,
}

impl WalExtent {
    fn end(&self) -> u64 {
        self.start + self.records.len() as u64
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RetireStats {
    extents: usize,
    records: usize,
}

#[derive(Debug)]
struct LaneDirtyPool {
    next_seq: u64,
    hwm: ZcDirtyHwmTracker,
    wal: BTreeMap<u64, WalExtent>,
    dirty: HashMap<u64, DirtyEntry>,
    reduced: HashMap<u64, u64>,
}

impl LaneDirtyPool {
    fn new() -> Self {
        Self {
            next_seq: 0,
            hwm: ZcDirtyHwmTracker::new(),
            wal: BTreeMap::new(),
            dirty: HashMap::new(),
            reduced: HashMap::new(),
        }
    }

    fn append_extent(&mut self, records: &[(u64, u64)]) -> (u64, u64) {
        let start = self.next_seq;
        let wal_records = records
            .iter()
            .enumerate()
            .map(|(index, (logical, value))| {
                let seq = start + index as u64;
                self.dirty
                    .insert(*logical, DirtyEntry { seq, value: *value });
                WalRecord {
                    logical: *logical,
                    value: *value,
                }
            })
            .collect::<Vec<_>>();
        self.next_seq += wal_records.len() as u64;
        self.wal.insert(
            start,
            WalExtent {
                start,
                records: wal_records,
            },
        );
        (start, self.next_seq)
    }

    fn read(&self, logical: u64) -> ReadResult {
        if let Some(entry) = self.dirty.get(&logical) {
            return ReadResult {
                source: ReadSource::Dirty,
                value: Some(entry.value),
            };
        }
        if let Some(value) = self.reduced.get(&logical) {
            return ReadResult {
                source: ReadSource::Reduced,
                value: Some(*value),
            };
        }
        ReadResult {
            source: ReadSource::Missing,
            value: None,
        }
    }

    fn replicate_through(&mut self, hwm: u64) {
        assert!(hwm <= self.next_seq);
        assert!(hwm >= self.hwm.replica_hwm());
        self.hwm.advance_replica_hwm(hwm);
    }

    fn reduce_through(&mut self, hwm: u64) {
        assert!(hwm <= self.next_seq);
        assert!(hwm >= self.hwm.reduce_hwm());
        let old_hwm = self.hwm.reduce_hwm();
        let extents = self
            .wal
            .range(..hwm)
            .map(|(_, extent)| extent.clone())
            .collect::<Vec<_>>();
        for extent in extents {
            for (index, record) in extent.records.iter().enumerate() {
                let seq = extent.start + index as u64;
                if seq < old_hwm || seq >= hwm {
                    continue;
                }
                self.reduced.insert(record.logical, record.value);
                if self
                    .dirty
                    .get(&record.logical)
                    .is_some_and(|entry| entry.seq == seq)
                {
                    self.dirty.remove(&record.logical);
                }
            }
        }
        self.hwm.advance_reduce_hwm(hwm);
    }

    fn pin_from(&mut self, seq: u64, _owner: impl Into<String>) {
        assert!(seq <= self.next_seq);
        self.hwm.pin_from(seq);
    }

    fn unpin_from(&mut self, seq: u64) {
        self.hwm.unpin_from(seq);
    }

    fn retire_free_ranges(&mut self) -> RetireStats {
        let retired_starts = self
            .wal
            .iter()
            .filter_map(|(start, extent)| (extent.end() <= self.hwm.free_hwm()).then_some(*start))
            .collect::<Vec<_>>();
        let mut stats = RetireStats {
            extents: 0,
            records: 0,
        };
        for start in retired_starts {
            let extent = self.wal.remove(&start).expect("retired extent disappeared");
            stats.extents += 1;
            stats.records += extent.records.len();
        }
        stats
    }

    fn free_hwm(&self) -> u64 {
        self.hwm.free_hwm()
    }
}

#[test]
fn dirty_payload_is_reparented_until_range_hwms_retire_it() {
    let mut lane = LaneDirtyPool::new();
    let (_start, end) = lane.append_extent(&[(10, 111), (11, 222), (12, 333), (13, 444)]);

    assert_eq!(
        lane.read(10),
        ReadResult {
            source: ReadSource::Dirty,
            value: Some(111)
        }
    );
    assert_eq!(lane.free_hwm(), 0);

    lane.replicate_through(end);
    assert_eq!(
        lane.free_hwm(),
        0,
        "replica completion alone must not release dirty payload memory"
    );

    lane.reduce_through(end);
    assert_eq!(
        lane.read(10),
        ReadResult {
            source: ReadSource::Reduced,
            value: Some(111)
        }
    );
    assert_eq!(lane.free_hwm(), end);
    assert_eq!(
        lane.retire_free_ranges(),
        RetireStats {
            extents: 1,
            records: 4
        }
    );
}

#[test]
fn free_hwm_is_minimum_of_replica_reduce_and_active_pins() {
    let mut lane = LaneDirtyPool::new();
    let (_e0_start, e0_end) = lane.append_extent(&[(0, 10), (1, 11), (2, 12), (3, 13)]);
    let (_e1_start, e1_end) = lane.append_extent(&[(4, 14), (5, 15), (6, 16), (7, 17)]);

    lane.pin_from(2, "reader holding dirty slice");
    lane.replicate_through(e1_end);
    lane.reduce_through(e0_end);
    assert_eq!(
        lane.free_hwm(),
        2,
        "active read/snapshot pins cap range release without per-record release accounting"
    );
    assert_eq!(
        lane.retire_free_ranges(),
        RetireStats {
            extents: 0,
            records: 0
        }
    );

    lane.reduce_through(e1_end);
    assert_eq!(lane.free_hwm(), 2);
    lane.unpin_from(2);
    assert_eq!(lane.free_hwm(), e1_end);
    assert_eq!(
        lane.retire_free_ranges(),
        RetireStats {
            extents: 2,
            records: 8
        }
    );
}

#[test]
fn reducing_old_extent_does_not_clear_newer_dirty_overwrite() {
    let mut lane = LaneDirtyPool::new();
    let (_old_start, old_end) = lane.append_extent(&[(7, 100)]);
    let (_new_start, new_end) = lane.append_extent(&[(7, 200)]);

    lane.replicate_through(new_end);
    lane.reduce_through(old_end);
    assert_eq!(
        lane.read(7),
        ReadResult {
            source: ReadSource::Dirty,
            value: Some(200)
        },
        "old range reduction must not clear a newer dirty-map entry"
    );
    assert_eq!(lane.free_hwm(), old_end);

    lane.reduce_through(new_end);
    assert_eq!(
        lane.read(7),
        ReadResult {
            source: ReadSource::Reduced,
            value: Some(200)
        }
    );
    assert_eq!(lane.free_hwm(), new_end);
}

#[test]
fn zcwal_reduce_bench_dirty_cache_smoke_runs_without_block_devices() {
    let output = Command::new(env!("CARGO_BIN_EXE_zcwal-reduce-bench"))
        .args([
            "--mode",
            "mixed",
            "--pattern",
            "random",
            "--lanes",
            "2",
            "--workers",
            "2",
            "--records-per-lane",
            "1024",
            "--block-records-per-lane",
            "128",
            "--extent-records",
            "32",
            "--read-pct",
            "50",
            "--no-pin",
        ])
        .output()
        .expect("failed to run zcwal-reduce-bench dirty-cache smoke");

    assert!(
        output.status.success(),
        "zcwal-reduce-bench failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("zcwal-reduce-summary:"));
    assert!(stdout.contains("dirty_hits="));
    assert!(stdout.contains("wal_extents="));
    assert!(
        !stdout.contains("/dev/"),
        "dirty-cache smoke must remain userspace-only, stdout was:\n{stdout}"
    );
}

#[test]
fn zcwal_reduce_bench_forward_ref_smoke_runs_without_materialized_reads() {
    let output = Command::new(env!("CARGO_BIN_EXE_zcwal-reduce-bench"))
        .args([
            "--mode",
            "mixed",
            "--pattern",
            "random",
            "--read-access",
            "forward-ref",
            "--lanes",
            "2",
            "--workers",
            "2",
            "--records-per-lane",
            "1024",
            "--block-records-per-lane",
            "128",
            "--extent-records",
            "32",
            "--read-pct",
            "50",
            "--forward-window",
            "64",
            "--no-pin",
        ])
        .output()
        .expect("failed to run zcwal-reduce-bench forward-ref smoke");

    assert!(
        output.status.success(),
        "zcwal-reduce-bench forward-ref failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("read_access=forward-ref"));
    assert!(stdout.contains("forward_events="));
    assert!(stdout.contains("forward_completions="));
    assert!(stdout.contains("read_Gbitps=0.000"));
    assert!(
        !stdout.contains("/dev/"),
        "forward-ref smoke must remain userspace-only, stdout was:\n{stdout}"
    );
}

#[test]
fn zcwal_reduce_bench_rejects_write_copy_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_zcwal-reduce-bench"))
        .args(["--write-access", "copy"])
        .output()
        .expect("failed to run zcwal-reduce-bench copy rejection smoke");

    assert!(
        !output.status.success(),
        "write-access=copy must be fatal while the zero-copy dirty-pool contract is unresolved"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("write-access=copy is disabled here")
            && stderr.contains("belongs in another layer"),
        "unexpected stderr for copy rejection:\n{stderr}"
    );
}
