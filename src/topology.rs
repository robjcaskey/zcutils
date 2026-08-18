//! Dynamic, event-applied topology and durability ownership.
//!
//! This is a userspace control-plane model. It never selects placement in the
//! kernel block edge. Arbitrary typed characteristics describe entities and
//! relationships; durability obligations evaluate those facts at a requested
//! WAL high-water mark.

use crate::change_log::{
    CHANGE_BATCH_SCHEMA_VERSION, ChangeBatch, ChangeLogStore, CommittedChangeBatch,
    ComponentChange, content_hash,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

pub const TOPOLOGY_STREAM: &str = "zccusan.topology.v1";
const TOPOLOGY_COMPONENT: &str = "zcutils.topology";

pub type Characteristics = BTreeMap<String, Value>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub entity_id: String,
    pub kind: String,
    pub characteristics: Characteristics,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relationship {
    pub relationship_id: String,
    pub kind: String,
    pub from_entity: String,
    pub to_entity: String,
    pub characteristics: Characteristics,
}

/// A policy assembled from facts rather than a fixed topology DSL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurabilityObligation {
    pub obligation_id: String,
    pub group_id: String,
    pub required_copies: usize,
    /// Each named characteristic must have this many distinct values among
    /// the selected witnesses (for example `failure.host` or `failure.az`).
    pub distinct: BTreeMap<String, usize>,
    /// Minimum witness counts by the entity's `durability.role` fact.
    pub required_roles: BTreeMap<String, usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustodyState {
    /// Receiving and validating a handoff, but not yet a durability witness.
    Staged,
    Active,
    PendingRelease,
    Released,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustodyLease {
    pub replica_id: String,
    pub group_id: String,
    pub owner_entity: String,
    pub log_id: String,
    pub incarnation: u64,
    pub term: u64,
    pub topology_epoch: u64,
    pub state: CustodyState,
    #[serde(with = "lane_hwm_map")]
    pub retain_through_lane_hwm: BTreeMap<u32, u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    Staged,
    CaughtUp,
    Activated,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub handoff_id: String,
    pub group_id: String,
    pub from_replica: String,
    pub to_replica: String,
    #[serde(with = "lane_hwm_map")]
    pub target_lane_hwm: BTreeMap<u32, u64>,
    pub state: HandoffState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum TopologyCommand {
    UpsertEntity {
        entity: Entity,
    },
    PatchEntityCharacteristics {
        entity_id: String,
        set: Characteristics,
        remove: BTreeSet<String>,
    },
    RemoveEntity {
        entity_id: String,
    },
    UpsertRelationship {
        relationship: Relationship,
    },
    RemoveRelationship {
        relationship_id: String,
    },
    SetObligation {
        obligation: DurabilityObligation,
    },
    RemoveObligation {
        obligation_id: String,
    },
    GrantCustody {
        lease: CustodyLease,
    },
    AdvanceCustodyHwm {
        replica_id: String,
        term: u64,
        #[serde(with = "lane_hwm_map")]
        lane_hwms: BTreeMap<u32, u64>,
    },
    BeginHandoff {
        handoff: Handoff,
    },
    MarkHandoffCaughtUp {
        handoff_id: String,
    },
    ActivateHandoff {
        handoff_id: String,
    },
    /// Activate a caught-up staged target while retaining the source.  This is
    /// the grow/copy counterpart to `ActivateHandoff`, which is a move.
    ActivateStagedCustody {
        handoff_id: String,
    },
    PrepareCustodyRelease {
        replica_id: String,
    },
    ReleaseCustody {
        replica_id: String,
        obligation_id: String,
        #[serde(with = "lane_hwm_map")]
        through_lane_hwm: BTreeMap<u32, u64>,
    },
    RetireCustody {
        replica_id: String,
    },
    RemoveHandoff {
        handoff_id: String,
    },
}

mod lane_hwm_map {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S>(value: &BTreeMap<u32, u64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value
            .iter()
            .map(|(&lane, &hwm)| (lane, hwm))
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BTreeMap<u32, u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Vec::<(u32, u64)>::deserialize(deserializer)?
            .into_iter()
            .collect())
    }
}

impl TopologyCommand {
    fn entity_key(&self) -> &str {
        match self {
            Self::UpsertEntity { entity } => &entity.entity_id,
            Self::PatchEntityCharacteristics { entity_id, .. } => entity_id,
            Self::RemoveEntity { entity_id } => entity_id,
            Self::UpsertRelationship { relationship } => &relationship.relationship_id,
            Self::RemoveRelationship { relationship_id } => relationship_id,
            Self::SetObligation { obligation } => &obligation.obligation_id,
            Self::RemoveObligation { obligation_id } => obligation_id,
            Self::GrantCustody { lease } => &lease.replica_id,
            Self::AdvanceCustodyHwm { replica_id, .. }
            | Self::PrepareCustodyRelease { replica_id }
            | Self::ReleaseCustody { replica_id, .. }
            | Self::RetireCustody { replica_id } => replica_id,
            Self::BeginHandoff { handoff } => &handoff.handoff_id,
            Self::MarkHandoffCaughtUp { handoff_id }
            | Self::ActivateHandoff { handoff_id }
            | Self::ActivateStagedCustody { handoff_id }
            | Self::RemoveHandoff { handoff_id } => handoff_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedTopologyBatch {
    pub raft_term: u64,
    pub raft_index: u64,
    pub transaction_id: String,
    pub expected_epoch: u64,
    pub new_epoch: u64,
    pub commands: Vec<TopologyCommand>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyState {
    pub applied_term: u64,
    pub applied_index: u64,
    pub topology_epoch: u64,
    pub entities: BTreeMap<String, Entity>,
    pub relationships: BTreeMap<String, Relationship>,
    pub obligations: BTreeMap<String, DurabilityObligation>,
    pub custody: BTreeMap<String, CustodyLease>,
    pub handoffs: BTreeMap<String, Handoff>,
}

impl TopologyState {
    pub fn apply_batch(&mut self, batch: &CommittedTopologyBatch) -> io::Result<()> {
        if batch.commands.is_empty() {
            return Err(invalid("topology transaction must not be empty"));
        }
        if batch.raft_term == 0 || batch.raft_index != self.applied_index.saturating_add(1) {
            return Err(invalid(
                "topology Raft term/index is invalid or non-contiguous",
            ));
        }
        if batch.raft_term < self.applied_term {
            return Err(invalid("topology Raft term regressed"));
        }
        if batch.expected_epoch != self.topology_epoch
            || batch.new_epoch != batch.expected_epoch.saturating_add(1)
        {
            return Err(invalid("topology epoch conflict"));
        }
        validate_id(&batch.transaction_id, "transaction_id")?;
        let mut staged = self.clone();
        for command in &batch.commands {
            staged.apply_command(command, batch.new_epoch)?;
        }
        staged.applied_term = batch.raft_term;
        staged.applied_index = batch.raft_index;
        staged.topology_epoch = batch.new_epoch;
        *self = staged;
        Ok(())
    }

    fn apply_command(&mut self, command: &TopologyCommand, new_epoch: u64) -> io::Result<()> {
        match command {
            TopologyCommand::UpsertEntity { entity } => {
                validate_id(&entity.entity_id, "entity_id")?;
                validate_id(&entity.kind, "entity kind")?;
                validate_characteristics(&entity.characteristics)?;
                self.entities
                    .insert(entity.entity_id.clone(), entity.clone());
            }
            TopologyCommand::PatchEntityCharacteristics {
                entity_id,
                set,
                remove,
            } => {
                validate_characteristics(set)?;
                if set.keys().any(|key| remove.contains(key)) {
                    return Err(invalid(
                        "a characteristic cannot be set and removed together",
                    ));
                }
                let entity = self
                    .entities
                    .get_mut(entity_id)
                    .ok_or_else(|| invalid(format!("unknown entity {entity_id}")))?;
                for key in remove {
                    validate_id(key, "characteristic key")?;
                    entity.characteristics.remove(key);
                }
                entity.characteristics.extend(set.clone());
            }
            TopologyCommand::RemoveEntity { entity_id } => {
                if self
                    .relationships
                    .values()
                    .any(|edge| edge.from_entity == *entity_id || edge.to_entity == *entity_id)
                    || self
                        .custody
                        .values()
                        .any(|lease| lease.owner_entity == *entity_id)
                {
                    return Err(invalid("entity is still referenced by topology or custody"));
                }
                self.entities
                    .remove(entity_id)
                    .ok_or_else(|| invalid(format!("unknown entity {entity_id}")))?;
            }
            TopologyCommand::UpsertRelationship { relationship } => {
                validate_id(&relationship.relationship_id, "relationship_id")?;
                validate_id(&relationship.kind, "relationship kind")?;
                if !self.entities.contains_key(&relationship.from_entity)
                    || !self.entities.contains_key(&relationship.to_entity)
                {
                    return Err(invalid("relationship endpoint does not exist"));
                }
                validate_characteristics(&relationship.characteristics)?;
                self.relationships
                    .insert(relationship.relationship_id.clone(), relationship.clone());
            }
            TopologyCommand::RemoveRelationship { relationship_id } => {
                self.relationships
                    .remove(relationship_id)
                    .ok_or_else(|| invalid(format!("unknown relationship {relationship_id}")))?;
            }
            TopologyCommand::SetObligation { obligation } => {
                validate_obligation(obligation)?;
                self.obligations
                    .insert(obligation.obligation_id.clone(), obligation.clone());
            }
            TopologyCommand::RemoveObligation { obligation_id } => {
                self.obligations
                    .remove(obligation_id)
                    .ok_or_else(|| invalid(format!("unknown obligation {obligation_id}")))?;
            }
            TopologyCommand::GrantCustody { lease } => {
                validate_custody(lease, new_epoch, &self.entities)?;
                if let Some(old) = self.custody.get(&lease.replica_id) {
                    if lease.incarnation <= old.incarnation || lease.term < old.term {
                        return Err(invalid("custody grant did not fence the old incarnation"));
                    }
                }
                self.custody.insert(lease.replica_id.clone(), lease.clone());
            }
            TopologyCommand::AdvanceCustodyHwm {
                replica_id,
                term,
                lane_hwms,
            } => {
                let lease = self
                    .custody
                    .get_mut(replica_id)
                    .ok_or_else(|| invalid(format!("unknown custody replica {replica_id}")))?;
                if lease.state == CustodyState::Released || *term != lease.term {
                    return Err(invalid("stale or released custody HWM publisher"));
                }
                advance_hwms(&mut lease.retain_through_lane_hwm, lane_hwms)?;
            }
            TopologyCommand::BeginHandoff { handoff } => {
                validate_id(&handoff.handoff_id, "handoff_id")?;
                if handoff.state != HandoffState::Staged
                    || handoff.from_replica == handoff.to_replica
                    || self.handoffs.contains_key(&handoff.handoff_id)
                {
                    return Err(invalid("invalid or duplicate staged handoff"));
                }
                let from = self
                    .custody
                    .get(&handoff.from_replica)
                    .ok_or_else(|| invalid("handoff source has no custody"))?;
                let to = self
                    .custody
                    .get(&handoff.to_replica)
                    .ok_or_else(|| invalid("handoff target has no custody"))?;
                if from.group_id != handoff.group_id || to.group_id != handoff.group_id {
                    return Err(invalid("handoff crosses durability groups"));
                }
                self.handoffs
                    .insert(handoff.handoff_id.clone(), handoff.clone());
            }
            TopologyCommand::MarkHandoffCaughtUp { handoff_id } => {
                let handoff = self
                    .handoffs
                    .get_mut(handoff_id)
                    .ok_or_else(|| invalid(format!("unknown handoff {handoff_id}")))?;
                if handoff.state != HandoffState::Staged {
                    return Err(invalid("handoff is not staged"));
                }
                let target = &self.custody[&handoff.to_replica];
                if !hwms_cover(&target.retain_through_lane_hwm, &handoff.target_lane_hwm) {
                    return Err(invalid("handoff target has not reached the target HWM"));
                }
                handoff.state = HandoffState::CaughtUp;
            }
            TopologyCommand::ActivateHandoff { handoff_id } => {
                let handoff = self
                    .handoffs
                    .get_mut(handoff_id)
                    .ok_or_else(|| invalid(format!("unknown handoff {handoff_id}")))?;
                if handoff.state != HandoffState::CaughtUp {
                    return Err(invalid("handoff target is not caught up"));
                }
                self.custody
                    .get_mut(&handoff.from_replica)
                    .expect("validated source")
                    .state = CustodyState::PendingRelease;
                self.custody
                    .get_mut(&handoff.to_replica)
                    .expect("validated target")
                    .state = CustodyState::Active;
                handoff.state = HandoffState::Activated;
            }
            TopologyCommand::ActivateStagedCustody { handoff_id } => {
                let handoff = self
                    .handoffs
                    .get_mut(handoff_id)
                    .ok_or_else(|| invalid(format!("unknown handoff {handoff_id}")))?;
                if handoff.state != HandoffState::CaughtUp {
                    return Err(invalid("handoff target is not caught up"));
                }
                let target = self
                    .custody
                    .get_mut(&handoff.to_replica)
                    .expect("validated target");
                if target.state != CustodyState::Staged {
                    return Err(invalid("handoff target custody is not staged"));
                }
                target.state = CustodyState::Active;
                handoff.state = HandoffState::Activated;
            }
            TopologyCommand::PrepareCustodyRelease { replica_id } => {
                let lease = self
                    .custody
                    .get_mut(replica_id)
                    .ok_or_else(|| invalid(format!("unknown custody replica {replica_id}")))?;
                if lease.state != CustodyState::Active {
                    return Err(invalid("only active custody can be prepared for release"));
                }
                lease.state = CustodyState::PendingRelease;
            }
            TopologyCommand::ReleaseCustody {
                replica_id,
                obligation_id,
                through_lane_hwm,
            } => {
                let obligation = self
                    .obligations
                    .get(obligation_id)
                    .ok_or_else(|| invalid(format!("unknown obligation {obligation_id}")))?;
                let lease = self
                    .custody
                    .get(replica_id)
                    .ok_or_else(|| invalid(format!("unknown custody replica {replica_id}")))?;
                if lease.state != CustodyState::PendingRelease {
                    return Err(invalid("custody was not activated for release"));
                }
                let excluded = BTreeSet::from([replica_id.as_str()]);
                self.verify_coverage_excluding(obligation, through_lane_hwm, &excluded)?;
                self.custody
                    .get_mut(replica_id)
                    .expect("known custody")
                    .state = CustodyState::Released;
            }
            TopologyCommand::RetireCustody { replica_id } => {
                let lease = self
                    .custody
                    .get(replica_id)
                    .ok_or_else(|| invalid(format!("unknown custody replica {replica_id}")))?;
                if lease.state != CustodyState::Released {
                    return Err(invalid("custody must be released before retirement"));
                }
                if self.handoffs.values().any(|handoff| {
                    handoff.from_replica == *replica_id || handoff.to_replica == *replica_id
                }) {
                    return Err(invalid("custody is still referenced by a handoff"));
                }
                self.custody.remove(replica_id);
            }
            TopologyCommand::RemoveHandoff { handoff_id } => {
                let handoff = self
                    .handoffs
                    .get(handoff_id)
                    .ok_or_else(|| invalid(format!("unknown handoff {handoff_id}")))?;
                if handoff.state != HandoffState::Activated {
                    return Err(invalid("only an activated handoff can be removed"));
                }
                self.handoffs.remove(handoff_id);
            }
        }
        Ok(())
    }

    pub fn verify_coverage(
        &self,
        obligation_id: &str,
        through_lane_hwm: &BTreeMap<u32, u64>,
    ) -> io::Result<Vec<String>> {
        let obligation = self
            .obligations
            .get(obligation_id)
            .ok_or_else(|| invalid(format!("unknown obligation {obligation_id}")))?;
        self.verify_coverage_excluding(obligation, through_lane_hwm, &BTreeSet::new())
    }

    fn verify_coverage_excluding(
        &self,
        obligation: &DurabilityObligation,
        hwm: &BTreeMap<u32, u64>,
        excluded: &BTreeSet<&str>,
    ) -> io::Result<Vec<String>> {
        let candidates: Vec<(&CustodyLease, &Entity)> = self
            .custody
            .values()
            .filter(|lease| {
                lease.group_id == obligation.group_id
                    && lease.state == CustodyState::Active
                    && !excluded.contains(lease.replica_id.as_str())
                    && hwms_cover(&lease.retain_through_lane_hwm, hwm)
            })
            .filter_map(|lease| {
                self.entities
                    .get(&lease.owner_entity)
                    .filter(|node| {
                        node.characteristics
                            .get("health.available")
                            .and_then(Value::as_bool)
                            .unwrap_or(true)
                    })
                    .map(|node| (lease, node))
            })
            .collect();
        if candidates.len() < obligation.required_copies {
            return Err(invalid("insufficient durable copies at requested HWM"));
        }
        for (key, required) in &obligation.distinct {
            let values: BTreeSet<String> = candidates
                .iter()
                .filter_map(|(_, entity)| canonical_fact(entity.characteristics.get(key)))
                .collect();
            if values.len() < *required {
                return Err(invalid(format!(
                    "durability characteristic {key} has {} distinct values, needs {required}",
                    values.len()
                )));
            }
        }
        for (role, required) in &obligation.required_roles {
            let count = candidates
                .iter()
                .filter(|(_, entity)| {
                    entity
                        .characteristics
                        .get("durability.role")
                        .and_then(Value::as_str)
                        == Some(role)
                })
                .count();
            if count < *required {
                return Err(invalid(format!(
                    "durability role {role} needs {required} witnesses"
                )));
            }
        }
        Ok(candidates
            .into_iter()
            .map(|(lease, _)| lease.replica_id.clone())
            .collect())
    }
}

pub struct TopologyStore {
    changes: ChangeLogStore,
    state: Mutex<TopologyState>,
}

impl TopologyStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let changes = ChangeLogStore::open(path)?;
        let mut state = TopologyState::default();
        for envelope in changes.replay_from(TOPOLOGY_STREAM, 0) {
            let commands = decode_commands(&envelope)?;
            state.apply_batch(&CommittedTopologyBatch {
                raft_term: envelope.raft_term,
                raft_index: envelope.raft_index,
                transaction_id: envelope.batch.transaction_id.clone(),
                expected_epoch: state.topology_epoch,
                new_epoch: envelope.batch.topology_epoch,
                commands,
            })?;
            if content_hash(&state)? != envelope.batch.resulting_state_hash {
                return Err(invalid("replayed topology state hash mismatch"));
            }
        }
        Ok(Self {
            changes,
            state: Mutex::new(state),
        })
    }

    pub fn apply_committed(&self, batch: &CommittedTopologyBatch) -> io::Result<()> {
        let mut state = self.state.lock().expect("topology state mutex poisoned");
        let mut staged = state.clone();
        staged.apply_batch(batch)?;
        let schema_hash = content_hash(&"zcutils.topology.command.v1")?;
        let commands = batch
            .commands
            .iter()
            .map(|command| {
                Ok(ComponentChange {
                    component_id: TOPOLOGY_COMPONENT.into(),
                    entity_id: command.entity_key().into(),
                    operation: "topology.apply".into(),
                    schema_hash: schema_hash.clone(),
                    payload: serde_json::to_value(command)
                        .map_err(|error| invalid(format!("encode topology command: {error}")))?,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let revision = self.changes.revision(TOPOLOGY_STREAM);
        let envelope = Arc::new(CommittedChangeBatch {
            raft_term: batch.raft_term,
            raft_index: batch.raft_index,
            batch: ChangeBatch {
                schema_version: CHANGE_BATCH_SCHEMA_VERSION,
                stream_id: TOPOLOGY_STREAM.into(),
                transaction_id: batch.transaction_id.clone(),
                expected_revision: revision,
                new_revision: revision + 1,
                topology_epoch: batch.new_epoch,
                changes: commands,
                referenced_object_hashes: Vec::new(),
                resulting_state_hash: content_hash(&staged)?,
            },
        });
        self.changes.persist(&envelope)?;
        *state = staged;
        drop(state);
        self.changes.publish(envelope);
        Ok(())
    }

    pub fn state(&self) -> TopologyState {
        self.state
            .lock()
            .expect("topology state mutex poisoned")
            .clone()
    }

    pub fn subscribe(&self) -> Receiver<Arc<CommittedChangeBatch>> {
        self.changes.subscribe()
    }
}

fn decode_commands(envelope: &CommittedChangeBatch) -> io::Result<Vec<TopologyCommand>> {
    envelope
        .batch
        .changes
        .iter()
        .map(|change| {
            if change.component_id != TOPOLOGY_COMPONENT {
                return Err(invalid("unexpected component in topology stream"));
            }
            serde_json::from_value(change.payload.clone())
                .map_err(|error| invalid(format!("decode topology command: {error}")))
        })
        .collect()
}

fn validate_obligation(value: &DurabilityObligation) -> io::Result<()> {
    validate_id(&value.obligation_id, "obligation_id")?;
    validate_id(&value.group_id, "group_id")?;
    if value.required_copies == 0 {
        return Err(invalid("durability obligation requires at least one copy"));
    }
    for (key, count) in value.distinct.iter().chain(value.required_roles.iter()) {
        validate_id(key, "obligation characteristic")?;
        if *count == 0 || *count > value.required_copies {
            return Err(invalid("obligation count is outside required copy count"));
        }
    }
    Ok(())
}

fn validate_custody(
    lease: &CustodyLease,
    new_epoch: u64,
    entities: &BTreeMap<String, Entity>,
) -> io::Result<()> {
    validate_id(&lease.replica_id, "replica_id")?;
    validate_id(&lease.group_id, "group_id")?;
    validate_id(&lease.owner_entity, "owner_entity")?;
    validate_id(&lease.log_id, "log_id")?;
    if !entities.contains_key(&lease.owner_entity)
        || lease.incarnation == 0
        || lease.term == 0
        || lease.topology_epoch != new_epoch
        || lease.state == CustodyState::Released
    {
        return Err(invalid("invalid custody grant"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn entity(id: &str, role: &str, host: &str) -> Entity {
        Entity {
            entity_id: id.into(),
            kind: "userspace_stage".into(),
            characteristics: BTreeMap::from([
                ("durability.role".into(), json!(role)),
                ("failure.host".into(), json!(host)),
                ("failure.az".into(), json!("us-east-2c")),
            ]),
        }
    }

    fn lease(id: &str, owner: &str, epoch: u64, state: CustodyState) -> CustodyLease {
        CustodyLease {
            replica_id: id.into(),
            group_id: "volume-0".into(),
            owner_entity: owner.into(),
            log_id: format!("log-{id}"),
            incarnation: 1,
            term: 1,
            topology_epoch: epoch,
            state,
            retain_through_lane_hwm: BTreeMap::from([(0, 100)]),
        }
    }

    fn obligation() -> DurabilityObligation {
        DurabilityObligation {
            obligation_id: "two-host-hop-leaf".into(),
            group_id: "volume-0".into(),
            required_copies: 2,
            distinct: BTreeMap::from([("failure.host".into(), 2)]),
            required_roles: BTreeMap::from([("hop".into(), 1), ("leaf".into(), 1)]),
        }
    }

    fn initial_batch() -> CommittedTopologyBatch {
        CommittedTopologyBatch {
            raft_term: 1,
            raft_index: 1,
            transaction_id: "initial-layout".into(),
            expected_epoch: 0,
            new_epoch: 1,
            commands: vec![
                TopologyCommand::UpsertEntity {
                    entity: entity("hop-a", "hop", "host-a"),
                },
                TopologyCommand::UpsertEntity {
                    entity: entity("leaf-b", "leaf", "host-b"),
                },
                TopologyCommand::SetObligation {
                    obligation: obligation(),
                },
                TopologyCommand::GrantCustody {
                    lease: lease("hop-copy", "hop-a", 1, CustodyState::Active),
                },
                TopologyCommand::GrantCustody {
                    lease: lease("leaf-copy", "leaf-b", 1, CustodyState::Active),
                },
            ],
        }
    }

    #[test]
    fn staged_handoff_does_not_count_and_release_requires_replacement_coverage() {
        let mut state = TopologyState::default();
        state.apply_batch(&initial_batch()).unwrap();
        assert_eq!(
            state
                .verify_coverage("two-host-hop-leaf", &BTreeMap::from([(0, 100)]))
                .unwrap()
                .len(),
            2
        );
        state
            .apply_batch(&CommittedTopologyBatch {
                raft_term: 1,
                raft_index: 2,
                transaction_id: "stage-leaf-c".into(),
                expected_epoch: 1,
                new_epoch: 2,
                commands: vec![
                    TopologyCommand::UpsertEntity {
                        entity: entity("leaf-c", "leaf", "host-c"),
                    },
                    TopologyCommand::GrantCustody {
                        lease: lease("leaf-copy-2", "leaf-c", 2, CustodyState::Staged),
                    },
                    TopologyCommand::BeginHandoff {
                        handoff: Handoff {
                            handoff_id: "move-leaf".into(),
                            group_id: "volume-0".into(),
                            from_replica: "leaf-copy".into(),
                            to_replica: "leaf-copy-2".into(),
                            target_lane_hwm: BTreeMap::from([(0, 100)]),
                            state: HandoffState::Staged,
                        },
                    },
                ],
            })
            .unwrap();
        assert_eq!(state.custody["leaf-copy-2"].state, CustodyState::Staged);
        state
            .apply_batch(&CommittedTopologyBatch {
                raft_term: 1,
                raft_index: 3,
                transaction_id: "activate-and-release".into(),
                expected_epoch: 2,
                new_epoch: 3,
                commands: vec![
                    TopologyCommand::MarkHandoffCaughtUp {
                        handoff_id: "move-leaf".into(),
                    },
                    TopologyCommand::ActivateHandoff {
                        handoff_id: "move-leaf".into(),
                    },
                    TopologyCommand::ReleaseCustody {
                        replica_id: "leaf-copy".into(),
                        obligation_id: "two-host-hop-leaf".into(),
                        through_lane_hwm: BTreeMap::from([(0, 100)]),
                    },
                ],
            })
            .unwrap();
        assert_eq!(state.custody["leaf-copy"].state, CustodyState::Released);
        assert_eq!(state.custody["leaf-copy-2"].state, CustodyState::Active);
    }

    #[test]
    fn rejected_batch_is_atomic_and_persisted_batches_replay_exactly() {
        let path = temp_path("atomic-replay");
        let store = TopologyStore::open(&path).unwrap();
        store.apply_committed(&initial_batch()).unwrap();
        let expected = store.state();
        let invalid_batch = CommittedTopologyBatch {
            raft_term: 1,
            raft_index: 2,
            transaction_id: "partially-invalid".into(),
            expected_epoch: 1,
            new_epoch: 2,
            commands: vec![
                TopologyCommand::UpsertEntity {
                    entity: entity("uncommitted", "leaf", "host-c"),
                },
                TopologyCommand::RemoveEntity {
                    entity_id: "missing".into(),
                },
            ],
        };
        assert!(store.apply_committed(&invalid_batch).is_err());
        assert_eq!(store.state(), expected);
        drop(store);
        assert_eq!(TopologyStore::open(&path).unwrap().state(), expected);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn characteristics_are_patched_without_clobbering_discovered_facts() {
        let mut state = TopologyState::default();
        state.apply_batch(&initial_batch()).unwrap();
        state
            .apply_batch(&CommittedTopologyBatch {
                raft_term: 1,
                raft_index: 2,
                transaction_id: "patch-facts".into(),
                expected_epoch: 1,
                new_epoch: 2,
                commands: vec![TopologyCommand::PatchEntityCharacteristics {
                    entity_id: "hop-a".into(),
                    set: BTreeMap::from([
                        ("cloud.placement_group".into(), json!("tier-pg")),
                        ("failure.rack".into(), json!("rack-7")),
                    ]),
                    remove: BTreeSet::from(["failure.az".into()]),
                }],
            })
            .unwrap();
        let facts = &state.entities["hop-a"].characteristics;
        assert_eq!(facts["failure.host"], "host-a");
        assert_eq!(facts["failure.rack"], "rack-7");
        assert_eq!(facts["cloud.placement_group"], "tier-pg");
        assert!(!facts.contains_key("failure.az"));
    }

    fn temp_path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "zc-topology-{label}-{}-{nonce}.log",
            std::process::id()
        ))
    }
}

fn validate_characteristics(values: &Characteristics) -> io::Result<()> {
    for (key, value) in values {
        validate_id(key, "characteristic key")?;
        if value.is_null() {
            return Err(invalid("null topology characteristics are not facts"));
        }
    }
    Ok(())
}

fn advance_hwms(current: &mut BTreeMap<u32, u64>, update: &BTreeMap<u32, u64>) -> io::Result<()> {
    if update.is_empty() {
        return Err(invalid("custody HWM update contains no lanes"));
    }
    for (&lane, &hwm) in update {
        let old = current.entry(lane).or_default();
        if hwm < *old {
            return Err(invalid(format!("custody lane {lane} HWM regressed")));
        }
        *old = hwm;
    }
    Ok(())
}

fn hwms_cover(actual: &BTreeMap<u32, u64>, required: &BTreeMap<u32, u64>) -> bool {
    !required.is_empty()
        && required
            .iter()
            .all(|(lane, hwm)| actual.get(lane).is_some_and(|actual| actual >= hwm))
}

fn canonical_fact(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| serde_json::to_string(value).ok())
}

fn validate_id(value: &str, label: &str) -> io::Result<()> {
    if value.is_empty() || value.contains('\0') || value.contains('\n') {
        return Err(invalid(format!("invalid {label}")));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
