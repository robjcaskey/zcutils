//! Userspace logical-volume partitioning, online resize, and live migration.
//!
//! Placement remains entirely after the client block edge. Each backing in
//! this module is a regular-file terminal leaf selected by userspace. A live
//! migration destination is staged and is not a durability witness until the
//! caller commits the corresponding topology/HWM handoff.

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

pub const IO_ALIGNMENT: u64 = 4096;
const MANIFEST_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionDefinition {
    pub partition_id: String,
    pub start_bytes: u64,
    pub length_bytes: u64,
    pub active_path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    Copying,
    BaseCopied,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationTracking {
    #[default]
    PageGenerations,
    RetainedWal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingMigration {
    pub migration_id: String,
    pub destination_path: PathBuf,
    pub phase: MigrationPhase,
    #[serde(default)]
    pub locality: MigrationLocality,
    #[serde(default)]
    pub tracking: MigrationTracking,
    #[serde(default)]
    pub wal_start_sequence: Option<u64>,
}

/// NUMA placement for migration/snapshot work. When a CPU is supplied, every
/// allocation and copy/replay operation is executed with the calling thread
/// temporarily pinned to it. On Linux, first-touch allocation consequently
/// places the copy buffer, dirty-generation table, and tmpfs/page-cache pages
/// on that CPU's node. `strict` additionally proves the CPU-to-node mapping
/// before any destination is provisioned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationLocality {
    pub preferred_cpu: Option<usize>,
    pub expected_numa_node: Option<u32>,
    pub strict: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PartitionRecord {
    definition: PartitionDefinition,
    pending_migration: Option<PendingMigration>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct VolumeManifest {
    version: u32,
    volume_id: String,
    capacity_bytes: u64,
    alignment_bytes: u64,
    generation: u64,
    partitions: BTreeMap<String, PartitionRecord>,
}

struct MigrationRuntime {
    descriptor: PendingMigration,
    destination: File,
    /// One generation per 4 KiB page. Foreground writes only perform a
    /// relaxed increment after their source pwrite; no migration payload is
    /// duplicated on the hot path.
    dirty_generations: Arc<[AtomicU64]>,
}

#[derive(Clone)]
struct PartitionRoute {
    definition: PartitionDefinition,
    active: Arc<File>,
    migration: Option<Arc<MigrationRuntime>>,
    /// Present only for the standalone page-generation fallback. A retained
    /// WAL migration leaves this absent, so foreground writes execute no
    /// migration atomic or replay bookkeeping.
    dirty_generations: Option<Arc<[AtomicU64]>>,
    admitting: bool,
}

struct PartitionRuntime {
    /// Immutable route snapshots make foreground I/O lock-free. Control-plane
    /// changes are serialized separately and publish with one atomic swap.
    route: ArcSwap<PartitionRoute>,
    control: Mutex<()>,
    retired: AtomicBool,
}

struct VolumeState {
    manifest: VolumeManifest,
    partitions: BTreeMap<String, Arc<PartitionRuntime>>,
}

/// Immutable userspace placement plan for the logical address space. It is
/// rebuilt only by topology changes and atomically published, so normal I/O
/// neither takes the volume control lock nor searches the manifest.
struct VolumeLayout {
    generation: u64,
    capacity_bytes: u64,
    segments: Box<[LogicalSegment]>,
    /// Fast path for a zero-based, contiguous, equal-sized power-of-two leaf
    /// layout. Irregular online layouts retain binary range lookup.
    uniform_leaf_shift: Option<u32>,
}

struct LogicalSegment {
    start_bytes: u64,
    end_bytes: u64,
    runtime: Arc<PartitionRuntime>,
}

pub struct PartitionedVolume {
    root: PathBuf,
    state: RwLock<VolumeState>,
    layout: ArcSwap<VolumeLayout>,
}

/// A lane-local placement snapshot. Resolve it once per placement epoch; leaf
/// route changes, migration cutovers, and retirement remain immediately
/// visible through the referenced partition runtimes.
#[derive(Clone)]
pub struct VolumeIoHandle {
    layout: Arc<VolumeLayout>,
}

/// A pre-resolved userspace lane route. The topology table is consulted once;
/// each I/O only observes the partition route fence needed for migration or
/// resize cutover.
#[derive(Clone)]
pub struct PartitionHandle {
    runtime: Arc<PartitionRuntime>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MigrationStats {
    pub base_bytes_copied: u64,
    pub redo_bytes_replayed: u64,
    pub redo_records_replayed: u64,
    pub cutover_generation: u64,
    pub cutover_fence_ns: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalReplayStats {
    pub records_replayed: u64,
    pub bytes_replayed: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyProgress {
    pub bytes_copied: u64,
    pub total_bytes: u64,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CopyMethod {
    #[default]
    Buffered,
    CopyFileRange,
}

impl PartitionedVolume {
    pub fn create(
        root: impl AsRef<Path>,
        volume_id: impl Into<String>,
        capacity_bytes: u64,
        definitions: Vec<PartitionDefinition>,
    ) -> io::Result<Self> {
        validate_aligned_nonzero(capacity_bytes, "volume capacity")?;
        let volume_id = volume_id.into();
        validate_id(&volume_id, "volume_id")?;
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        if manifest_path(&root).exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "volume manifest already exists",
            ));
        }
        let mut records = BTreeMap::new();
        for definition in definitions {
            validate_definition(&definition, capacity_bytes)?;
            if records.contains_key(&definition.partition_id) {
                return Err(invalid("duplicate partition_id"));
            }
            records.insert(
                definition.partition_id.clone(),
                PartitionRecord {
                    definition,
                    pending_migration: None,
                },
            );
        }
        validate_nonoverlapping(records.values().map(|record| &record.definition))?;
        for record in records.values() {
            provision_regular_file(
                &record.definition.active_path,
                record.definition.length_bytes,
                false,
            )?;
        }
        let manifest = VolumeManifest {
            version: MANIFEST_VERSION,
            volume_id,
            capacity_bytes,
            alignment_bytes: IO_ALIGNMENT,
            generation: 1,
            partitions: records,
        };
        persist_manifest(&root, &manifest)?;
        Self::from_manifest(root, manifest)
    }

    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let body = fs::read(manifest_path(&root))?;
        let manifest: VolumeManifest = serde_json::from_slice(&body)
            .map_err(|error| invalid(format!("decode volume manifest: {error}")))?;
        validate_manifest(&manifest)?;
        Self::from_manifest(root, manifest)
    }

    fn from_manifest(root: PathBuf, manifest: VolumeManifest) -> io::Result<Self> {
        let mut partitions = BTreeMap::new();
        for (partition_id, record) in &manifest.partitions {
            let active = open_regular_file(&record.definition.active_path)?;
            ensure_file_size(&active, record.definition.length_bytes, "active partition")?;
            let migration = record
                .pending_migration
                .as_ref()
                .map(|pending| open_migration(pending, record.definition.length_bytes))
                .transpose()?
                .map(Arc::new);
            let dirty_generations = migration.as_ref().and_then(|migration| {
                (migration.descriptor.tracking == MigrationTracking::PageGenerations)
                    .then(|| Arc::clone(&migration.dirty_generations))
            });
            partitions.insert(
                partition_id.clone(),
                Arc::new(PartitionRuntime {
                    route: ArcSwap::from_pointee(PartitionRoute {
                        definition: record.definition.clone(),
                        active: Arc::new(active),
                        migration,
                        dirty_generations,
                        admitting: true,
                    }),
                    control: Mutex::new(()),
                    retired: AtomicBool::new(false),
                }),
            );
        }
        let state = VolumeState {
            manifest,
            partitions,
        };
        let layout = build_layout(&state)?;
        Ok(Self {
            root,
            state: RwLock::new(state),
            layout: ArcSwap::from_pointee(layout),
        })
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.state
            .read()
            .expect("volume state lock poisoned")
            .manifest
            .capacity_bytes
    }

    pub fn generation(&self) -> u64 {
        self.state
            .read()
            .expect("volume state lock poisoned")
            .manifest
            .generation
    }

    pub fn partitions(&self) -> Vec<PartitionDefinition> {
        self.state
            .read()
            .expect("volume state lock poisoned")
            .manifest
            .partitions
            .values()
            .map(|record| record.definition.clone())
            .collect()
    }

    pub fn partition_handle(&self, partition_id: &str) -> io::Result<PartitionHandle> {
        Ok(PartitionHandle {
            runtime: self.runtime(partition_id)?,
        })
    }

    pub fn io_handle(&self) -> VolumeIoHandle {
        VolumeIoHandle {
            layout: self.layout.load_full(),
        }
    }

    /// Add an aligned partition online. Provisioning happens before manifest
    /// publication, so a crash can leave only an unreferenced terminal file,
    /// never a partition whose backing is missing.
    pub fn create_partition(&self, definition: PartitionDefinition) -> io::Result<()> {
        let mut state = self.state.write().expect("volume state lock poisoned");
        validate_definition(&definition, state.manifest.capacity_bytes)?;
        if state
            .manifest
            .partitions
            .contains_key(&definition.partition_id)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "partition already exists",
            ));
        }
        validate_nonoverlapping(
            state
                .manifest
                .partitions
                .values()
                .map(|record| &record.definition)
                .chain(std::iter::once(&definition)),
        )?;
        provision_regular_file(&definition.active_path, definition.length_bytes, false)?;
        let active = open_regular_file(&definition.active_path)?;
        let record = PartitionRecord {
            definition: definition.clone(),
            pending_migration: None,
        };
        state
            .manifest
            .partitions
            .insert(definition.partition_id.clone(), record);
        let old_generation = state.manifest.generation;
        state.manifest.generation = next_generation(state.manifest.generation)?;
        if let Err(error) = persist_manifest(&self.root, &state.manifest) {
            state.manifest.partitions.remove(&definition.partition_id);
            state.manifest.generation = old_generation;
            return Err(error);
        }
        state.partitions.insert(
            definition.partition_id.clone(),
            Arc::new(PartitionRuntime {
                route: ArcSwap::from_pointee(PartitionRoute {
                    definition,
                    active: Arc::new(active),
                    migration: None,
                    dirty_generations: None,
                    admitting: true,
                }),
                control: Mutex::new(()),
                retired: AtomicBool::new(false),
            }),
        );
        self.publish_layout(&state)?;
        Ok(())
    }

    /// Remove a partition from the routing table without deleting its terminal
    /// file. Existing handles are fenced and fail closed; media reclamation is
    /// a separate explicit lifecycle action after custody release.
    pub fn remove_partition(&self, partition_id: &str) -> io::Result<PartitionDefinition> {
        let mut state = self.state.write().expect("volume state lock poisoned");
        let runtime = state
            .partitions
            .get(partition_id)
            .cloned()
            .ok_or_else(|| invalid(format!("unknown partition {partition_id}")))?;
        let _control = runtime
            .control
            .lock()
            .expect("partition control lock poisoned");
        let route = runtime.route.load_full();
        if route.migration.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cannot remove a partition with an active migration",
            ));
        }
        let mut fenced = (*route).clone();
        fenced.admitting = false;
        let definition = route.definition.clone();
        let old_record = state
            .manifest
            .partitions
            .remove(partition_id)
            .expect("manifest/runtime agree");
        let old_generation = state.manifest.generation;
        state.manifest.generation = next_generation(state.manifest.generation)?;
        if let Err(error) = persist_manifest(&self.root, &state.manifest) {
            state
                .manifest
                .partitions
                .insert(partition_id.to_string(), old_record);
            state.manifest.generation = old_generation;
            return Err(error);
        }
        runtime.publish_and_quiesce(fenced);
        runtime.retired.store(true, Ordering::Release);
        state.partitions.remove(partition_id);
        self.publish_layout(&state)?;
        Ok(definition)
    }

    /// Grow or shrink the logical container online. Shrink is rejected if it
    /// would cut through an existing partition.
    pub fn resize_volume(&self, new_capacity_bytes: u64) -> io::Result<()> {
        validate_aligned_nonzero(new_capacity_bytes, "volume capacity")?;
        let mut state = self.state.write().expect("volume state lock poisoned");
        if state.manifest.partitions.values().any(|record| {
            partition_end(&record.definition)
                .map(|end| end > new_capacity_bytes)
                .unwrap_or(true)
        }) {
            return Err(invalid("volume shrink would truncate a partition"));
        }
        let old_capacity = state.manifest.capacity_bytes;
        let old_generation = state.manifest.generation;
        state.manifest.capacity_bytes = new_capacity_bytes;
        state.manifest.generation = next_generation(state.manifest.generation)?;
        if let Err(error) = persist_manifest(&self.root, &state.manifest) {
            state.manifest.capacity_bytes = old_capacity;
            state.manifest.generation = old_generation;
            return Err(error);
        }
        self.publish_layout(&state)?;
        Ok(())
    }

    /// Resize one partition while I/O to other partitions continues. The
    /// partition's short route write lock fences its own boundary change.
    pub fn resize_partition(&self, partition_id: &str, new_length_bytes: u64) -> io::Result<()> {
        validate_aligned_nonzero(new_length_bytes, "partition length")?;
        let mut state = self.state.write().expect("volume state lock poisoned");
        let runtime = state
            .partitions
            .get(partition_id)
            .cloned()
            .ok_or_else(|| invalid(format!("unknown partition {partition_id}")))?;
        let _control = runtime
            .control
            .lock()
            .expect("partition control lock poisoned");
        let route = runtime.route.load_full();
        if route.migration.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "partition resize is fenced while migration is active",
            ));
        }
        let mut resized = route.definition.clone();
        resized.length_bytes = new_length_bytes;
        validate_definition(&resized, state.manifest.capacity_bytes)?;
        validate_nonoverlapping(state.manifest.partitions.iter().map(|(id, record)| {
            if id == partition_id {
                &resized
            } else {
                &record.definition
            }
        }))?;

        let mut fenced = (*route).clone();
        fenced.admitting = false;
        runtime.publish_and_quiesce(fenced);

        // Grow the terminal first; for shrink publish the smaller boundary
        // first. Either crash ordering leaves a safe superset of bytes. The
        // route is fenced only for this rare geometry transition.
        if new_length_bytes > route.definition.length_bytes {
            if let Err(error) = route.active.set_len(new_length_bytes) {
                runtime.route.store(Arc::new((*route).clone()));
                return Err(error);
            }
            if let Err(error) = route.active.sync_data() {
                runtime.route.store(Arc::new((*route).clone()));
                return Err(error);
            }
        }
        let old_record = state.manifest.partitions[partition_id].clone();
        let old_generation = state.manifest.generation;
        state
            .manifest
            .partitions
            .get_mut(partition_id)
            .expect("known partition")
            .definition = resized.clone();
        state.manifest.generation = next_generation(state.manifest.generation)?;
        if let Err(error) = persist_manifest(&self.root, &state.manifest) {
            state
                .manifest
                .partitions
                .insert(partition_id.to_string(), old_record);
            state.manifest.generation = old_generation;
            runtime.route.store(Arc::new((*route).clone()));
            return Err(error);
        }
        let resized_route = PartitionRoute {
            definition: resized,
            active: Arc::clone(&route.active),
            migration: None,
            dirty_generations: None,
            admitting: true,
        };
        runtime.route.store(Arc::new(resized_route));
        self.publish_layout(&state)?;
        if new_length_bytes < route.definition.length_bytes {
            route.active.set_len(new_length_bytes)?;
            route.active.sync_data()?;
        }
        Ok(())
    }

    pub fn read_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        validate_io(offset, out.len())?;
        self.for_each_segment(
            offset,
            out.len() as u64,
            |runtime, relative, cursor, len| {
                let route = runtime.load_admitted()?;
                read_exact_at(&route.active, &mut out[cursor..cursor + len], relative)
            },
        )
    }

    pub fn write_at(&self, offset: u64, payload: &[u8]) -> io::Result<()> {
        validate_io(offset, payload.len())?;
        self.for_each_segment(
            offset,
            payload.len() as u64,
            |runtime, relative, cursor, len| {
                let route = runtime.load_admitted()?;
                let slice = &payload[cursor..cursor + len];
                write_all_at(&route.active, slice, relative)?;
                if let Some(dirty_generations) = &route.dirty_generations {
                    mark_dirty(dirty_generations, relative, len);
                }
                Ok(())
            },
        )
    }

    pub fn sync(&self) -> io::Result<()> {
        let runtimes: Vec<_> = self
            .state
            .read()
            .expect("volume state lock poisoned")
            .partitions
            .values()
            .cloned()
            .collect();
        for runtime in runtimes {
            let route = runtime.load_admitted()?;
            route.active.sync_data()?;
        }
        Ok(())
    }

    /// Persist a staged destination and redo journal. The destination remains
    /// non-authoritative and reads continue exclusively from the source.
    pub fn begin_migration(
        &self,
        partition_id: &str,
        migration_id: &str,
        destination_path: impl AsRef<Path>,
    ) -> io::Result<()> {
        self.begin_migration_with_locality(
            partition_id,
            migration_id,
            destination_path,
            MigrationLocality::default(),
        )
    }

    pub fn begin_migration_with_locality(
        &self,
        partition_id: &str,
        migration_id: &str,
        destination_path: impl AsRef<Path>,
        locality: MigrationLocality,
    ) -> io::Result<()> {
        self.begin_migration_tracking(
            partition_id,
            migration_id,
            destination_path.as_ref(),
            locality,
            MigrationTracking::PageGenerations,
            None,
        )
    }

    /// Begin a migration whose foreground writes are already retained in the
    /// upstream WAL. This route installs no dirty-generation tracker; callers
    /// must seal it with `commit_migration_with_retained_wal` (or the snapshot
    /// equivalent), which runs replay after admissions are fenced.
    pub fn begin_migration_from_retained_wal_with_locality(
        &self,
        partition_id: &str,
        migration_id: &str,
        destination_path: impl AsRef<Path>,
        locality: MigrationLocality,
        wal_start_sequence: u64,
    ) -> io::Result<()> {
        if wal_start_sequence == 0 {
            return Err(invalid("retained WAL start sequence must be nonzero"));
        }
        self.begin_migration_tracking(
            partition_id,
            migration_id,
            destination_path.as_ref(),
            locality,
            MigrationTracking::RetainedWal,
            Some(wal_start_sequence),
        )
    }

    fn begin_migration_tracking(
        &self,
        partition_id: &str,
        migration_id: &str,
        destination_path: &Path,
        locality: MigrationLocality,
        tracking: MigrationTracking,
        wal_start_sequence: Option<u64>,
    ) -> io::Result<()> {
        validate_id(migration_id, "migration_id")?;
        validate_locality(locality)?;
        let _affinity = ScopedAffinity::apply(locality)?;
        let destination_path = destination_path.to_path_buf();
        if !destination_path.is_absolute() {
            return Err(invalid("migration destination path must be absolute"));
        }
        let mut state = self.state.write().expect("volume state lock poisoned");
        let runtime = state
            .partitions
            .get(partition_id)
            .cloned()
            .ok_or_else(|| invalid(format!("unknown partition {partition_id}")))?;
        let _control = runtime
            .control
            .lock()
            .expect("partition control lock poisoned");
        let route = runtime.route.load_full();
        if route.migration.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "partition migration already active",
            ));
        }
        provision_regular_file(&destination_path, route.definition.length_bytes, true)?;
        reject_same_file(&route.active, &destination_path)?;
        let descriptor = PendingMigration {
            migration_id: migration_id.to_string(),
            destination_path: destination_path.clone(),
            phase: MigrationPhase::Copying,
            locality,
            tracking,
            wal_start_sequence,
        };
        let pages = usize::try_from(route.definition.length_bytes / IO_ALIGNMENT)
            .map_err(|_| invalid("partition page count exceeds usize"))?;
        let page_count = if tracking == MigrationTracking::PageGenerations {
            pages
        } else {
            0
        };
        let migration = Arc::new(MigrationRuntime {
            descriptor: descriptor.clone(),
            destination: open_regular_file(&destination_path)?,
            dirty_generations: Arc::from(
                (0..page_count)
                    .map(|_| AtomicU64::new(0))
                    .collect::<Vec<_>>(),
            ),
        });
        state
            .manifest
            .partitions
            .get_mut(partition_id)
            .expect("known partition")
            .pending_migration = Some(descriptor);
        let old_generation = state.manifest.generation;
        state.manifest.generation = next_generation(state.manifest.generation)?;
        if let Err(error) = persist_manifest(&self.root, &state.manifest) {
            state
                .manifest
                .partitions
                .get_mut(partition_id)
                .expect("known partition")
                .pending_migration = None;
            state.manifest.generation = old_generation;
            return Err(error);
        }
        runtime.publish_and_quiesce(PartitionRoute {
            definition: route.definition.clone(),
            active: Arc::clone(&route.active),
            dirty_generations: (tracking == MigrationTracking::PageGenerations)
                .then(|| Arc::clone(&migration.dirty_generations)),
            migration: Some(migration),
            admitting: true,
        });
        Ok(())
    }

    /// Copy the current source image without blocking foreground I/O. Redo
    /// records make any raced source reads harmless at final replay.
    pub fn copy_migration_base(&self, partition_id: &str, chunk_bytes: usize) -> io::Result<u64> {
        self.copy_migration_base_paced(partition_id, chunk_bytes, 0)
    }

    /// Copy with an optional userspace bandwidth ceiling. A zero ceiling is
    /// unthrottled. Pacing happens after each chunk and never holds a route or
    /// manifest lock, allowing the foreground latency/throughput policy to be
    /// tuned independently of correctness.
    pub fn copy_migration_base_paced(
        &self,
        partition_id: &str,
        chunk_bytes: usize,
        max_bytes_per_second: u64,
    ) -> io::Result<u64> {
        self.copy_migration_base_controlled(partition_id, chunk_bytes, |_| max_bytes_per_second)
    }

    /// Copy with a per-chunk feedback controller. The callback returns the
    /// ceiling for the next chunk, permitting a foreground IOPS/latency loop
    /// to slow or accelerate migration without changing correctness.
    pub fn copy_migration_base_controlled<F>(
        &self,
        partition_id: &str,
        chunk_bytes: usize,
        next_rate: F,
    ) -> io::Result<u64>
    where
        F: FnMut(CopyProgress) -> u64,
    {
        self.copy_migration_base_controlled_with_method(
            partition_id,
            chunk_bytes,
            CopyMethod::Buffered,
            next_rate,
        )
    }

    pub fn copy_migration_base_controlled_with_method<F>(
        &self,
        partition_id: &str,
        chunk_bytes: usize,
        method: CopyMethod,
        mut next_rate: F,
    ) -> io::Result<u64>
    where
        F: FnMut(CopyProgress) -> u64,
    {
        if chunk_bytes == 0 || chunk_bytes as u64 % IO_ALIGNMENT != 0 {
            return Err(invalid("migration chunk must be a non-zero 4096 multiple"));
        }
        let runtime = self.runtime(partition_id)?;
        let (source, migration, length) = {
            let route = runtime.route.load_full();
            (
                Arc::clone(&route.active),
                route
                    .migration
                    .clone()
                    .ok_or_else(|| invalid("partition has no active migration"))?,
                route.definition.length_bytes,
            )
        };
        let _affinity = ScopedAffinity::apply(migration.descriptor.locality)?;
        let mut buffer =
            (method == CopyMethod::Buffered).then(|| vec![0u8; chunk_bytes.min(length as usize)]);
        let mut offset = 0u64;
        let copy_started = Instant::now();
        while offset < length {
            let chunk_started = Instant::now();
            let len = chunk_bytes.min((length - offset) as usize);
            let before = dirty_snapshot(&migration.dirty_generations, offset, len);
            match method {
                CopyMethod::Buffered => {
                    let buffer = buffer.as_mut().expect("buffered copy has a buffer");
                    read_exact_at(&source, &mut buffer[..len], offset)?;
                    write_all_at(&migration.destination, &buffer[..len], offset)?;
                }
                CopyMethod::CopyFileRange => {
                    copy_file_range_all(&source, &migration.destination, offset, len)?
                }
            }
            preserve_raced_dirty(&migration.dirty_generations, offset, &before);
            offset += len as u64;
            let rate = next_rate(CopyProgress {
                bytes_copied: offset,
                total_bytes: length,
                elapsed: copy_started.elapsed(),
            });
            pace_chunk(chunk_started, len as u64, rate);
        }
        migration.destination.sync_data()?;
        self.set_migration_phase(partition_id, MigrationPhase::BaseCopied)?;
        Ok(offset)
    }

    /// Opportunistically drain page-generation fallback dirtiness while
    /// foreground I/O remains admitted. A generation CAS clears only a page
    /// whose source image did not race the copy; raced pages remain for a
    /// later pass or the final fence. Retained-WAL mode should instead replay
    /// a durable prefix and advance its replay cursor.
    pub fn drain_dirty_pages_paced(
        &self,
        partition_id: &str,
        max_pages: usize,
        max_bytes_per_second: u64,
    ) -> io::Result<WalReplayStats> {
        if self
            .pending_migration(partition_id)
            .is_none_or(|pending| pending.phase != MigrationPhase::BaseCopied)
        {
            return Err(invalid("migration base copy is not complete"));
        }
        let runtime = self.runtime(partition_id)?;
        let route = runtime.load_admitted()?;
        let migration = route
            .migration
            .as_ref()
            .ok_or_else(|| invalid("partition has no active migration"))?;
        if migration.descriptor.tracking != MigrationTracking::PageGenerations {
            return Err(invalid("retained-WAL migration has no dirty pages"));
        }
        let _affinity = ScopedAffinity::apply(migration.descriptor.locality)?;
        let mut page = vec![0u8; IO_ALIGNMENT as usize];
        let mut attempted = 0usize;
        let mut records = 0u64;
        for (page_index, generation) in migration.dirty_generations.iter().enumerate() {
            let observed = generation.load(Ordering::Acquire);
            if observed == 0 {
                continue;
            }
            if max_pages != 0 && attempted == max_pages {
                break;
            }
            attempted += 1;
            let page_started = Instant::now();
            let offset = page_index as u64 * IO_ALIGNMENT;
            read_exact_at(&route.active, &mut page, offset)?;
            write_all_at(&migration.destination, &page, offset)?;
            if generation
                .compare_exchange(observed, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                records += 1;
            }
            pace_chunk(page_started, IO_ALIGNMENT, max_bytes_per_second);
        }
        Ok(WalReplayStats {
            records_replayed: records,
            bytes_replayed: records * IO_ALIGNMENT,
        })
    }

    /// Briefly fence this partition, replay its migration redo tail, drain the
    /// destination, durably publish the new route, then release foreground I/O.
    pub fn commit_migration(&self, partition_id: &str) -> io::Result<MigrationStats> {
        self.seal_destination(partition_id, true, |source, migration| {
            if migration.descriptor.tracking != MigrationTracking::PageGenerations {
                return Err(invalid(
                    "retained-WAL migration requires commit_migration_with_retained_wal",
                ));
            }
            replay_dirty_pages(source, &migration.destination, &migration.dirty_generations)
        })
    }

    /// Seal an online point-in-time terminal snapshot without changing the
    /// active route. The same dirty-generation machinery keeps foreground
    /// writes off the snapshot data path until the short final recopy fence.
    pub fn commit_snapshot(&self, partition_id: &str) -> io::Result<MigrationStats> {
        self.seal_destination(partition_id, false, |source, migration| {
            if migration.descriptor.tracking != MigrationTracking::PageGenerations {
                return Err(invalid(
                    "retained-WAL snapshot requires commit_snapshot_with_retained_wal",
                ));
            }
            replay_dirty_pages(source, &migration.destination, &migration.dirty_generations)
        })
    }

    pub fn commit_migration_with_retained_wal<F>(
        &self,
        partition_id: &str,
        replay: F,
    ) -> io::Result<MigrationStats>
    where
        F: FnOnce(&File) -> io::Result<WalReplayStats>,
    {
        self.seal_destination(partition_id, true, move |_source, migration| {
            if migration.descriptor.tracking != MigrationTracking::RetainedWal {
                return Err(invalid("migration is not using retained-WAL tracking"));
            }
            let stats = replay(&migration.destination)?;
            Ok((stats.records_replayed, stats.bytes_replayed))
        })
    }

    pub fn commit_snapshot_with_retained_wal<F>(
        &self,
        partition_id: &str,
        replay: F,
    ) -> io::Result<MigrationStats>
    where
        F: FnOnce(&File) -> io::Result<WalReplayStats>,
    {
        self.seal_destination(partition_id, false, move |_source, migration| {
            if migration.descriptor.tracking != MigrationTracking::RetainedWal {
                return Err(invalid("snapshot is not using retained-WAL tracking"));
            }
            let stats = replay(&migration.destination)?;
            Ok((stats.records_replayed, stats.bytes_replayed))
        })
    }

    fn seal_destination<F>(
        &self,
        partition_id: &str,
        activate_destination: bool,
        replay: F,
    ) -> io::Result<MigrationStats>
    where
        F: FnOnce(&File, &MigrationRuntime) -> io::Result<(u64, u64)>,
    {
        let mut state = self.state.write().expect("volume state lock poisoned");
        let runtime = state
            .partitions
            .get(partition_id)
            .cloned()
            .ok_or_else(|| invalid(format!("unknown partition {partition_id}")))?;
        let _control = runtime
            .control
            .lock()
            .expect("partition control lock poisoned");
        let route = runtime.route.load_full();
        let migration = route
            .migration
            .clone()
            .ok_or_else(|| invalid("partition has no active migration"))?;
        if migration.descriptor.phase != MigrationPhase::BaseCopied
            && state.manifest.partitions[partition_id]
                .pending_migration
                .as_ref()
                .map(|pending| pending.phase)
                != Some(MigrationPhase::BaseCopied)
        {
            return Err(invalid("migration base copy is not complete"));
        }
        let mut fenced = (*route).clone();
        fenced.admitting = false;
        let cutover_started = Instant::now();
        runtime.publish_and_quiesce(fenced);
        let restore_route = || runtime.route.store(Arc::clone(&route));
        let _affinity = match ScopedAffinity::apply(migration.descriptor.locality) {
            Ok(affinity) => affinity,
            Err(error) => {
                restore_route();
                return Err(error);
            }
        };
        let (records, bytes) = match replay(&route.active, &migration) {
            Ok(replayed) => replayed,
            Err(error) => {
                restore_route();
                return Err(error);
            }
        };
        if let Err(error) = migration.destination.sync_data() {
            restore_route();
            return Err(error);
        }

        let old_record = state.manifest.partitions[partition_id].clone();
        let old_generation = state.manifest.generation;
        let record = state
            .manifest
            .partitions
            .get_mut(partition_id)
            .expect("known partition");
        if activate_destination {
            record.definition.active_path = migration.descriptor.destination_path.clone();
        }
        record.pending_migration = None;
        state.manifest.generation = match next_generation(state.manifest.generation) {
            Ok(generation) => generation,
            Err(error) => {
                state
                    .manifest
                    .partitions
                    .insert(partition_id.to_string(), old_record);
                restore_route();
                return Err(error);
            }
        };
        if let Err(error) = persist_manifest(&self.root, &state.manifest) {
            state
                .manifest
                .partitions
                .insert(partition_id.to_string(), old_record);
            state.manifest.generation = old_generation;
            restore_route();
            return Err(error);
        }
        let mut definition = route.definition.clone();
        let active = if activate_destination {
            definition.active_path = migration.descriptor.destination_path.clone();
            match migration.destination.try_clone() {
                Ok(destination) => Arc::new(destination),
                Err(error) => {
                    // The durable manifest already names the destination. A
                    // clone failure is fatal to this process; reopen obtains
                    // the published route without risking a source rollback.
                    runtime.retired.store(true, Ordering::Release);
                    return Err(error);
                }
            }
        } else {
            Arc::clone(&route.active)
        };
        runtime.route.store(Arc::new(PartitionRoute {
            definition: definition.clone(),
            active,
            migration: None,
            dirty_generations: None,
            admitting: true,
        }));
        let cutover_fence_ns = cutover_started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        Ok(MigrationStats {
            base_bytes_copied: definition.length_bytes,
            redo_bytes_replayed: bytes,
            redo_records_replayed: records,
            cutover_generation: state.manifest.generation,
            cutover_fence_ns,
        })
    }

    pub fn migrate_partition(
        &self,
        partition_id: &str,
        migration_id: &str,
        destination_path: impl AsRef<Path>,
        chunk_bytes: usize,
    ) -> io::Result<MigrationStats> {
        self.migrate_partition_paced(partition_id, migration_id, destination_path, chunk_bytes, 0)
    }

    pub fn migrate_partition_paced(
        &self,
        partition_id: &str,
        migration_id: &str,
        destination_path: impl AsRef<Path>,
        chunk_bytes: usize,
        max_bytes_per_second: u64,
    ) -> io::Result<MigrationStats> {
        self.migrate_partition_paced_with_locality(
            partition_id,
            migration_id,
            destination_path,
            chunk_bytes,
            max_bytes_per_second,
            MigrationLocality::default(),
        )
    }

    pub fn migrate_partition_paced_with_locality(
        &self,
        partition_id: &str,
        migration_id: &str,
        destination_path: impl AsRef<Path>,
        chunk_bytes: usize,
        max_bytes_per_second: u64,
        locality: MigrationLocality,
    ) -> io::Result<MigrationStats> {
        self.begin_migration_with_locality(partition_id, migration_id, destination_path, locality)?;
        self.copy_migration_base_paced(partition_id, chunk_bytes, max_bytes_per_second)?;
        self.commit_migration(partition_id)
    }

    pub fn snapshot_partition(
        &self,
        partition_id: &str,
        snapshot_id: &str,
        snapshot_path: impl AsRef<Path>,
        chunk_bytes: usize,
    ) -> io::Result<MigrationStats> {
        self.snapshot_partition_paced(partition_id, snapshot_id, snapshot_path, chunk_bytes, 0)
    }

    pub fn snapshot_partition_paced(
        &self,
        partition_id: &str,
        snapshot_id: &str,
        snapshot_path: impl AsRef<Path>,
        chunk_bytes: usize,
        max_bytes_per_second: u64,
    ) -> io::Result<MigrationStats> {
        self.snapshot_partition_paced_with_locality(
            partition_id,
            snapshot_id,
            snapshot_path,
            chunk_bytes,
            max_bytes_per_second,
            MigrationLocality::default(),
        )
    }

    pub fn snapshot_partition_paced_with_locality(
        &self,
        partition_id: &str,
        snapshot_id: &str,
        snapshot_path: impl AsRef<Path>,
        chunk_bytes: usize,
        max_bytes_per_second: u64,
        locality: MigrationLocality,
    ) -> io::Result<MigrationStats> {
        self.begin_migration_with_locality(partition_id, snapshot_id, snapshot_path, locality)?;
        self.copy_migration_base_paced(partition_id, chunk_bytes, max_bytes_per_second)?;
        self.commit_snapshot(partition_id)
    }

    /// Restore a sealed snapshot into the active terminal. This intentionally
    /// fences the partition for the duration; a non-disruptive restore should
    /// instead restore into a staged destination and use live migration
    /// cutover after application-level validation.
    pub fn restore_partition_from_snapshot(
        &self,
        partition_id: &str,
        snapshot_path: impl AsRef<Path>,
        chunk_bytes: usize,
    ) -> io::Result<u64> {
        if chunk_bytes == 0 || chunk_bytes as u64 % IO_ALIGNMENT != 0 {
            return Err(invalid("restore chunk must be a non-zero 4096 multiple"));
        }
        let snapshot = open_regular_file(snapshot_path.as_ref())?;
        let runtime = self.runtime(partition_id)?;
        let _control = runtime
            .control
            .lock()
            .expect("partition control lock poisoned");
        let route = runtime.route.load_full();
        if route.migration.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "restore is fenced while migration or snapshot capture is active",
            ));
        }
        let mut fenced = (*route).clone();
        fenced.admitting = false;
        runtime.publish_and_quiesce(fenced);
        let result = (|| {
            ensure_file_size(&snapshot, route.definition.length_bytes, "snapshot")?;
            let mut buffer = vec![0u8; chunk_bytes.min(route.definition.length_bytes as usize)];
            let mut offset = 0u64;
            while offset < route.definition.length_bytes {
                let len = buffer
                    .len()
                    .min((route.definition.length_bytes - offset) as usize);
                read_exact_at(&snapshot, &mut buffer[..len], offset)?;
                write_all_at(&route.active, &buffer[..len], offset)?;
                offset += len as u64;
            }
            route.active.sync_data()?;
            Ok(offset)
        })();
        runtime.route.store(route);
        result
    }

    pub fn pending_migration(&self, partition_id: &str) -> Option<PendingMigration> {
        self.state
            .read()
            .expect("volume state lock poisoned")
            .manifest
            .partitions
            .get(partition_id)
            .and_then(|record| record.pending_migration.clone())
    }

    fn set_migration_phase(&self, partition_id: &str, phase: MigrationPhase) -> io::Result<()> {
        let mut state = self.state.write().expect("volume state lock poisoned");
        let record = state
            .manifest
            .partitions
            .get_mut(partition_id)
            .ok_or_else(|| invalid(format!("unknown partition {partition_id}")))?;
        let pending = record
            .pending_migration
            .as_mut()
            .ok_or_else(|| invalid("partition has no pending migration"))?;
        let old_phase = pending.phase;
        pending.phase = phase;
        let old_generation = state.manifest.generation;
        state.manifest.generation = next_generation(state.manifest.generation)?;
        if let Err(error) = persist_manifest(&self.root, &state.manifest) {
            state
                .manifest
                .partitions
                .get_mut(partition_id)
                .and_then(|record| record.pending_migration.as_mut())
                .expect("known pending migration")
                .phase = old_phase;
            state.manifest.generation = old_generation;
            return Err(error);
        }
        Ok(())
    }

    fn runtime(&self, partition_id: &str) -> io::Result<Arc<PartitionRuntime>> {
        self.state
            .read()
            .expect("volume state lock poisoned")
            .partitions
            .get(partition_id)
            .cloned()
            .ok_or_else(|| invalid(format!("unknown partition {partition_id}")))
    }

    fn publish_layout(&self, state: &VolumeState) -> io::Result<()> {
        self.layout.store(Arc::new(build_layout(state)?));
        Ok(())
    }

    fn for_each_segment<F>(&self, offset: u64, length: u64, mut operation: F) -> io::Result<()>
    where
        F: FnMut(&Arc<PartitionRuntime>, u64, usize, usize) -> io::Result<()>,
    {
        // The ArcSwap guard pins this placement generation without adding a
        // shared Arc refcount operation to every I/O.
        let layout = self.layout.load();
        for_each_layout_segment(&layout, offset, length, &mut operation)
    }
}

impl VolumeIoHandle {
    pub fn placement_generation(&self) -> u64 {
        self.layout.generation
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.layout.capacity_bytes
    }

    /// Hot path for the client edge's native 4 KiB request. Placement is
    /// still resolved here in userspace, but the request cannot cross a leaf
    /// boundary and therefore needs no segment-splitting machinery.
    #[inline(always)]
    pub fn read_page_at(
        &self,
        offset: u64,
        out: &mut [u8; IO_ALIGNMENT as usize],
    ) -> io::Result<()> {
        if offset % IO_ALIGNMENT != 0
            || offset
                .checked_add(IO_ALIGNMENT)
                .is_none_or(|end| end > self.layout.capacity_bytes)
        {
            return Err(invalid("4K read exceeds or is misaligned to the volume"));
        }
        let segment = layout_segment_at(&self.layout, offset)?;
        if offset + IO_ALIGNMENT > segment.end_bytes {
            return Err(invalid("4K read crosses a userspace leaf boundary"));
        }
        let route = segment.runtime.load_admitted()?;
        read_exact_at(&route.active, out, offset - segment.start_bytes)
    }

    #[inline(always)]
    pub fn write_page_at(
        &self,
        offset: u64,
        payload: &[u8; IO_ALIGNMENT as usize],
    ) -> io::Result<()> {
        if offset % IO_ALIGNMENT != 0
            || offset
                .checked_add(IO_ALIGNMENT)
                .is_none_or(|end| end > self.layout.capacity_bytes)
        {
            return Err(invalid("4K write exceeds or is misaligned to the volume"));
        }
        let segment = layout_segment_at(&self.layout, offset)?;
        if offset + IO_ALIGNMENT > segment.end_bytes {
            return Err(invalid("4K write crosses a userspace leaf boundary"));
        }
        let relative = offset - segment.start_bytes;
        let route = segment.runtime.load_admitted()?;
        write_all_at(&route.active, payload, relative)?;
        if let Some(dirty_generations) = &route.dirty_generations {
            mark_dirty(dirty_generations, relative, IO_ALIGNMENT as usize);
        }
        Ok(())
    }

    pub fn read_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        validate_io(offset, out.len())?;
        let end = offset
            .checked_add(out.len() as u64)
            .ok_or_else(|| invalid("logical I/O range overflow"))?;
        if end > self.layout.capacity_bytes {
            return Err(invalid("logical I/O exceeds volume capacity"));
        }
        let segment = layout_segment_at(&self.layout, offset)?;
        if end <= segment.end_bytes {
            let route = segment.runtime.load_admitted()?;
            return read_exact_at(&route.active, out, offset - segment.start_bytes);
        }
        for_each_layout_segment(
            &self.layout,
            offset,
            out.len() as u64,
            &mut |runtime, relative, cursor, len| {
                let route = runtime.load_admitted()?;
                read_exact_at(&route.active, &mut out[cursor..cursor + len], relative)
            },
        )
    }

    pub fn write_at(&self, offset: u64, payload: &[u8]) -> io::Result<()> {
        validate_io(offset, payload.len())?;
        let end = offset
            .checked_add(payload.len() as u64)
            .ok_or_else(|| invalid("logical I/O range overflow"))?;
        if end > self.layout.capacity_bytes {
            return Err(invalid("logical I/O exceeds volume capacity"));
        }
        let segment = layout_segment_at(&self.layout, offset)?;
        if end <= segment.end_bytes {
            let relative = offset - segment.start_bytes;
            let route = segment.runtime.load_admitted()?;
            write_all_at(&route.active, payload, relative)?;
            if let Some(dirty_generations) = &route.dirty_generations {
                mark_dirty(dirty_generations, relative, payload.len());
            }
            return Ok(());
        }
        for_each_layout_segment(
            &self.layout,
            offset,
            payload.len() as u64,
            &mut |runtime, relative, cursor, len| {
                let route = runtime.load_admitted()?;
                let slice = &payload[cursor..cursor + len];
                write_all_at(&route.active, slice, relative)?;
                if let Some(dirty_generations) = &route.dirty_generations {
                    mark_dirty(dirty_generations, relative, len);
                }
                Ok(())
            },
        )
    }
}

#[inline(always)]
fn layout_segment_at(layout: &VolumeLayout, logical: u64) -> io::Result<&LogicalSegment> {
    let index = if let Some(shift) = layout.uniform_leaf_shift {
        usize::try_from(logical >> shift).ok().filter(|index| {
            layout
                .segments
                .get(*index)
                .is_some_and(|segment| logical < segment.end_bytes)
        })
    } else {
        layout
            .segments
            .partition_point(|segment| segment.start_bytes <= logical)
            .checked_sub(1)
            .filter(|index| logical < layout.segments[*index].end_bytes)
    }
    .ok_or_else(|| {
        invalid(format!(
            "logical I/O intersects an unmapped gap at {logical}"
        ))
    })?;
    Ok(&layout.segments[index])
}

fn for_each_layout_segment<F>(
    layout: &VolumeLayout,
    offset: u64,
    length: u64,
    operation: &mut F,
) -> io::Result<()>
where
    F: FnMut(&Arc<PartitionRuntime>, u64, usize, usize) -> io::Result<()>,
{
    let end = offset
        .checked_add(length)
        .ok_or_else(|| invalid("logical I/O range overflow"))?;
    if end > layout.capacity_bytes {
        return Err(invalid("logical I/O exceeds volume capacity"));
    }
    let mut logical = offset;
    let mut cursor = 0usize;
    while logical < end {
        let segment = layout_segment_at(layout, logical)?;
        let segment_end = end.min(segment.end_bytes);
        let len = usize::try_from(segment_end - logical)
            .map_err(|_| invalid("logical segment length exceeds usize"))?;
        operation(&segment.runtime, logical - segment.start_bytes, cursor, len)?;
        logical = segment_end;
        cursor += len;
    }
    Ok(())
}

impl PartitionHandle {
    pub fn length_bytes(&self) -> u64 {
        self.runtime.route.load().definition.length_bytes
    }

    pub fn read_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.ensure_active()?;
        validate_io(offset, out.len())?;
        let route = self.runtime.load_admitted()?;
        validate_partition_range(offset, out.len(), route.definition.length_bytes)?;
        read_exact_at(&route.active, out, offset)
    }

    pub fn write_at(&self, offset: u64, payload: &[u8]) -> io::Result<()> {
        self.ensure_active()?;
        validate_io(offset, payload.len())?;
        let route = self.runtime.load_admitted()?;
        validate_partition_range(offset, payload.len(), route.definition.length_bytes)?;
        write_all_at(&route.active, payload, offset)?;
        if let Some(dirty_generations) = &route.dirty_generations {
            mark_dirty(dirty_generations, offset, payload.len());
        }
        Ok(())
    }

    pub fn sync(&self) -> io::Result<()> {
        self.ensure_active()?;
        let route = self.runtime.load_admitted()?;
        route.active.sync_data()
    }

    fn ensure_active(&self) -> io::Result<()> {
        if self.runtime.retired.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "partition route has been retired",
            ));
        }
        Ok(())
    }
}

impl PartitionRuntime {
    fn load_admitted(&self) -> io::Result<Arc<PartitionRoute>> {
        loop {
            if self.retired.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "partition route has been retired",
                ));
            }
            let route = self.route.load_full();
            if route.admitting {
                if self.retired.load(Ordering::Acquire) {
                    continue;
                }
                return Ok(route);
            }
            std::hint::spin_loop();
            thread::yield_now();
        }
    }

    fn publish_and_quiesce(&self, next: PartitionRoute) {
        let previous = self.route.swap(Arc::new(next));
        // Two references are expected here: `previous` and the controller's
        // snapshot used to construct `next`. Any references beyond those are
        // foreground operations that must finish before control proceeds.
        while Arc::strong_count(&previous) > 2 {
            std::hint::spin_loop();
            thread::yield_now();
        }
    }
}

fn open_migration(
    descriptor: &PendingMigration,
    expected_length: u64,
) -> io::Result<MigrationRuntime> {
    let destination = open_regular_file(&descriptor.destination_path)?;
    ensure_file_size(&destination, expected_length, "migration destination")?;
    let pages = usize::try_from(expected_length / IO_ALIGNMENT)
        .map_err(|_| invalid("migration destination page count exceeds usize"))?;
    let page_count = if descriptor.tracking == MigrationTracking::PageGenerations {
        pages
    } else {
        0
    };
    Ok(MigrationRuntime {
        descriptor: descriptor.clone(),
        destination,
        dirty_generations: Arc::from(
            (0..page_count)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>(),
        ),
    })
}

fn mark_dirty(dirty_generations: &[AtomicU64], offset: u64, length: usize) {
    if dirty_generations.is_empty() {
        return;
    }
    let first_page = (offset / IO_ALIGNMENT) as usize;
    let page_count = length / IO_ALIGNMENT as usize;
    for generation in &dirty_generations[first_page..first_page + page_count] {
        generation.fetch_add(1, Ordering::Release);
    }
}

fn dirty_snapshot(dirty_generations: &[AtomicU64], offset: u64, length: usize) -> Vec<u64> {
    if dirty_generations.is_empty() {
        return Vec::new();
    }
    let first_page = (offset / IO_ALIGNMENT) as usize;
    let page_count = length / IO_ALIGNMENT as usize;
    dirty_generations[first_page..first_page + page_count]
        .iter()
        .map(|generation| generation.load(Ordering::Acquire))
        .collect()
}

/// Clear dirtiness already incorporated by the base copy. A write racing the
/// copy changes the generation and makes the compare-exchange fail, leaving
/// that page for the final recopy.
fn preserve_raced_dirty(dirty_generations: &[AtomicU64], offset: u64, before: &[u64]) {
    if dirty_generations.is_empty() {
        return;
    }
    let first_page = (offset / IO_ALIGNMENT) as usize;
    for (generation, observed) in dirty_generations[first_page..]
        .iter()
        .zip(before.iter().copied())
    {
        let _ = generation.compare_exchange(observed, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

fn replay_dirty_pages(
    source: &File,
    destination: &File,
    dirty_generations: &[AtomicU64],
) -> io::Result<(u64, u64)> {
    let mut page = vec![0u8; IO_ALIGNMENT as usize];
    let mut records = 0u64;
    let mut bytes = 0u64;
    for (page_index, generation) in dirty_generations.iter().enumerate() {
        if generation.load(Ordering::Acquire) == 0 {
            continue;
        }
        let offset = page_index as u64 * IO_ALIGNMENT;
        read_exact_at(source, &mut page, offset)?;
        write_all_at(destination, &page, offset)?;
        records += 1;
        bytes += IO_ALIGNMENT;
    }
    Ok((records, bytes))
}

fn pace_chunk(started: Instant, bytes_copied: u64, max_bytes_per_second: u64) {
    if max_bytes_per_second == 0 {
        return;
    }
    let target = Duration::from_secs_f64(bytes_copied as f64 / max_bytes_per_second as f64);
    if let Some(delay) = target.checked_sub(started.elapsed()) {
        thread::sleep(delay);
    }
}

fn validate_locality(locality: MigrationLocality) -> io::Result<()> {
    if locality.strict && locality.preferred_cpu.is_none() {
        return Err(invalid(
            "strict migration locality requires a preferred CPU",
        ));
    }
    let Some(cpu) = locality.preferred_cpu else {
        if locality.expected_numa_node.is_some() {
            return Err(invalid(
                "expected NUMA node requires a preferred migration CPU",
            ));
        }
        return Ok(());
    };
    #[cfg(target_os = "linux")]
    {
        if cpu >= libc::CPU_SETSIZE as usize {
            return Err(invalid(format!(
                "migration CPU {cpu} exceeds CPU affinity set capacity"
            )));
        }
        let actual = linux_cpu_numa_node(cpu)?;
        if let Some(expected) = locality.expected_numa_node {
            if actual != Some(expected) {
                return Err(invalid(format!(
                    "migration CPU {cpu} NUMA mismatch: expected node {expected}, found {}",
                    actual.map_or_else(|| "unknown".into(), |node| node.to_string())
                )));
            }
        } else if locality.strict && actual.is_none() {
            return Err(invalid(format!(
                "strict migration locality cannot determine NUMA node for CPU {cpu}"
            )));
        }
    }
    #[cfg(not(target_os = "linux"))]
    if locality.strict {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("strict NUMA locality is unavailable for CPU {cpu} on this platform"),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_cpu_numa_node(cpu: usize) -> io::Result<Option<u32>> {
    let cpu_path = PathBuf::from(format!("/sys/devices/system/cpu/cpu{cpu}"));
    if !cpu_path.exists() {
        return Err(invalid(format!("migration CPU {cpu} does not exist")));
    }
    for entry in fs::read_dir(cpu_path)? {
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
    Ok(None)
}

struct ScopedAffinity {
    #[cfg(target_os = "linux")]
    previous: Option<libc::cpu_set_t>,
}

impl ScopedAffinity {
    fn apply(locality: MigrationLocality) -> io::Result<Self> {
        validate_locality(locality)?;
        #[cfg(target_os = "linux")]
        {
            let Some(cpu) = locality.preferred_cpu else {
                return Ok(Self { previous: None });
            };
            let mut previous = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
            let size = std::mem::size_of::<libc::cpu_set_t>();
            let rc =
                unsafe { libc::pthread_getaffinity_np(libc::pthread_self(), size, &mut previous) };
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc));
            }
            let mut requested = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
            unsafe {
                libc::CPU_ZERO(&mut requested);
                libc::CPU_SET(cpu, &mut requested);
            }
            let rc =
                unsafe { libc::pthread_setaffinity_np(libc::pthread_self(), size, &requested) };
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc));
            }
            Ok(Self {
                previous: Some(previous),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = locality;
            Ok(Self {})
        }
    }
}

impl Drop for ScopedAffinity {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(previous) = self.previous.take() {
            let rc = unsafe {
                libc::pthread_setaffinity_np(
                    libc::pthread_self(),
                    std::mem::size_of::<libc::cpu_set_t>(),
                    &previous,
                )
            };
            if rc != 0 {
                eprintln!(
                    "zcvolume: WARNING: failed to restore migration thread affinity: {}",
                    io::Error::from_raw_os_error(rc)
                );
            }
        }
    }
}

fn validate_manifest(manifest: &VolumeManifest) -> io::Result<()> {
    if manifest.version != MANIFEST_VERSION || manifest.alignment_bytes != IO_ALIGNMENT {
        return Err(invalid("unsupported volume manifest version or alignment"));
    }
    validate_id(&manifest.volume_id, "volume_id")?;
    validate_aligned_nonzero(manifest.capacity_bytes, "volume capacity")?;
    if manifest.generation == 0 {
        return Err(invalid("volume generation must be nonzero"));
    }
    for (id, record) in &manifest.partitions {
        if id != &record.definition.partition_id {
            return Err(invalid("partition manifest key differs from partition_id"));
        }
        validate_definition(&record.definition, manifest.capacity_bytes)?;
        if let Some(pending) = &record.pending_migration {
            validate_id(&pending.migration_id, "migration_id")?;
            validate_locality(pending.locality)?;
            match (pending.tracking, pending.wal_start_sequence) {
                (MigrationTracking::PageGenerations, None) => {}
                (MigrationTracking::RetainedWal, Some(sequence)) if sequence != 0 => {}
                _ => return Err(invalid("migration tracking metadata is inconsistent")),
            }
            if pending.destination_path == record.definition.active_path {
                return Err(invalid("migration destination aliases active path"));
            }
        }
    }
    validate_nonoverlapping(
        manifest
            .partitions
            .values()
            .map(|record| &record.definition),
    )
}

fn build_layout(state: &VolumeState) -> io::Result<VolumeLayout> {
    let mut segments = state
        .manifest
        .partitions
        .values()
        .map(|record| {
            let definition = &record.definition;
            let runtime = state
                .partitions
                .get(&definition.partition_id)
                .cloned()
                .ok_or_else(|| invalid("partition manifest has no runtime route"))?;
            Ok(LogicalSegment {
                start_bytes: definition.start_bytes,
                end_bytes: partition_end(definition)?,
                runtime,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    segments.sort_unstable_by_key(|segment| segment.start_bytes);
    let uniform_leaf_shift = segments.first().and_then(|first| {
        let bytes = first.end_bytes.checked_sub(first.start_bytes)?;
        (first.start_bytes == 0
            && bytes.is_power_of_two()
            && segments.iter().enumerate().all(|(index, segment)| {
                segment.start_bytes == index as u64 * bytes
                    && segment.end_bytes == (index as u64 + 1) * bytes
            }))
        .then(|| bytes.trailing_zeros())
    });
    Ok(VolumeLayout {
        generation: state.manifest.generation,
        capacity_bytes: state.manifest.capacity_bytes,
        segments: segments.into_boxed_slice(),
        uniform_leaf_shift,
    })
}

fn validate_definition(definition: &PartitionDefinition, capacity: u64) -> io::Result<()> {
    validate_id(&definition.partition_id, "partition_id")?;
    validate_aligned(definition.start_bytes, "partition start")?;
    validate_aligned_nonzero(definition.length_bytes, "partition length")?;
    if partition_end(definition)? > capacity {
        return Err(invalid("partition exceeds volume capacity"));
    }
    if !definition.active_path.is_absolute() {
        return Err(invalid("partition backing path must be absolute"));
    }
    Ok(())
}

fn validate_nonoverlapping<'a>(
    definitions: impl Iterator<Item = &'a PartitionDefinition>,
) -> io::Result<()> {
    let mut ranges = definitions
        .map(|definition| {
            Ok((
                definition.start_bytes,
                partition_end(definition)?,
                definition.partition_id.as_str(),
            ))
        })
        .collect::<io::Result<Vec<_>>>()?;
    ranges.sort_unstable_by_key(|range| range.0);
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(invalid(format!(
                "partitions {} and {} overlap",
                pair[0].2, pair[1].2
            )));
        }
    }
    Ok(())
}

fn partition_end(definition: &PartitionDefinition) -> io::Result<u64> {
    definition
        .start_bytes
        .checked_add(definition.length_bytes)
        .ok_or_else(|| invalid("partition range overflow"))
}

fn provision_regular_file(path: &Path, length: u64, exclusive: bool) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    if exclusive {
        options.create_new(true);
    }
    let file = options.open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(invalid("partition terminal must be a regular file"));
    }
    file.set_len(length)?;
    file.sync_data()
}

fn open_regular_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(invalid("partition terminal must be a regular file"));
    }
    Ok(file)
}

fn ensure_file_size(file: &File, required: u64, label: &str) -> io::Result<()> {
    if file.metadata()?.len() < required {
        return Err(invalid(format!("{label} is shorter than its logical size")));
    }
    Ok(())
}

fn reject_same_file(active: &File, destination: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let active = active.metadata()?;
    let destination = fs::metadata(destination)?;
    if active.dev() == destination.dev() && active.ino() == destination.ino() {
        return Err(invalid(
            "migration source and destination are the same file",
        ));
    }
    Ok(())
}

fn persist_manifest(root: &Path, manifest: &VolumeManifest) -> io::Result<()> {
    let path = manifest_path(root);
    let temporary = root.join("volume-manifest.json.next");
    let encoded = serde_json::to_vec(manifest)
        .map_err(|error| invalid(format!("encode volume manifest: {error}")))?;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)?;
    write_all_at(&file, &encoded, 0)?;
    file.sync_data()?;
    fs::rename(&temporary, &path)?;
    File::open(root)?.sync_all()
}

fn manifest_path(root: &Path) -> PathBuf {
    root.join("volume-manifest.json")
}

fn validate_io(offset: u64, length: usize) -> io::Result<()> {
    if length == 0 || offset % IO_ALIGNMENT != 0 || length as u64 % IO_ALIGNMENT != 0 {
        return Err(invalid("volume I/O must be a non-zero 4096-aligned range"));
    }
    Ok(())
}

fn validate_partition_range(offset: u64, length: usize, partition_bytes: u64) -> io::Result<()> {
    if offset
        .checked_add(length as u64)
        .is_none_or(|end| end > partition_bytes)
    {
        return Err(invalid("I/O exceeds partition boundary"));
    }
    Ok(())
}

fn validate_aligned(value: u64, label: &str) -> io::Result<()> {
    if value % IO_ALIGNMENT != 0 {
        return Err(invalid(format!("{label} must be 4096 aligned")));
    }
    Ok(())
}

fn validate_aligned_nonzero(value: u64, label: &str) -> io::Result<()> {
    if value == 0 {
        return Err(invalid(format!("{label} must be nonzero")));
    }
    validate_aligned(value, label)
}

fn validate_id(value: &str, label: &str) -> io::Result<()> {
    if value.is_empty() || value.contains(['\0', '\n', '/']) {
        return Err(invalid(format!("invalid {label}")));
    }
    Ok(())
}

fn next_generation(generation: u64) -> io::Result<u64> {
    generation
        .checked_add(1)
        .ok_or_else(|| invalid("volume generation overflow"))
}

fn read_exact_at(file: &File, mut out: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !out.is_empty() {
        let read = file.read_at(out, offset)?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short pread"));
        }
        offset += read as u64;
        out = &mut out[read..];
    }
    Ok(())
}

fn write_all_at(file: &File, mut input: &[u8], mut offset: u64) -> io::Result<()> {
    while !input.is_empty() {
        let written = file.write_at(input, offset)?;
        if written == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short pwrite"));
        }
        offset += written as u64;
        input = &input[written..];
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_file_range_all(
    source: &File,
    destination: &File,
    offset: u64,
    len: usize,
) -> io::Result<()> {
    let mut source_offset = i64::try_from(offset).map_err(|_| invalid("source offset overflow"))?;
    let mut destination_offset =
        i64::try_from(offset).map_err(|_| invalid("destination offset overflow"))?;
    let mut remaining = len;
    while remaining != 0 {
        let copied = unsafe {
            libc::copy_file_range(
                source.as_raw_fd(),
                &mut source_offset,
                destination.as_raw_fd(),
                &mut destination_offset,
                remaining,
                0,
            )
        };
        if copied < 0 {
            return Err(io::Error::last_os_error());
        }
        if copied == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "copy_file_range made no progress",
            ));
        }
        remaining -= copied as usize;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn copy_file_range_all(
    _source: &File,
    _destination: &File,
    _offset: u64,
    _len: usize,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "copy_file_range is only available on Linux",
    ))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("zc-volume-{label}-{}-{nonce}", std::process::id()))
    }

    fn definition(root: &Path, id: &str, start: u64, length: u64) -> PartitionDefinition {
        PartitionDefinition {
            partition_id: id.into(),
            start_bytes: start,
            length_bytes: length,
            active_path: root.join(format!("{id}-source.img")),
        }
    }

    #[cfg(target_os = "linux")]
    fn allowed_cpus() -> Vec<usize> {
        let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
        let rc = unsafe {
            libc::pthread_getaffinity_np(
                libc::pthread_self(),
                std::mem::size_of::<libc::cpu_set_t>(),
                &mut set,
            )
        };
        assert_eq!(rc, 0);
        (0..libc::CPU_SETSIZE as usize)
            .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) })
            .collect()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_numa_locality_is_persisted_and_affinity_is_scoped() {
        let before = allowed_cpus();
        let cpu = before[0];
        let Some(node) = linux_cpu_numa_node(cpu).unwrap() else {
            return;
        };
        let root = temp_root("numa-local");
        let bytes = 4 * 1024 * 1024;
        let volume = PartitionedVolume::create(
            &root,
            "v-numa",
            bytes,
            vec![definition(&root, "p0", 0, bytes)],
        )
        .unwrap();
        let locality = MigrationLocality {
            preferred_cpu: Some(cpu),
            expected_numa_node: Some(node),
            strict: true,
        };
        volume
            .begin_migration_with_locality(
                "p0",
                "numa-move",
                root.join("numa-destination.img"),
                locality,
            )
            .unwrap();
        assert_eq!(allowed_cpus(), before);
        assert_eq!(volume.pending_migration("p0").unwrap().locality, locality);
        drop(volume);

        let reopened = PartitionedVolume::open(&root).unwrap();
        assert_eq!(reopened.pending_migration("p0").unwrap().locality, locality);
        reopened.copy_migration_base("p0", 64 * 1024).unwrap();
        reopened.commit_migration("p0").unwrap();
        assert_eq!(allowed_cpus(), before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partition_alignment_overlap_and_online_resize_are_enforced() {
        let root = temp_root("layout");
        let p0 = definition(&root, "p0", 0, 4 * IO_ALIGNMENT);
        let p1 = definition(&root, "p1", 8 * IO_ALIGNMENT, 4 * IO_ALIGNMENT);
        let volume =
            PartitionedVolume::create(&root, "v0", 16 * IO_ALIGNMENT, vec![p0.clone(), p1])
                .unwrap();
        assert!(volume.resize_partition("p0", 9 * IO_ALIGNMENT).is_err());
        assert!(volume.resize_partition("p0", IO_ALIGNMENT + 1).is_err());
        volume.resize_partition("p0", 8 * IO_ALIGNMENT).unwrap();
        assert_eq!(volume.partitions()[0].length_bytes, 8 * IO_ALIGNMENT);
        assert!(volume.resize_volume(11 * IO_ALIGNMENT).is_err());
        volume.resize_volume(20 * IO_ALIGNMENT).unwrap();
        drop(volume);
        let reopened = PartitionedVolume::open(&root).unwrap();
        assert_eq!(reopened.capacity_bytes(), 20 * IO_ALIGNMENT);
        assert_eq!(reopened.partitions()[0].length_bytes, 8 * IO_ALIGNMENT);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn logical_io_uses_sorted_published_layout_and_crosses_leaf_boundaries() {
        let root = temp_root("published-layout");
        let leaf_bytes = 4 * IO_ALIGNMENT;
        // Deliberately choose IDs whose lexical order differs from logical
        // order; the hot placement plan must be sorted by byte range.
        let low = definition(&root, "z-low", 0, leaf_bytes);
        let high = definition(&root, "a-high", leaf_bytes, leaf_bytes);
        let volume =
            PartitionedVolume::create(&root, "v-published-layout", 3 * leaf_bytes, vec![high, low])
                .unwrap();

        let payload = vec![0x6d; 2 * IO_ALIGNMENT as usize];
        volume
            .write_at(leaf_bytes - IO_ALIGNMENT, &payload)
            .unwrap();
        let mut read = vec![0; payload.len()];
        volume
            .read_at(leaf_bytes - IO_ALIGNMENT, &mut read)
            .unwrap();
        assert_eq!(read, payload);

        let io = volume.io_handle();
        let page = [0xa7; IO_ALIGNMENT as usize];
        io.write_page_at(2 * IO_ALIGNMENT, &page).unwrap();
        let mut page_read = [0; IO_ALIGNMENT as usize];
        io.read_page_at(2 * IO_ALIGNMENT, &mut page_read).unwrap();
        assert_eq!(page_read, page);

        assert!(
            volume
                .read_at(2 * leaf_bytes, &mut vec![0; IO_ALIGNMENT as usize])
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_migration_preserves_foreground_writes_and_recovers_new_route() {
        const PARTITION_BYTES: u64 = 32 * 1024 * 1024;
        let root = temp_root("live");
        let source = definition(&root, "data", 0, PARTITION_BYTES);
        let volume = Arc::new(
            PartitionedVolume::create(&root, "v-live", PARTITION_BYTES, vec![source]).unwrap(),
        );
        let initial = vec![0x11; 1024 * 1024];
        for offset in (0..PARTITION_BYTES).step_by(initial.len()) {
            volume.write_at(offset, &initial).unwrap();
        }
        volume.sync().unwrap();
        let destination = root.join("data-destination.img");
        volume
            .begin_migration("data", "move-1", &destination)
            .unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let writes = Arc::new(AtomicU64::new(0));
        let writer_volume = Arc::clone(&volume);
        let writer_running = Arc::clone(&running);
        let writer_writes = Arc::clone(&writes);
        let writer = thread::spawn(move || {
            let mut iteration = 1u64;
            while writer_running.load(Ordering::Acquire) {
                let page = (iteration * 7919) % (PARTITION_BYTES / IO_ALIGNMENT);
                let payload = vec![(iteration as u8).wrapping_mul(17); IO_ALIGNMENT as usize];
                writer_volume
                    .write_at(page * IO_ALIGNMENT, &payload)
                    .unwrap();
                writer_writes.fetch_add(1, Ordering::Relaxed);
                iteration += 1;
            }
        });
        let copied = volume.copy_migration_base("data", 64 * 1024).unwrap();
        assert_eq!(copied, PARTITION_BYTES);
        thread::sleep(Duration::from_millis(10));
        let stats = volume.commit_migration("data").unwrap();
        running.store(false, Ordering::Release);
        writer.join().unwrap();
        assert!(writes.load(Ordering::Relaxed) > 0);
        assert!(stats.redo_records_replayed > 0);
        volume.sync().unwrap();

        let mut expected = vec![0u8; PARTITION_BYTES as usize];
        volume.read_at(0, &mut expected).unwrap();
        drop(volume);
        let reopened = PartitionedVolume::open(&root).unwrap();
        assert_eq!(reopened.partitions()[0].active_path, destination);
        let mut recovered = vec![0u8; PARTITION_BYTES as usize];
        reopened.read_at(0, &mut recovered).unwrap();
        assert_eq!(recovered, expected);
        let final_page = vec![0x7e; IO_ALIGNMENT as usize];
        reopened.write_at(0, &final_page).unwrap();
        reopened.sync().unwrap();
        let mut direct = vec![0u8; IO_ALIGNMENT as usize];
        read_exact_at(&open_regular_file(&destination).unwrap(), &mut direct, 0).unwrap();
        assert_eq!(direct, final_page);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_copy_reopens_and_resumes_from_durable_manifest() {
        let root = temp_root("resume");
        let bytes = 4 * 1024 * 1024;
        let volume = PartitionedVolume::create(
            &root,
            "v-resume",
            bytes,
            vec![definition(&root, "p0", 0, bytes)],
        )
        .unwrap();
        volume
            .begin_migration("p0", "resume-1", root.join("resume-dest.img"))
            .unwrap();
        volume
            .write_at(IO_ALIGNMENT, &vec![0xa5; IO_ALIGNMENT as usize])
            .unwrap();
        volume.sync().unwrap();
        drop(volume);

        let reopened = PartitionedVolume::open(&root).unwrap();
        assert!(reopened.pending_migration("p0").is_some());
        reopened.copy_migration_base("p0", 64 * 1024).unwrap();
        reopened.commit_migration("p0").unwrap();
        let mut page = vec![0u8; IO_ALIGNMENT as usize];
        reopened.read_at(IO_ALIGNMENT, &mut page).unwrap();
        assert_eq!(page, vec![0xa5; IO_ALIGNMENT as usize]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn online_snapshot_seals_a_restorable_cut_without_switching_route() {
        let root = temp_root("snapshot");
        let bytes = 8 * 1024 * 1024;
        let source = definition(&root, "p0", 0, bytes);
        let source_path = source.active_path.clone();
        let volume =
            Arc::new(PartitionedVolume::create(&root, "v-snapshot", bytes, vec![source]).unwrap());
        let snapshot_path = root.join("snapshot.img");
        volume
            .begin_migration("p0", "snapshot-1", &snapshot_path)
            .unwrap();
        volume.copy_migration_base("p0", 64 * 1024).unwrap();
        let cut_page = vec![0x5c; IO_ALIGNMENT as usize];
        volume.write_at(2 * IO_ALIGNMENT, &cut_page).unwrap();
        let stats = volume.commit_snapshot("p0").unwrap();
        assert_eq!(stats.redo_records_replayed, 1);
        assert_eq!(volume.partitions()[0].active_path, source_path);

        volume
            .write_at(2 * IO_ALIGNMENT, &vec![0xe7; IO_ALIGNMENT as usize])
            .unwrap();
        volume
            .restore_partition_from_snapshot("p0", &snapshot_path, 64 * 1024)
            .unwrap();
        let mut restored = vec![0u8; IO_ALIGNMENT as usize];
        volume.read_at(2 * IO_ALIGNMENT, &mut restored).unwrap();
        assert_eq!(restored, cut_page);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retained_wal_replay_has_no_foreground_dirty_tracker() {
        let root = temp_root("retained-wal");
        let bytes = 8 * 1024 * 1024;
        let volume = PartitionedVolume::create(
            &root,
            "v-wal-move",
            bytes,
            vec![definition(&root, "p0", 0, bytes)],
        )
        .unwrap();
        let destination = root.join("wal-destination.img");
        volume
            .begin_migration_from_retained_wal_with_locality(
                "p0",
                "wal-move",
                &destination,
                MigrationLocality::default(),
                1,
            )
            .unwrap();
        let runtime = volume.runtime("p0").unwrap();
        assert!(runtime.route.load().dirty_generations.is_none());
        volume.copy_migration_base("p0", 64 * 1024).unwrap();

        // The upstream stage retains this payload before publishing the source
        // write. The migration route itself performs no dirty bookkeeping.
        let offset = 17 * IO_ALIGNMENT;
        let payload = vec![0xc7; IO_ALIGNMENT as usize];
        volume.write_at(offset, &payload).unwrap();
        let replay_payload = payload.clone();
        let stats = volume
            .commit_migration_with_retained_wal("p0", move |destination| {
                write_all_at(destination, &replay_payload, offset)?;
                Ok(WalReplayStats {
                    records_replayed: 1,
                    bytes_replayed: IO_ALIGNMENT,
                })
            })
            .unwrap();
        assert_eq!(stats.redo_records_replayed, 1);
        assert_eq!(stats.redo_bytes_replayed, IO_ALIGNMENT);
        let mut read = vec![0u8; IO_ALIGNMENT as usize];
        volume.read_at(offset, &mut read).unwrap();
        assert_eq!(read, payload);
        drop(volume);
        let reopened = PartitionedVolume::open(&root).unwrap();
        reopened.read_at(offset, &mut read).unwrap();
        assert_eq!(read, payload);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partition_grows_online_while_old_range_remains_writable() {
        let root = temp_root("online-resize");
        let original = 4 * 1024 * 1024;
        let grown = 8 * 1024 * 1024;
        let volume = Arc::new(
            PartitionedVolume::create(
                &root,
                "v-resize",
                grown,
                vec![definition(&root, "p0", 0, original)],
            )
            .unwrap(),
        );
        let handle = volume.partition_handle("p0").unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let writer_running = Arc::clone(&running);
        let writer = thread::spawn(move || {
            let payload = vec![0x39; IO_ALIGNMENT as usize];
            let mut page = 0u64;
            while writer_running.load(Ordering::Acquire) {
                handle
                    .write_at((page % (original / IO_ALIGNMENT)) * IO_ALIGNMENT, &payload)
                    .unwrap();
                page += 1;
            }
        });
        volume.resize_partition("p0", grown).unwrap();
        let new_page = vec![0xd4; IO_ALIGNMENT as usize];
        volume.write_at(grown - IO_ALIGNMENT, &new_page).unwrap();
        running.store(false, Ordering::Release);
        writer.join().unwrap();
        let mut read = vec![0u8; IO_ALIGNMENT as usize];
        volume.read_at(grown - IO_ALIGNMENT, &mut read).unwrap();
        assert_eq!(read, new_page);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partitions_can_be_added_and_retired_online_with_old_handles_fenced() {
        let root = temp_root("add-remove");
        let volume = PartitionedVolume::create(
            &root,
            "v-partitions",
            16 * IO_ALIGNMENT,
            vec![definition(&root, "p0", 0, 4 * IO_ALIGNMENT)],
        )
        .unwrap();
        let stale_layout = volume.io_handle();
        let p1 = definition(&root, "p1", 8 * IO_ALIGNMENT, 4 * IO_ALIGNMENT);
        volume.create_partition(p1.clone()).unwrap();
        assert!(
            stale_layout
                .write_at(8 * IO_ALIGNMENT, &vec![0x71; IO_ALIGNMENT as usize])
                .is_err()
        );
        let current_layout = volume.io_handle();
        current_layout
            .write_at(8 * IO_ALIGNMENT, &vec![0x71; IO_ALIGNMENT as usize])
            .unwrap();
        let handle = volume.partition_handle("p1").unwrap();
        handle
            .write_at(0, &vec![0x71; IO_ALIGNMENT as usize])
            .unwrap();
        let retired = volume.remove_partition("p1").unwrap();
        assert_eq!(retired, p1);
        assert!(
            handle
                .read_at(0, &mut vec![0; IO_ALIGNMENT as usize])
                .is_err()
        );
        assert_eq!(volume.partitions().len(), 1);
        assert!(retired.active_path.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
