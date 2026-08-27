//! Scale contract for the volume-oriented control plane.
//!
//! Regional placement is private to regional state shards. A single logical
//! relationship directory owns application, database, volume, and consistency
//! edges, but its records are physically partitioned. Federation-level state
//! sees only regional boundary summaries, cross-region relationships, health,
//! fencing, and explicitly coordinated operations.

use crate::gang_scheduler::{EstateScaleEnvelope, FEDERAL_REPATRIATION_4X_DESIGN_ENVELOPE_V1};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;

pub const DEFAULT_SCALE_DENOMINATOR: u64 = 1_000;
pub const SCHEDULER_SHARDS_PER_REGION: u16 = 16;
pub const RELATIONSHIP_DIRECTORY_PARTITIONS: u16 = 4_096;

pub const DATABASE_VOLUME_EDGES: u64 = 24_000_000;
pub const ENVIRONMENT_DATABASE_EDGES: u64 = 8_000_000;
pub const VOLUME_CONSISTENCY_EDGES: u64 = 10_000_000;
pub const APPLICATION_DEPENDENCY_EDGES: u64 = 8_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeightedOrdinalRange {
    pub first_ordinal: u64,
    pub concrete_count: u64,
}

impl WeightedOrdinalRange {
    pub fn end_exclusive(self) -> u64 {
        self.first_ordinal.saturating_add(self.concrete_count)
    }
}

/// A deterministic compressed population. Every representative covers one
/// contiguous range; ranges cover the concrete population without gaps or
/// overlap. This is exact for cardinality and distribution accounting while
/// intentionally approximating individual graph identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedPopulation {
    pub concrete_count: u64,
    pub scale_denominator: u64,
    pub ranges: Vec<WeightedOrdinalRange>,
}

impl ProjectedPopulation {
    pub fn new(concrete_count: u64, scale_denominator: u64) -> io::Result<Self> {
        if concrete_count == 0 || scale_denominator == 0 {
            return Err(invalid("population and scale denominator must be nonzero"));
        }
        let representative_count = concrete_count.div_ceil(scale_denominator);
        let mut ranges = Vec::with_capacity(
            usize::try_from(representative_count)
                .map_err(|_| invalid("representative population does not fit memory"))?,
        );
        let mut first_ordinal = 0u64;
        while first_ordinal < concrete_count {
            ranges.push(WeightedOrdinalRange {
                first_ordinal,
                concrete_count: scale_denominator.min(concrete_count - first_ordinal),
            });
            first_ordinal = first_ordinal.saturating_add(scale_denominator);
        }
        let population = Self {
            concrete_count,
            scale_denominator,
            ranges,
        };
        population.validate()?;
        Ok(population)
    }

    pub fn representative_count(&self) -> u64 {
        u64::try_from(self.ranges.len()).unwrap_or(u64::MAX)
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.concrete_count == 0 || self.scale_denominator == 0 || self.ranges.is_empty() {
            return Err(invalid("invalid projected population geometry"));
        }
        let mut expected = 0u64;
        for range in &self.ranges {
            if range.first_ordinal != expected
                || range.concrete_count == 0
                || range.concrete_count > self.scale_denominator
            {
                return Err(invalid("projected population has a gap or overlap"));
            }
            expected = range.end_exclusive();
        }
        if expected != self.concrete_count {
            return Err(invalid("projected population does not cover its target"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipKind {
    DatabaseBackedByVolume,
    EnvironmentUsesDatabase,
    VolumeConsistency,
    ApplicationDependency,
}

impl RelationshipKind {
    pub fn concrete_count(self) -> u64 {
        match self {
            Self::DatabaseBackedByVolume => DATABASE_VOLUME_EDGES,
            Self::EnvironmentUsesDatabase => ENVIRONMENT_DATABASE_EDGES,
            Self::VolumeConsistency => VOLUME_CONSISTENCY_EDGES,
            Self::ApplicationDependency => APPLICATION_DEPENDENCY_EDGES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipResourceKind {
    Application,
    ApplicationEnvironment,
    LogicalDatabase,
    Volume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RelationshipEndpoint {
    pub kind: RelationshipResourceKind,
    pub ordinal: u64,
    pub region_ordinal: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedRelationship {
    /// Globally unique first concrete relationship ordinal. It is also the
    /// stable id of this compressed representative range.
    pub relationship_id: u64,
    pub concrete_range: WeightedOrdinalRange,
    pub kind: RelationshipKind,
    pub left: RelationshipEndpoint,
    pub right: RelationshipEndpoint,
}

impl ProjectedRelationship {
    pub fn crosses_regions(&self) -> bool {
        self.left.region_ordinal != self.right.region_ordinal
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RegionalStateShardKey {
    pub region_ordinal: u16,
    pub shard_ordinal: u16,
}

impl RegionalStateShardKey {
    pub fn for_volume(region_ordinal: u16, volume_ordinal: u64) -> Self {
        Self {
            region_ordinal,
            shard_ordinal: u16::try_from(
                mix64(volume_ordinal ^ u64::from(region_ordinal))
                    % u64::from(SCHEDULER_SHARDS_PER_REGION),
            )
            .expect("scheduler shard modulus fits u16"),
        }
    }

    pub fn validate(self, region_count: u64) -> io::Result<()> {
        if u64::from(self.region_ordinal) >= region_count
            || self.shard_ordinal >= SCHEDULER_SHARDS_PER_REGION
        {
            return Err(invalid("regional state shard is outside the topology"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipDirectoryPartition {
    pub partition_ordinal: u16,
    pub edges: Vec<ProjectedRelationship>,
    pub concrete_edge_count: u64,
    pub record_hwm: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipCheckpointRef {
    pub epoch: u64,
    pub root_digest: u64,
}

/// A coherent directory cut. `partition_hwms` is the exact vector reduced by
/// the checkpoint epoch; the compact reference is safe to copy into regional
/// summaries and cross-shard manifests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipDirectoryCheckpoint {
    pub reference: RelationshipCheckpointRef,
    pub partition_hwms: Vec<u64>,
    pub representative_edge_count: u64,
    pub concrete_edge_count: u64,
}

/// One logical relationship authority. Each edge has exactly one authoritative
/// partition. Regional adjacency views are derived projections and cannot
/// mutate or independently commit an edge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipDirectory {
    partitions: Vec<RelationshipDirectoryPartition>,
    concrete_edge_count: u64,
    representative_edge_count: u64,
    checkpoint: RelationshipDirectoryCheckpoint,
}

impl RelationshipDirectory {
    pub fn from_projected_edges(edges: Vec<ProjectedRelationship>) -> io::Result<Self> {
        let mut partitions = (0..RELATIONSHIP_DIRECTORY_PARTITIONS)
            .map(|partition_ordinal| RelationshipDirectoryPartition {
                partition_ordinal,
                ..RelationshipDirectoryPartition::default()
            })
            .collect::<Vec<_>>();
        let mut seen = BTreeSet::new();
        let mut concrete_edge_count = 0u64;
        for edge in edges {
            if !seen.insert(edge.relationship_id) {
                return Err(invalid("relationship representative id was duplicated"));
            }
            let partition_ordinal = Self::owner_partition(&edge);
            let partition = &mut partitions[usize::from(partition_ordinal)];
            partition.concrete_edge_count = partition
                .concrete_edge_count
                .saturating_add(edge.concrete_range.concrete_count);
            concrete_edge_count =
                concrete_edge_count.saturating_add(edge.concrete_range.concrete_count);
            partition.edges.push(edge);
        }
        let representative_edge_count = u64::try_from(seen.len()).unwrap_or(u64::MAX);
        for partition in &mut partitions {
            partition.record_hwm = u64::try_from(partition.edges.len()).unwrap_or(u64::MAX);
        }
        let partition_hwms = partitions
            .iter()
            .map(|partition| partition.record_hwm)
            .collect::<Vec<_>>();
        let checkpoint = RelationshipDirectoryCheckpoint {
            reference: RelationshipCheckpointRef {
                epoch: 1,
                root_digest: checkpoint_digest(
                    &partition_hwms,
                    representative_edge_count,
                    concrete_edge_count,
                ),
            },
            partition_hwms,
            representative_edge_count,
            concrete_edge_count,
        };
        let directory = Self {
            partitions,
            concrete_edge_count,
            representative_edge_count,
            checkpoint,
        };
        directory.validate()?;
        Ok(directory)
    }

    pub fn owner_partition(edge: &ProjectedRelationship) -> u16 {
        u16::try_from(
            mix64(
                edge.relationship_id
                    ^ edge.left.ordinal.rotate_left(17)
                    ^ edge.right.ordinal.rotate_left(39),
            ) % u64::from(RELATIONSHIP_DIRECTORY_PARTITIONS),
        )
        .expect("relationship partition modulus fits u16")
    }

    pub fn partitions(&self) -> &[RelationshipDirectoryPartition] {
        &self.partitions
    }

    pub fn concrete_edge_count(&self) -> u64 {
        self.concrete_edge_count
    }

    pub fn representative_edge_count(&self) -> u64 {
        self.representative_edge_count
    }

    pub fn checkpoint(&self) -> &RelationshipDirectoryCheckpoint {
        &self.checkpoint
    }

    pub fn kind_counts(&self) -> BTreeMap<RelationshipKind, u64> {
        let mut counts = BTreeMap::new();
        for edge in self
            .partitions
            .iter()
            .flat_map(|partition| &partition.edges)
        {
            *counts.entry(edge.kind).or_insert(0u64) = counts
                .get(&edge.kind)
                .copied()
                .unwrap_or(0)
                .saturating_add(edge.concrete_range.concrete_count);
        }
        counts
    }

    pub fn region_projection(&self, region_ordinal: u16) -> RegionalRelationshipProjection {
        let mut projection = RegionalRelationshipProjection {
            region_ordinal,
            directory_checkpoint: self.checkpoint.reference,
            ..RegionalRelationshipProjection::default()
        };
        for edge in self
            .partitions
            .iter()
            .flat_map(|partition| &partition.edges)
        {
            if edge.left.region_ordinal == region_ordinal
                || edge.right.region_ordinal == region_ordinal
            {
                projection.visible_concrete_edges = projection
                    .visible_concrete_edges
                    .saturating_add(edge.concrete_range.concrete_count);
                if edge.crosses_regions() {
                    projection.cross_region_concrete_edges = projection
                        .cross_region_concrete_edges
                        .saturating_add(edge.concrete_range.concrete_count);
                } else {
                    projection.local_concrete_edges = projection
                        .local_concrete_edges
                        .saturating_add(edge.concrete_range.concrete_count);
                }
            }
        }
        projection
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.partitions.len() != usize::from(RELATIONSHIP_DIRECTORY_PARTITIONS) {
            return Err(invalid("relationship directory partition count changed"));
        }
        let mut ids = BTreeSet::new();
        let mut concrete = 0u64;
        for (expected, partition) in self.partitions.iter().enumerate() {
            if usize::from(partition.partition_ordinal) != expected {
                return Err(invalid("relationship partitions are not canonical"));
            }
            let mut partition_concrete = 0u64;
            for edge in &partition.edges {
                if Self::owner_partition(edge) != partition.partition_ordinal
                    || !ids.insert(edge.relationship_id)
                    || edge.concrete_range.first_ordinal != edge.relationship_id
                    || edge.concrete_range.concrete_count == 0
                {
                    return Err(invalid("relationship authority invariant failed"));
                }
                partition_concrete =
                    partition_concrete.saturating_add(edge.concrete_range.concrete_count);
            }
            if partition_concrete != partition.concrete_edge_count {
                return Err(invalid("relationship partition count drifted"));
            }
            if partition.record_hwm != u64::try_from(partition.edges.len()).unwrap_or(u64::MAX) {
                return Err(invalid("relationship partition HWM drifted"));
            }
            concrete = concrete.saturating_add(partition_concrete);
        }
        let partition_hwms = self
            .partitions
            .iter()
            .map(|partition| partition.record_hwm)
            .collect::<Vec<_>>();
        if concrete != self.concrete_edge_count
            || u64::try_from(ids.len()).unwrap_or(u64::MAX) != self.representative_edge_count
            || self.checkpoint.reference.epoch == 0
            || self.checkpoint.partition_hwms != partition_hwms
            || self.checkpoint.representative_edge_count != self.representative_edge_count
            || self.checkpoint.concrete_edge_count != self.concrete_edge_count
            || self.checkpoint.reference.root_digest
                != checkpoint_digest(
                    &partition_hwms,
                    self.representative_edge_count,
                    self.concrete_edge_count,
                )
        {
            return Err(invalid("relationship directory totals drifted"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionalRelationshipProjection {
    pub region_ordinal: u16,
    pub directory_checkpoint: RelationshipCheckpointRef,
    pub visible_concrete_edges: u64,
    pub local_concrete_edges: u64,
    pub cross_region_concrete_edges: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusinessEntityPresence {
    pub business_entity_ordinal: u16,
    pub region_ordinals: BTreeSet<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionalConcreteShape {
    pub region_ordinal: u16,
    pub site_ordinals: Vec<u16>,
    pub present_business_entities: BTreeSet<u16>,
    pub concrete_storage_hosts: u64,
    pub concrete_logical_applications: u64,
    pub concrete_application_environments: u64,
    pub concrete_volumes: u64,
    pub concrete_logical_databases: u64,
    /// Exact concrete volume population per region-private scheduler shard.
    pub scheduler_shard_volume_counts: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederalEstateProjection {
    pub target: EstateScaleEnvelope,
    pub scale_denominator: u64,
    pub business_entity_presences: Vec<BusinessEntityPresence>,
    pub regions: Vec<RegionalConcreteShape>,
    pub hosts: ProjectedPopulation,
    pub logical_applications: ProjectedPopulation,
    pub application_environments: ProjectedPopulation,
    pub volumes: ProjectedPopulation,
    pub logical_databases: ProjectedPopulation,
    pub relationship_directory: RelationshipDirectory,
}

impl FederalEstateProjection {
    pub fn build(scale_denominator: u64) -> io::Result<Self> {
        let target = FEDERAL_REPATRIATION_4X_DESIGN_ENVELOPE_V1;
        let relationships = generate_relationship_projection(target, scale_denominator)?;
        let business_entity_presences = generate_business_entity_presences(target)?;
        let regions = generate_regional_shapes(target, &business_entity_presences)?;
        let projection = Self {
            target,
            scale_denominator,
            business_entity_presences,
            regions,
            hosts: ProjectedPopulation::new(target.storage_hosts, scale_denominator)?,
            logical_applications: ProjectedPopulation::new(
                target.logical_applications,
                scale_denominator,
            )?,
            application_environments: ProjectedPopulation::new(
                target.application_environments,
                scale_denominator,
            )?,
            volumes: ProjectedPopulation::new(target.managed_volumes, scale_denominator)?,
            logical_databases: ProjectedPopulation::new(
                target.logical_databases,
                scale_denominator,
            )?,
            relationship_directory: RelationshipDirectory::from_projected_edges(relationships)?,
        };
        projection.validate()?;
        Ok(projection)
    }

    pub fn validate(&self) -> io::Result<()> {
        for population in [
            &self.hosts,
            &self.logical_applications,
            &self.application_environments,
            &self.volumes,
            &self.logical_databases,
        ] {
            population.validate()?;
            if population.scale_denominator != self.scale_denominator {
                return Err(invalid("projection uses mixed scale denominators"));
            }
        }
        if self.business_entity_presences.len()
            != usize::try_from(self.target.business_entities).unwrap_or(usize::MAX)
            || self.regions.len()
                != usize::try_from(self.target.administrative_regions).unwrap_or(usize::MAX)
        {
            return Err(invalid("projection topology cardinality drifted"));
        }
        let mut sites = BTreeSet::new();
        let mut regional_hosts = 0u64;
        let mut regional_applications = 0u64;
        let mut regional_environments = 0u64;
        let mut regional_volumes = 0u64;
        let mut regional_databases = 0u64;
        for (expected_region, region) in self.regions.iter().enumerate() {
            if usize::from(region.region_ordinal) != expected_region
                || region.site_ordinals.is_empty()
                || region.present_business_entities.is_empty()
                || region.scheduler_shard_volume_counts.len()
                    != usize::from(SCHEDULER_SHARDS_PER_REGION)
                || region.scheduler_shard_volume_counts.iter().sum::<u64>()
                    != region.concrete_volumes
            {
                return Err(invalid("regional concrete shape invariant failed"));
            }
            sites.extend(region.site_ordinals.iter().copied());
            regional_hosts = regional_hosts.saturating_add(region.concrete_storage_hosts);
            regional_applications =
                regional_applications.saturating_add(region.concrete_logical_applications);
            regional_environments =
                regional_environments.saturating_add(region.concrete_application_environments);
            regional_volumes = regional_volumes.saturating_add(region.concrete_volumes);
            regional_databases =
                regional_databases.saturating_add(region.concrete_logical_databases);
        }
        if u64::try_from(sites.len()).unwrap_or(u64::MAX) != self.target.failure_domain_sites
            || regional_hosts != self.target.storage_hosts
            || regional_applications != self.target.logical_applications
            || regional_environments != self.target.application_environments
            || regional_volumes != self.target.managed_volumes
            || regional_databases != self.target.logical_databases
        {
            return Err(invalid(
                "regional concrete populations do not total the target",
            ));
        }
        for (expected_entity, presence) in self.business_entity_presences.iter().enumerate() {
            if usize::from(presence.business_entity_ordinal) != expected_entity
                || presence.region_ordinals.len() != 12
                || presence.region_ordinals.iter().any(|region| {
                    !self.regions[usize::from(*region)]
                        .present_business_entities
                        .contains(&presence.business_entity_ordinal)
                })
            {
                return Err(invalid("business entity presence projection drifted"));
            }
        }
        if self.hosts.concrete_count != self.target.storage_hosts
            || self.logical_applications.concrete_count != self.target.logical_applications
            || self.application_environments.concrete_count != self.target.application_environments
            || self.volumes.concrete_count != self.target.managed_volumes
            || self.logical_databases.concrete_count != self.target.logical_databases
            || self.target.database_volume_memberships != DATABASE_VOLUME_EDGES
            || self.relationship_directory.concrete_edge_count() != self.target.relationship_edges
            || self.relationship_directory.kind_counts()
                != BTreeMap::from([
                    (
                        RelationshipKind::DatabaseBackedByVolume,
                        DATABASE_VOLUME_EDGES,
                    ),
                    (
                        RelationshipKind::EnvironmentUsesDatabase,
                        ENVIRONMENT_DATABASE_EDGES,
                    ),
                    (
                        RelationshipKind::VolumeConsistency,
                        VOLUME_CONSISTENCY_EDGES,
                    ),
                    (
                        RelationshipKind::ApplicationDependency,
                        APPLICATION_DEPENDENCY_EDGES,
                    ),
                ])
        {
            return Err(invalid("federal projection does not cover its target"));
        }
        self.relationship_directory.validate()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionalBoundaryValues {
    pub topology_generation: u64,
    pub online: bool,
    pub free_capacity_bytes: u128,
    pub protected_iops: u64,
    pub failover_reserved_bytes: u128,
    pub failover_reserved_iops: u64,
    pub relationship_directory_checkpoint: RelationshipCheckpointRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionalExportSummary {
    pub region_ordinal: u16,
    pub export_generation: u64,
    pub values: RegionalBoundaryValues,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionalScheduleMutation {
    pub shard: RegionalStateShardKey,
    pub expected_local_revision: u64,
    pub affected_volume_count: u64,
    /// Digest of region-private lane, worker, host, and leaf assignments.
    pub private_placement_digest: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionalPrivateScheduleState {
    pub region_ordinal: u16,
    pub local_revision: u64,
    pub private_placement_digest: u64,
    pub exported: RegionalExportSummary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FederationBoundaryEvent {
    RegionalSummaryChanged(RegionalExportSummary),
}

impl RegionalPrivateScheduleState {
    pub fn new(region_ordinal: u16, values: RegionalBoundaryValues) -> Self {
        Self {
            region_ordinal,
            local_revision: 0,
            private_placement_digest: 0,
            exported: RegionalExportSummary {
                region_ordinal,
                export_generation: 1,
                values,
            },
        }
    }

    /// Applies lane/flow placement locally. No federation event is produced:
    /// remote regions neither observe nor acknowledge downstream scheduling.
    pub fn apply_local_schedule_mutation(
        &mut self,
        mutation: RegionalScheduleMutation,
        region_count: u64,
    ) -> io::Result<()> {
        mutation.shard.validate(region_count)?;
        if mutation.shard.region_ordinal != self.region_ordinal
            || mutation.expected_local_revision != self.local_revision
            || mutation.affected_volume_count == 0
        {
            return Err(invalid("invalid regional schedule mutation"));
        }
        self.local_revision = self.local_revision.saturating_add(1);
        self.private_placement_digest = mix64(
            self.private_placement_digest ^ mutation.private_placement_digest ^ self.local_revision,
        );
        Ok(())
    }

    /// Publishes only a boundary-relevant change. Re-publishing identical
    /// values is a no-op and does not create global reconciliation work.
    pub fn publish_boundary_if_changed(
        &mut self,
        values: RegionalBoundaryValues,
    ) -> Option<FederationBoundaryEvent> {
        if self.exported.values == values {
            return None;
        }
        self.exported = RegionalExportSummary {
            region_ordinal: self.region_ordinal,
            export_generation: self.exported.export_generation.saturating_add(1),
            values,
        };
        Some(FederationBoundaryEvent::RegionalSummaryChanged(
            self.exported,
        ))
    }
}

/// Global/federation state contains only exported summaries. Region-private
/// revisions and placement digests are deliberately absent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederationBoundaryState {
    pub regions: BTreeMap<u16, RegionalExportSummary>,
}

impl FederationBoundaryState {
    pub fn apply(&mut self, event: FederationBoundaryEvent) -> io::Result<()> {
        let FederationBoundaryEvent::RegionalSummaryChanged(summary) = event;
        match self.regions.get(&summary.region_ordinal) {
            Some(existing)
                if summary.export_generation != existing.export_generation.saturating_add(1) =>
            {
                return Err(invalid("regional export generation did not advance by one"));
            }
            None if summary.export_generation != 1 => {
                return Err(invalid("first regional export generation must be one"));
            }
            Some(_) | None => {}
        }
        self.regions.insert(summary.region_ordinal, summary);
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardCutParticipant {
    pub shard: RegionalStateShardKey,
    pub member_count: u64,
    pub selector_digest: u64,
}

/// A cross-shard operation carries one bounded participant record per regional
/// state shard, not a global list of volume ids. Each shard resolves and
/// prepares its immutable selector at the same relationship-directory HWM.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossShardCutManifest {
    pub operation_id: String,
    pub relationship_directory_checkpoint: RelationshipCheckpointRef,
    pub participants: Vec<ShardCutParticipant>,
}

impl CrossShardCutManifest {
    pub fn validate(&self, region_count: u64) -> io::Result<()> {
        if self.operation_id.is_empty()
            || self.relationship_directory_checkpoint.epoch == 0
            || self.relationship_directory_checkpoint.root_digest == 0
            || self.participants.is_empty()
        {
            return Err(invalid("cross-shard cut manifest is incomplete"));
        }
        let mut shards = BTreeSet::new();
        for participant in &self.participants {
            participant.shard.validate(region_count)?;
            if participant.member_count == 0 || !shards.insert(participant.shard) {
                return Err(invalid("cross-shard cut participant is invalid"));
            }
        }
        Ok(())
    }

    pub fn total_members(&self) -> u64 {
        self.participants
            .iter()
            .map(|participant| participant.member_count)
            .fold(0u64, u64::saturating_add)
    }

    pub fn validate_at(
        &self,
        region_count: u64,
        expected_checkpoint: RelationshipCheckpointRef,
    ) -> io::Result<()> {
        self.validate(region_count)?;
        if self.relationship_directory_checkpoint != expected_checkpoint {
            return Err(invalid(
                "cross-shard cut does not use the selected relationship checkpoint",
            ));
        }
        Ok(())
    }
}

fn generate_relationship_projection(
    target: EstateScaleEnvelope,
    scale_denominator: u64,
) -> io::Result<Vec<ProjectedRelationship>> {
    if scale_denominator == 0 {
        return Err(invalid("relationship projection scale must be nonzero"));
    }
    let region_count = target.administrative_regions;
    let mut edges = Vec::new();
    let mut global_first = 0u64;
    for kind in [
        RelationshipKind::DatabaseBackedByVolume,
        RelationshipKind::EnvironmentUsesDatabase,
        RelationshipKind::VolumeConsistency,
        RelationshipKind::ApplicationDependency,
    ] {
        let concrete_count = kind.concrete_count();
        let mut kind_first = 0u64;
        while kind_first < concrete_count {
            let count = scale_denominator.min(concrete_count - kind_first);
            let seed = mix64(global_first ^ kind_first.rotate_left(23));
            let left_region = u16::try_from(seed % region_count)
                .map_err(|_| invalid("region count exceeds endpoint representation"))?;
            let representative_ordinal = kind_first / scale_denominator;
            let cross_region = match kind {
                RelationshipKind::DatabaseBackedByVolume => false,
                RelationshipKind::EnvironmentUsesDatabase => representative_ordinal % 10 == 0,
                RelationshipKind::VolumeConsistency => representative_ordinal % 20 == 0,
                RelationshipKind::ApplicationDependency => representative_ordinal % 5 == 0,
            };
            let right_region = if cross_region {
                u16::try_from(
                    (u64::from(left_region) + 1 + seed % (region_count - 1)) % region_count,
                )
                .map_err(|_| invalid("region count exceeds endpoint representation"))?
            } else {
                left_region
            };
            let (left_kind, left_total, right_kind, right_total) = match kind {
                RelationshipKind::DatabaseBackedByVolume => (
                    RelationshipResourceKind::LogicalDatabase,
                    target.logical_databases,
                    RelationshipResourceKind::Volume,
                    target.managed_volumes,
                ),
                RelationshipKind::EnvironmentUsesDatabase => (
                    RelationshipResourceKind::ApplicationEnvironment,
                    target.application_environments,
                    RelationshipResourceKind::LogicalDatabase,
                    target.logical_databases,
                ),
                RelationshipKind::VolumeConsistency => (
                    RelationshipResourceKind::Volume,
                    target.managed_volumes,
                    RelationshipResourceKind::Volume,
                    target.managed_volumes,
                ),
                RelationshipKind::ApplicationDependency => (
                    RelationshipResourceKind::Application,
                    target.logical_applications,
                    RelationshipResourceKind::Application,
                    target.logical_applications,
                ),
            };
            let left = RelationshipEndpoint {
                kind: left_kind,
                ordinal: ordinal_in_region(left_total, region_count, left_region, seed),
                region_ordinal: left_region,
            };
            let right = RelationshipEndpoint {
                kind: right_kind,
                ordinal: ordinal_in_region(
                    right_total,
                    region_count,
                    right_region,
                    mix64(seed ^ 0xa076_1d64_78bd_642f),
                ),
                region_ordinal: right_region,
            };
            edges.push(ProjectedRelationship {
                relationship_id: global_first,
                concrete_range: WeightedOrdinalRange {
                    first_ordinal: global_first,
                    concrete_count: count,
                },
                kind,
                left,
                right,
            });
            global_first = global_first.saturating_add(count);
            kind_first = kind_first.saturating_add(count);
        }
    }
    if global_first != target.relationship_edges {
        return Err(invalid(
            "relationship edge categories do not total the target",
        ));
    }
    Ok(edges)
}

fn generate_business_entity_presences(
    target: EstateScaleEnvelope,
) -> io::Result<Vec<BusinessEntityPresence>> {
    let entity_count = u16::try_from(target.business_entities)
        .map_err(|_| invalid("business entity target exceeds modeled ordinal"))?;
    let region_count = u16::try_from(target.administrative_regions)
        .map_err(|_| invalid("region target exceeds modeled ordinal"))?;
    let mut presences = Vec::with_capacity(usize::from(entity_count));
    for business_entity_ordinal in 0..entity_count {
        let region_ordinals = (0..12u16)
            .map(|offset| {
                business_entity_ordinal
                    .saturating_mul(4)
                    .wrapping_add(offset)
                    % region_count
            })
            .collect::<BTreeSet<_>>();
        presences.push(BusinessEntityPresence {
            business_entity_ordinal,
            region_ordinals,
        });
    }
    Ok(presences)
}

fn generate_regional_shapes(
    target: EstateScaleEnvelope,
    business_entity_presences: &[BusinessEntityPresence],
) -> io::Result<Vec<RegionalConcreteShape>> {
    let region_count = u16::try_from(target.administrative_regions)
        .map_err(|_| invalid("region target exceeds modeled ordinal"))?;
    let sites_per_region = target.failure_domain_sites / target.administrative_regions;
    if sites_per_region == 0
        || sites_per_region * target.administrative_regions != target.failure_domain_sites
    {
        return Err(invalid("failure-domain sites do not divide across regions"));
    }
    let mut regions = Vec::with_capacity(usize::from(region_count));
    for region_ordinal in 0..region_count {
        let region_index = u64::from(region_ordinal);
        let present_business_entities = business_entity_presences
            .iter()
            .filter(|presence| presence.region_ordinals.contains(&region_ordinal))
            .map(|presence| presence.business_entity_ordinal)
            .collect::<BTreeSet<_>>();
        let concrete_volumes = partition_count(
            target.managed_volumes,
            target.administrative_regions,
            region_index,
        );
        let scheduler_shard_volume_counts = (0..SCHEDULER_SHARDS_PER_REGION)
            .map(|shard| {
                partition_count(
                    concrete_volumes,
                    u64::from(SCHEDULER_SHARDS_PER_REGION),
                    u64::from(shard),
                )
            })
            .collect();
        regions.push(RegionalConcreteShape {
            region_ordinal,
            site_ordinals: (0..sites_per_region)
                .map(|offset| {
                    u16::try_from(region_index * sites_per_region + offset)
                        .map_err(|_| invalid("site ordinal exceeds modeled representation"))
                })
                .collect::<io::Result<Vec<_>>>()?,
            present_business_entities,
            concrete_storage_hosts: partition_count(
                target.storage_hosts,
                target.administrative_regions,
                region_index,
            ),
            concrete_logical_applications: partition_count(
                target.logical_applications,
                target.administrative_regions,
                region_index,
            ),
            concrete_application_environments: partition_count(
                target.application_environments,
                target.administrative_regions,
                region_index,
            ),
            concrete_volumes,
            concrete_logical_databases: partition_count(
                target.logical_databases,
                target.administrative_regions,
                region_index,
            ),
            scheduler_shard_volume_counts,
        });
    }
    Ok(regions)
}

fn partition_count(total: u64, partitions: u64, partition: u64) -> u64 {
    total / partitions + u64::from(partition < total % partitions)
}

fn ordinal_in_region(total: u64, regions: u64, region: u16, seed: u64) -> u64 {
    let region = u64::from(region);
    let members = total.saturating_sub(region).div_ceil(regions).max(1);
    let candidate = region.saturating_add((seed % members).saturating_mul(regions));
    if candidate < total { candidate } else { region }
}

fn mix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn checkpoint_digest(
    partition_hwms: &[u64],
    representative_edge_count: u64,
    concrete_edge_count: u64,
) -> u64 {
    partition_hwms.iter().enumerate().fold(
        mix64(representative_edge_count ^ concrete_edge_count.rotate_left(31)),
        |digest, (partition, hwm)| {
            mix64(digest ^ hwm.rotate_left(u32::try_from(partition % 63).unwrap_or(0)))
        },
    )
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint_reference() -> RelationshipCheckpointRef {
        RelationshipCheckpointRef {
            epoch: 1,
            root_digest: 0xfeed_cafe,
        }
    }

    fn boundary_values() -> RegionalBoundaryValues {
        RegionalBoundaryValues {
            topology_generation: 7,
            online: true,
            free_capacity_bytes: 100_000_000_000_000,
            protected_iops: 12_000_000,
            failover_reserved_bytes: 25_000_000_000_000,
            failover_reserved_iops: 1_000_000,
            relationship_directory_checkpoint: checkpoint_reference(),
        }
    }

    #[test]
    fn one_to_one_thousand_projection_exactly_covers_the_federal_4x_estate() {
        let projection = FederalEstateProjection::build(DEFAULT_SCALE_DENOMINATOR).unwrap();
        assert_eq!(projection.hosts.representative_count(), 3_500);
        assert_eq!(projection.logical_applications.representative_count(), 400);
        assert_eq!(
            projection.application_environments.representative_count(),
            2_800
        );
        assert_eq!(projection.volumes.representative_count(), 12_000);
        assert_eq!(projection.logical_databases.representative_count(), 8_000);
        assert_eq!(projection.business_entity_presences.len(), 128);
        assert_eq!(projection.regions.len(), 512);
        assert!(
            projection
                .business_entity_presences
                .iter()
                .all(|presence| presence.region_ordinals.len() == 12)
        );
        assert!(projection.regions.iter().all(|region| {
            region.site_ordinals.len() == 2
                && region.present_business_entities.len() == 3
                && region.scheduler_shard_volume_counts.len() == 16
        }));
        assert_eq!(
            projection
                .regions
                .iter()
                .map(|region| region.scheduler_shard_volume_counts.len())
                .sum::<usize>(),
            8_192
        );
        assert_eq!(
            projection
                .relationship_directory
                .representative_edge_count(),
            50_000
        );
        assert_eq!(
            projection.relationship_directory.concrete_edge_count(),
            50_000_000
        );
        assert_eq!(
            projection.relationship_directory.kind_counts(),
            BTreeMap::from([
                (RelationshipKind::DatabaseBackedByVolume, 24_000_000),
                (RelationshipKind::EnvironmentUsesDatabase, 8_000_000),
                (RelationshipKind::VolumeConsistency, 10_000_000),
                (RelationshipKind::ApplicationDependency, 8_000_000),
            ])
        );
        projection.validate().unwrap();
    }

    #[test]
    fn relationship_authority_is_unique_and_region_views_are_projections() {
        let projection = FederalEstateProjection::build(DEFAULT_SCALE_DENOMINATOR).unwrap();
        let directory = &projection.relationship_directory;
        let mut ids = BTreeSet::new();
        for partition in directory.partitions() {
            for edge in &partition.edges {
                assert!(ids.insert(edge.relationship_id));
                assert_eq!(
                    RelationshipDirectory::owner_partition(edge),
                    partition.partition_ordinal
                );
            }
        }
        assert_eq!(ids.len(), 50_000);
        let cross_region = directory
            .partitions()
            .iter()
            .flat_map(|partition| &partition.edges)
            .filter(|edge| edge.crosses_regions())
            .map(|edge| edge.concrete_range.concrete_count)
            .sum::<u64>();
        assert_eq!(cross_region, 2_900_000);
        let region = directory.region_projection(17);
        assert!(region.visible_concrete_edges > 0);
        assert!(region.local_concrete_edges > 0);
        assert!(region.cross_region_concrete_edges > 0);
        assert_eq!(
            region.visible_concrete_edges,
            region.local_concrete_edges + region.cross_region_concrete_edges
        );
        assert_eq!(
            region.directory_checkpoint,
            directory.checkpoint().reference
        );
    }

    #[test]
    fn downstream_schedule_changes_are_private_until_a_boundary_changes() {
        let target = FEDERAL_REPATRIATION_4X_DESIGN_ENVELOPE_V1;
        let mut regional = RegionalPrivateScheduleState::new(9, boundary_values());
        let mut federation = FederationBoundaryState::default();
        federation
            .apply(FederationBoundaryEvent::RegionalSummaryChanged(
                regional.exported,
            ))
            .unwrap();
        let before = federation.clone();

        for revision in 0..10_000 {
            regional
                .apply_local_schedule_mutation(
                    RegionalScheduleMutation {
                        shard: RegionalStateShardKey::for_volume(9, revision),
                        expected_local_revision: revision,
                        affected_volume_count: 1 + revision % 32,
                        private_placement_digest: mix64(revision),
                    },
                    target.administrative_regions,
                )
                .unwrap();
        }
        assert_eq!(federation, before);
        assert_eq!(regional.local_revision, 10_000);
        assert_eq!(
            regional.publish_boundary_if_changed(boundary_values()),
            None
        );

        let mut changed = boundary_values();
        changed.free_capacity_bytes -= 1_000_000_000_000;
        let event = regional.publish_boundary_if_changed(changed).unwrap();
        federation.apply(event).unwrap();
        assert_ne!(federation, before);
        assert_eq!(federation.regions[&9].export_generation, 2);
    }

    #[test]
    fn cross_shard_cut_names_only_participants_at_one_directory_hwm() {
        let manifest = CrossShardCutManifest {
            operation_id: "quarter-close".to_string(),
            relationship_directory_checkpoint: checkpoint_reference(),
            participants: vec![
                ShardCutParticipant {
                    shard: RegionalStateShardKey {
                        region_ordinal: 7,
                        shard_ordinal: 3,
                    },
                    member_count: 8_000,
                    selector_digest: 11,
                },
                ShardCutParticipant {
                    shard: RegionalStateShardKey {
                        region_ordinal: 311,
                        shard_ordinal: 12,
                    },
                    member_count: 2_500,
                    selector_digest: 12,
                },
            ],
        };
        manifest.validate_at(512, checkpoint_reference()).unwrap();
        assert_eq!(manifest.total_members(), 10_500);

        let mut duplicate = manifest.clone();
        duplicate
            .participants
            .push(duplicate.participants[0].clone());
        assert!(duplicate.validate(512).is_err());
        let wrong_checkpoint = RelationshipCheckpointRef {
            epoch: 2,
            root_digest: checkpoint_reference().root_digest,
        };
        assert!(manifest.validate_at(512, wrong_checkpoint).is_err());
    }
}
