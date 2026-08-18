//! Policy-neutral orchestration helpers for online topology evolution.
//!
//! The controller translates discovered facts and high-level placement choices
//! into small, committed topology transactions.  Data movement is deliberately
//! external: a userspace RAID/tier stage copies the log, reports its durable
//! HWM, and only then may the controller activate or release custody.

use crate::topology::{
    Characteristics, CommittedTopologyBatch, CustodyLease, CustodyState, DurabilityObligation,
    Entity, Handoff, HandoffState, TopologyCommand, TopologyState, TopologyStore,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodePlacement {
    pub node_id: String,
    pub region: String,
    pub az: String,
    pub tier: String,
    pub cost_class: u64,
    pub durability_role: String,
    #[serde(default = "available")]
    pub available: bool,
}

fn available() -> bool {
    true
}

impl NodePlacement {
    pub fn entity(&self) -> Entity {
        Entity {
            entity_id: self.node_id.clone(),
            kind: "userspace_raid_stage".into(),
            characteristics: BTreeMap::from([
                ("cloud.region".into(), json!(self.region)),
                ("failure.region".into(), json!(self.region)),
                ("failure.az".into(), json!(self.az)),
                ("failure.host".into(), json!(self.node_id)),
                ("tier.class".into(), json!(self.tier)),
                ("tier.cost_class".into(), json!(self.cost_class)),
                ("durability.role".into(), json!(self.durability_role)),
                ("health.available".into(), json!(self.available)),
            ]),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaPlacement {
    pub replica_id: String,
    pub node: NodePlacement,
    pub group_id: String,
    pub log_id: String,
}

/// A single-writer controller. In production its `commit` calls are the
/// entries proposed to the metadata Raft group; the store is the committed
/// application/replay side of that boundary.
pub struct EvolutionController {
    store: TopologyStore,
    raft_term: u64,
}

impl EvolutionController {
    pub fn open(path: impl AsRef<Path>, raft_term: u64) -> io::Result<Self> {
        if raft_term == 0 {
            return Err(invalid("controller Raft term must be nonzero"));
        }
        Ok(Self {
            store: TopologyStore::open(path)?,
            raft_term,
        })
    }

    pub fn state(&self) -> TopologyState {
        self.store.state()
    }

    pub fn commit(
        &self,
        transaction_id: impl Into<String>,
        commands: Vec<TopologyCommand>,
    ) -> io::Result<()> {
        let state = self.store.state();
        self.store.apply_committed(&CommittedTopologyBatch {
            raft_term: self.raft_term.max(state.applied_term),
            raft_index: state.applied_index.saturating_add(1),
            transaction_id: transaction_id.into(),
            expected_epoch: state.topology_epoch,
            new_epoch: state.topology_epoch.saturating_add(1),
            commands,
        })
    }

    pub fn bootstrap(
        &self,
        transaction_id: &str,
        obligation: DurabilityObligation,
        replicas: &[(ReplicaPlacement, BTreeMap<u32, u64>)],
    ) -> io::Result<()> {
        let new_epoch = self.state().topology_epoch.saturating_add(1);
        let mut commands = vec![TopologyCommand::SetObligation { obligation }];
        for (placement, hwm) in replicas {
            commands.push(TopologyCommand::UpsertEntity {
                entity: placement.node.entity(),
            });
            commands.push(TopologyCommand::GrantCustody {
                lease: lease(placement, new_epoch, CustodyState::Active, hwm.clone()),
            });
        }
        self.commit(transaction_id, commands)
    }

    /// Phase one of growth/replacement. The target is not a witness yet.
    pub fn stage_replica(
        &self,
        transaction_id: &str,
        source_replica: &str,
        target: &ReplicaPlacement,
        target_hwm: BTreeMap<u32, u64>,
    ) -> io::Result<String> {
        let state = self.state();
        let source = state
            .custody
            .get(source_replica)
            .ok_or_else(|| invalid(format!("unknown source replica {source_replica}")))?;
        if source.state != CustodyState::Active || source.group_id != target.group_id {
            return Err(invalid("handoff source is not active in the target group"));
        }
        let handoff_id = format!("handoff-{}-to-{}", source_replica, target.replica_id);
        let epoch = state.topology_epoch.saturating_add(1);
        self.commit(
            transaction_id,
            vec![
                TopologyCommand::UpsertEntity {
                    entity: target.node.entity(),
                },
                TopologyCommand::GrantCustody {
                    lease: lease(target, epoch, CustodyState::Staged, BTreeMap::new()),
                },
                TopologyCommand::BeginHandoff {
                    handoff: Handoff {
                        handoff_id: handoff_id.clone(),
                        group_id: target.group_id.clone(),
                        from_replica: source_replica.into(),
                        to_replica: target.replica_id.clone(),
                        target_lane_hwm: target_hwm,
                        state: HandoffState::Staged,
                    },
                },
            ],
        )?;
        Ok(handoff_id)
    }

    /// Commit a data-plane durable-HWM acknowledgement and make the copied
    /// replica a witness without demoting its source.
    pub fn activate_copied_replica(
        &self,
        transaction_id: &str,
        handoff_id: &str,
        durable_hwm: BTreeMap<u32, u64>,
    ) -> io::Result<()> {
        let state = self.state();
        let handoff = state
            .handoffs
            .get(handoff_id)
            .ok_or_else(|| invalid(format!("unknown handoff {handoff_id}")))?;
        let target = &state.custody[&handoff.to_replica];
        self.commit(
            transaction_id,
            vec![
                TopologyCommand::AdvanceCustodyHwm {
                    replica_id: target.replica_id.clone(),
                    term: target.term,
                    lane_hwms: durable_hwm,
                },
                TopologyCommand::MarkHandoffCaughtUp {
                    handoff_id: handoff_id.into(),
                },
                TopologyCommand::ActivateStagedCustody {
                    handoff_id: handoff_id.into(),
                },
            ],
        )
    }

    /// Commit the cutover side of a live move. The staged destination becomes
    /// active at its durable HWM and the source becomes pending release in the
    /// same topology transaction. Data copy and the short userspace route
    /// fence must complete before this command is proposed.
    pub fn activate_moved_replica(
        &self,
        transaction_id: &str,
        handoff_id: &str,
        durable_hwm: BTreeMap<u32, u64>,
    ) -> io::Result<()> {
        let state = self.state();
        let handoff = state
            .handoffs
            .get(handoff_id)
            .ok_or_else(|| invalid(format!("unknown handoff {handoff_id}")))?;
        let target = &state.custody[&handoff.to_replica];
        self.commit(
            transaction_id,
            vec![
                TopologyCommand::AdvanceCustodyHwm {
                    replica_id: target.replica_id.clone(),
                    term: target.term,
                    lane_hwms: durable_hwm,
                },
                TopologyCommand::MarkHandoffCaughtUp {
                    handoff_id: handoff_id.into(),
                },
                TopologyCommand::ActivateHandoff {
                    handoff_id: handoff_id.into(),
                },
            ],
        )
    }

    /// Release only after the state machine proves the remaining live
    /// witnesses meet the obligation at `through_hwm`.
    pub fn release_replica(
        &self,
        transaction_id: &str,
        replica_id: &str,
        obligation_id: &str,
        through_hwm: BTreeMap<u32, u64>,
    ) -> io::Result<()> {
        let state = self.state();
        let lease = state
            .custody
            .get(replica_id)
            .ok_or_else(|| invalid(format!("unknown replica {replica_id}")))?;
        let mut commands = Vec::new();
        match lease.state {
            CustodyState::Active => commands.push(TopologyCommand::PrepareCustodyRelease {
                replica_id: replica_id.into(),
            }),
            CustodyState::PendingRelease => {}
            _ => return Err(invalid("replica is not eligible for custody release")),
        }
        commands.push(TopologyCommand::ReleaseCustody {
            replica_id: replica_id.into(),
            obligation_id: obligation_id.into(),
            through_lane_hwm: through_hwm,
        });
        self.commit(transaction_id, commands)
    }

    pub fn retire_released_replica(
        &self,
        transaction_id: &str,
        replica_id: &str,
    ) -> io::Result<()> {
        let state = self.state();
        let owner = state
            .custody
            .get(replica_id)
            .ok_or_else(|| invalid(format!("unknown replica {replica_id}")))?
            .owner_entity
            .clone();
        let mut commands = Vec::new();
        for (id, handoff) in &state.handoffs {
            if handoff.from_replica == replica_id || handoff.to_replica == replica_id {
                commands.push(TopologyCommand::RemoveHandoff {
                    handoff_id: id.clone(),
                });
            }
        }
        commands.push(TopologyCommand::RetireCustody {
            replica_id: replica_id.into(),
        });
        for (id, relationship) in &state.relationships {
            if relationship.from_entity == owner || relationship.to_entity == owner {
                commands.push(TopologyCommand::RemoveRelationship {
                    relationship_id: id.clone(),
                });
            }
        }
        commands.push(TopologyCommand::RemoveEntity { entity_id: owner });
        self.commit(transaction_id, commands)
    }

    pub fn set_available(
        &self,
        transaction_id: &str,
        node_id: &str,
        available: bool,
    ) -> io::Result<()> {
        self.patch_facts(
            transaction_id,
            node_id,
            BTreeMap::from([("health.available".into(), json!(available))]),
        )
    }

    pub fn set_tier(
        &self,
        transaction_id: &str,
        node_id: &str,
        tier: &str,
        cost_class: u64,
    ) -> io::Result<()> {
        self.patch_facts(
            transaction_id,
            node_id,
            BTreeMap::from([
                ("tier.class".into(), json!(tier)),
                ("tier.cost_class".into(), json!(cost_class)),
            ]),
        )
    }

    fn patch_facts(
        &self,
        transaction_id: &str,
        node_id: &str,
        set: Characteristics,
    ) -> io::Result<()> {
        self.commit(
            transaction_id,
            vec![TopologyCommand::PatchEntityCharacteristics {
                entity_id: node_id.into(),
                set,
                remove: BTreeSet::new(),
            }],
        )
    }
}

fn lease(
    placement: &ReplicaPlacement,
    topology_epoch: u64,
    state: CustodyState,
    hwm: BTreeMap<u32, u64>,
) -> CustodyLease {
    CustodyLease {
        replica_id: placement.replica_id.clone(),
        group_id: placement.group_id.clone(),
        owner_entity: placement.node.node_id.clone(),
        log_id: placement.log_id.clone(),
        incarnation: 1,
        term: 1,
        topology_epoch,
        state,
        retain_through_lane_hwm: hwm,
    }
}

pub fn region_obligation(
    id: &str,
    group: &str,
    copies: usize,
    regions: usize,
) -> DurabilityObligation {
    DurabilityObligation {
        obligation_id: id.into(),
        group_id: group.into(),
        required_copies: copies,
        distinct: BTreeMap::from([("failure.region".into(), regions)]),
        required_roles: BTreeMap::new(),
    }
}

pub fn fact_str<'a>(state: &'a TopologyState, node_id: &str, key: &str) -> Option<&'a str> {
    state
        .entities
        .get(node_id)?
        .characteristics
        .get(key)?
        .as_str()
}

pub fn fact_u64(state: &TopologyState, node_id: &str, key: &str) -> Option<u64> {
    state
        .entities
        .get(node_id)?
        .characteristics
        .get(key)?
        .as_u64()
}

pub fn fact_bool(state: &TopologyState, node_id: &str, key: &str) -> Option<bool> {
    state
        .entities
        .get(node_id)?
        .characteristics
        .get(key)?
        .as_bool()
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn placement(
        replica: &str,
        node: &str,
        region: &str,
        tier: &str,
        cost: u64,
    ) -> ReplicaPlacement {
        ReplicaPlacement {
            replica_id: replica.into(),
            node: NodePlacement {
                node_id: node.into(),
                region: region.into(),
                az: format!("{region}-a"),
                tier: tier.into(),
                cost_class: cost,
                durability_role: "leaf".into(),
                available: true,
            },
            group_id: "volume-0".into(),
            log_id: "log-0".into(),
        }
    }

    #[test]
    fn grow_promote_replace_collapse_and_replay() {
        let path = temp_path("evolution");
        let hwm = BTreeMap::from([(0, 900), (1, 850)]);
        let controller = EvolutionController::open(&path, 7).unwrap();
        let a = placement("rep-a", "node-a", "region-a", "hot", 9);
        let b = placement("rep-b", "node-b", "region-b", "cold", 1);
        controller
            .bootstrap(
                "bootstrap",
                region_obligation("cross-region", "volume-0", 2, 2),
                &[(a.clone(), hwm.clone()), (b.clone(), hwm.clone())],
            )
            .unwrap();

        controller
            .set_available("region-a-failed", "node-a", false)
            .unwrap();
        assert!(
            controller
                .state()
                .verify_coverage("cross-region", &hwm)
                .is_err()
        );

        let c = placement("rep-c", "node-c", "region-c", "cold", 1);
        let handoff = controller
            .stage_replica("stage-region-c", "rep-b", &c, hwm.clone())
            .unwrap();
        assert!(
            controller
                .state()
                .verify_coverage("cross-region", &hwm)
                .is_err()
        );
        controller
            .activate_copied_replica("activate-region-c", &handoff, hwm.clone())
            .unwrap();
        controller
            .state()
            .verify_coverage("cross-region", &hwm)
            .unwrap();

        controller
            .set_tier("promote-b", "node-b", "hot", 9)
            .unwrap();
        controller
            .set_tier("promote-c", "node-c", "warm", 4)
            .unwrap();
        assert_eq!(
            fact_str(&controller.state(), "node-b", "tier.class"),
            Some("hot")
        );

        let d = placement("rep-d", "node-d", "region-b", "warm", 3);
        let replacement = controller
            .stage_replica("stage-replacement", "rep-b", &d, hwm.clone())
            .unwrap();
        controller
            .activate_copied_replica("activate-replacement", &replacement, hwm.clone())
            .unwrap();
        controller
            .release_replica("release-expensive-b", "rep-b", "cross-region", hwm.clone())
            .unwrap();
        controller
            .retire_released_replica("retire-expensive-b", "rep-b")
            .unwrap();
        assert!(!controller.state().entities.contains_key("node-b"));
        controller
            .state()
            .verify_coverage("cross-region", &hwm)
            .unwrap();

        let expected = controller.state();
        drop(controller);
        let replayed = EvolutionController::open(&path, 8).unwrap().state();
        assert_eq!(replayed, expected);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn cannot_contract_before_replacement_is_live() {
        let path = temp_path("fail-closed");
        let hwm = BTreeMap::from([(0, 100)]);
        let controller = EvolutionController::open(&path, 1).unwrap();
        let a = placement("rep-a", "node-a", "region-a", "hot", 8);
        let b = placement("rep-b", "node-b", "region-b", "cold", 1);
        controller
            .bootstrap(
                "bootstrap",
                region_obligation("cross-region", "volume-0", 2, 2),
                &[(a, hwm.clone()), (b.clone(), hwm.clone())],
            )
            .unwrap();
        let c = placement("rep-c", "node-c", "region-c", "cold", 1);
        controller
            .stage_replica("stage-c", "rep-b", &c, hwm.clone())
            .unwrap();
        let before = controller.state();
        assert!(
            controller
                .release_replica("unsafe-release", "rep-a", "cross-region", hwm)
                .is_err()
        );
        assert_eq!(controller.state(), before);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn live_move_activates_target_and_releases_pending_source() {
        let path = temp_path("live-move");
        let hwm = BTreeMap::from([(0, 400), (1, 380)]);
        let controller = EvolutionController::open(&path, 3).unwrap();
        let a = placement("rep-a", "node-a", "region-a", "hot", 8);
        let b = placement("rep-b", "node-b", "region-b", "hot", 8);
        controller
            .bootstrap(
                "bootstrap",
                region_obligation("cross-region", "volume-0", 2, 2),
                &[(a, hwm.clone()), (b, hwm.clone())],
            )
            .unwrap();
        let c = placement("rep-c", "node-c", "region-c", "warm", 3);
        let handoff = controller
            .stage_replica("stage-move", "rep-b", &c, hwm.clone())
            .unwrap();
        controller
            .activate_moved_replica("cutover-move", &handoff, hwm.clone())
            .unwrap();
        let state = controller.state();
        assert_eq!(state.custody["rep-b"].state, CustodyState::PendingRelease);
        assert_eq!(state.custody["rep-c"].state, CustodyState::Active);
        controller
            .release_replica("release-source", "rep-b", "cross-region", hwm.clone())
            .unwrap();
        assert_eq!(
            controller.state().custody["rep-b"].state,
            CustodyState::Released
        );
        controller
            .state()
            .verify_coverage("cross-region", &hwm)
            .unwrap();
        fs::remove_file(path).unwrap();
    }

    fn temp_path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "zc-controller-{label}-{}-{nonce}.log",
            std::process::id()
        ))
    }
}
