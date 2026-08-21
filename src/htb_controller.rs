//! Off-path hierarchical borrowing over lane-local IOPS limiters.
//!
//! The controller consumes interval demand snapshots, computes a complete grant
//! generation, and publishes it to cache-line-isolated lane mailboxes. It never
//! participates in descriptor admission or completion retirement.

use crate::iops_policy::{LaneBudgetMailbox, LaneBudgetSnapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HtbClassSpec {
    pub id: String,
    pub parent_id: Option<String>,
    /// Capacity protected from sibling borrowers when this subtree has demand.
    pub guaranteed_iops: u64,
    /// Absolute maximum after borrowing.
    pub ceiling_iops: u64,
    /// Relative share of contested spare capacity.
    pub borrow_weight: u32,
    pub burst_seconds: u32,
    /// Nonzero only for schedulable leaves.
    pub lanes: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HtbPolicy {
    pub revision: u64,
    pub root_id: String,
    pub classes: Vec<HtbClassSpec>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HtbDemandSnapshot {
    pub interval_start_ns: u64,
    pub interval_end_ns: u64,
    /// Demand includes completed work plus work that remained queued or was
    /// throttled during the interval. Missing leaves are treated as idle.
    pub demand_iops: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HtbClassGrant {
    pub class_id: String,
    pub target_iops: u64,
    pub guaranteed_iops: u64,
    pub borrowed_iops: u64,
    pub ceiling_iops: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HtbGrantPlan {
    pub generation: u64,
    pub policy_revision: u64,
    pub observed_interval_end_ns: u64,
    pub effective_ns: u64,
    pub grants: Vec<HtbClassGrant>,
}

#[derive(Clone, Debug)]
struct ClassNode {
    spec: HtbClassSpec,
    children: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct HtbBorrowingController {
    policy: HtbPolicy,
    nodes: BTreeMap<String, ClassNode>,
}

impl HtbBorrowingController {
    pub fn new(policy: HtbPolicy) -> io::Result<Self> {
        if policy.revision == 0 || policy.root_id.is_empty() {
            return Err(invalid("HTB policy revision and root id must be nonzero"));
        }
        let mut nodes = BTreeMap::new();
        for spec in &policy.classes {
            if spec.id.is_empty() || spec.ceiling_iops < spec.guaranteed_iops {
                return Err(invalid(format!("invalid HTB class {}", spec.id)));
            }
            if spec.borrow_weight == 0 {
                return Err(invalid(format!(
                    "HTB class {} has zero borrow weight",
                    spec.id
                )));
            }
            if nodes
                .insert(
                    spec.id.clone(),
                    ClassNode {
                        spec: spec.clone(),
                        children: Vec::new(),
                    },
                )
                .is_some()
            {
                return Err(invalid(format!("duplicate HTB class {}", spec.id)));
            }
        }
        if !nodes.contains_key(&policy.root_id) {
            return Err(invalid("HTB root does not exist"));
        }
        if nodes[&policy.root_id].spec.parent_id.is_some() {
            return Err(invalid("HTB root must not have a parent"));
        }
        let ids = nodes.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            if id == policy.root_id {
                continue;
            }
            let parent_id = nodes[&id]
                .spec
                .parent_id
                .as_ref()
                .ok_or_else(|| invalid(format!("HTB class {id} is disconnected")))?
                .clone();
            let parent = nodes
                .get_mut(&parent_id)
                .ok_or_else(|| invalid(format!("HTB class {id} has unknown parent {parent_id}")))?;
            parent.children.push(id);
        }
        for node in nodes.values_mut() {
            node.children.sort();
            if node.children.is_empty() != (node.spec.lanes != 0) {
                return Err(invalid(format!(
                    "HTB class {} must be either an internal node or a schedulable leaf",
                    node.spec.id
                )));
            }
        }
        let protected_by_parent = nodes
            .iter()
            .map(|(id, node)| {
                let protected = node.children.iter().try_fold(0u64, |sum, child| {
                    sum.checked_add(nodes[child].spec.guaranteed_iops)
                        .ok_or_else(|| invalid("HTB guarantee sum overflow"))
                })?;
                Ok((id.clone(), protected))
            })
            .collect::<io::Result<BTreeMap<_, _>>>()?;
        for node in nodes.values() {
            let protected = protected_by_parent[&node.spec.id];
            if protected > node.spec.guaranteed_iops {
                return Err(invalid(format!(
                    "HTB children of {} protect {protected} IOPS above parent guarantee {}",
                    node.spec.id, node.spec.guaranteed_iops
                )));
            }
        }
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        visit_tree(&policy.root_id, &nodes, &mut visiting, &mut visited)?;
        if visited.len() != nodes.len() {
            return Err(invalid("HTB policy contains disconnected classes"));
        }
        Ok(Self { policy, nodes })
    }

    pub fn policy(&self) -> &HtbPolicy {
        &self.policy
    }

    /// Compute a deterministic generation. Call this from a controller task,
    /// never from a lane worker.
    pub fn compute_grants(
        &self,
        generation: u64,
        effective_ns: u64,
        demand: &HtbDemandSnapshot,
    ) -> io::Result<HtbGrantPlan> {
        if generation == 0 || demand.interval_end_ns < demand.interval_start_ns {
            return Err(invalid("invalid HTB grant generation or metric interval"));
        }
        let mut subtree_demand = BTreeMap::new();
        self.subtree_demand(&self.policy.root_id, demand, &mut subtree_demand)?;
        let root = &self.nodes[&self.policy.root_id].spec;
        let root_target = subtree_demand[&self.policy.root_id].min(root.ceiling_iops);
        let mut targets = BTreeMap::new();
        targets.insert(self.policy.root_id.clone(), root_target);
        self.allocate_children(
            &self.policy.root_id,
            root_target,
            &subtree_demand,
            &mut targets,
        )?;

        let grants = self
            .nodes
            .values()
            .filter(|node| node.children.is_empty())
            .map(|node| {
                let target = targets.get(&node.spec.id).copied().unwrap_or(0);
                HtbClassGrant {
                    class_id: node.spec.id.clone(),
                    target_iops: target,
                    guaranteed_iops: node.spec.guaranteed_iops,
                    borrowed_iops: target.saturating_sub(node.spec.guaranteed_iops),
                    ceiling_iops: node.spec.ceiling_iops,
                }
            })
            .collect();
        Ok(HtbGrantPlan {
            generation,
            policy_revision: self.policy.revision,
            observed_interval_end_ns: demand.interval_end_ns,
            effective_ns,
            grants,
        })
    }

    fn subtree_demand(
        &self,
        id: &str,
        demand: &HtbDemandSnapshot,
        output: &mut BTreeMap<String, u64>,
    ) -> io::Result<u64> {
        let node = &self.nodes[id];
        let total = if node.children.is_empty() {
            demand.demand_iops.get(id).copied().unwrap_or(0)
        } else {
            node.children.iter().try_fold(0u64, |sum, child| {
                let child_demand = self.subtree_demand(child, demand, output)?;
                sum.checked_add(child_demand)
                    .ok_or_else(|| invalid("HTB subtree demand overflow"))
            })?
        }
        .min(node.spec.ceiling_iops);
        output.insert(id.to_string(), total);
        Ok(total)
    }

    fn allocate_children(
        &self,
        parent_id: &str,
        parent_target: u64,
        demand: &BTreeMap<String, u64>,
        targets: &mut BTreeMap<String, u64>,
    ) -> io::Result<()> {
        let children = &self.nodes[parent_id].children;
        if children.is_empty() {
            return Ok(());
        }
        let mut remaining = parent_target;
        for child_id in children {
            let child = &self.nodes[child_id].spec;
            let floor = demand[child_id].min(child.guaranteed_iops);
            targets.insert(child_id.clone(), floor);
            remaining = remaining.checked_sub(floor).ok_or_else(|| {
                invalid(format!("HTB protected demand exceeds grant at {parent_id}"))
            })?;
        }
        weighted_borrow(children, remaining, &self.nodes, demand, targets)?;
        for child_id in children {
            self.allocate_children(child_id, targets[child_id], demand, targets)?;
        }
        Ok(())
    }
}

fn weighted_borrow(
    children: &[String],
    mut remaining: u64,
    nodes: &BTreeMap<String, ClassNode>,
    demand: &BTreeMap<String, u64>,
    targets: &mut BTreeMap<String, u64>,
) -> io::Result<()> {
    let mut active = children
        .iter()
        .filter(|id| targets[*id] < demand[*id].min(nodes[*id].spec.ceiling_iops))
        .cloned()
        .collect::<Vec<_>>();
    while remaining != 0 && !active.is_empty() {
        let total_weight = active.iter().try_fold(0u64, |sum, id| {
            sum.checked_add(u64::from(nodes[id].spec.borrow_weight))
                .ok_or_else(|| invalid("HTB borrow weight overflow"))
        })?;
        let before = remaining;
        for id in &active {
            if remaining == 0 {
                break;
            }
            let limit = demand[id].min(nodes[id].spec.ceiling_iops);
            let headroom = limit.saturating_sub(targets[id]);
            let weighted = ((u128::from(before) * u128::from(nodes[id].spec.borrow_weight))
                / u128::from(total_weight)) as u64;
            let grant = headroom.min(weighted.max(1)).min(remaining);
            *targets.get_mut(id).expect("active HTB target exists") += grant;
            remaining -= grant;
        }
        active.retain(|id| targets[id] < demand[id].min(nodes[id].spec.ceiling_iops));
    }
    Ok(())
}

fn visit_tree(
    id: &str,
    nodes: &BTreeMap<String, ClassNode>,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
) -> io::Result<()> {
    if !visiting.insert(id.to_string()) {
        return Err(invalid("HTB policy contains a cycle"));
    }
    for child in &nodes[id].children {
        visit_tree(child, nodes, visiting, visited)?;
    }
    visiting.remove(id);
    visited.insert(id.to_string());
    Ok(())
}

/// Connects a computed leaf grant to already-running lane limiters. Publication
/// performs a bounded number of seqlock writes in the controller thread.
pub struct HtbGrantPublisher {
    mailboxes: BTreeMap<String, Vec<Arc<LaneBudgetMailbox>>>,
    quantum_ops: u64,
    metric_publish_ns: u64,
}

impl HtbGrantPublisher {
    pub fn new(quantum_ops: u64, metric_publish_ns: u64) -> io::Result<Self> {
        if quantum_ops == 0 || metric_publish_ns == 0 {
            return Err(invalid("HTB quantum and metric interval must be nonzero"));
        }
        Ok(Self {
            mailboxes: BTreeMap::new(),
            quantum_ops,
            metric_publish_ns,
        })
    }

    pub fn register_leaf(
        &mut self,
        class_id: impl Into<String>,
        mailboxes: Vec<Arc<LaneBudgetMailbox>>,
    ) -> io::Result<()> {
        let class_id = class_id.into();
        if class_id.is_empty() || mailboxes.is_empty() || self.mailboxes.contains_key(&class_id) {
            return Err(invalid("invalid or duplicate HTB mailbox registration"));
        }
        self.mailboxes.insert(class_id, mailboxes);
        Ok(())
    }

    pub fn publish(
        &self,
        controller: &HtbBorrowingController,
        plan: &HtbGrantPlan,
    ) -> io::Result<()> {
        self.publish_with_lease(controller, plan, 0)
    }

    /// Publish borrowed capacity as a renewable local-monotonic lease. If the
    /// regional leader or quorum disappears, every lane independently falls
    /// back to its protected guarantee after `valid_until_ns`.
    pub fn publish_with_lease(
        &self,
        controller: &HtbBorrowingController,
        plan: &HtbGrantPlan,
        valid_until_ns: u64,
    ) -> io::Result<()> {
        if valid_until_ns != 0 && valid_until_ns <= plan.effective_ns {
            return Err(invalid("HTB lease must expire after its effective time"));
        }
        if plan.policy_revision != controller.policy.revision {
            return Err(invalid("HTB plan policy revision mismatch"));
        }
        for grant in &plan.grants {
            let node = controller
                .nodes
                .get(&grant.class_id)
                .ok_or_else(|| invalid("HTB plan references unknown class"))?;
            let mailboxes = self
                .mailboxes
                .get(&grant.class_id)
                .ok_or_else(|| invalid(format!("HTB leaf {} has no mailboxes", grant.class_id)))?;
            if mailboxes.len() != usize::from(node.spec.lanes) {
                return Err(invalid(format!(
                    "HTB leaf {} lane count mismatch",
                    grant.class_id
                )));
            }
            for (lane, mailbox) in mailboxes.iter().enumerate() {
                let sustained = split_rate(grant.target_iops, mailboxes.len(), lane);
                let peak = split_rate(grant.ceiling_iops, mailboxes.len(), lane).max(sustained);
                let burst_ops = self.quantum_ops.saturating_add(
                    peak.saturating_sub(sustained)
                        .saturating_mul(u64::from(node.spec.burst_seconds)),
                );
                mailbox.publish(LaneBudgetSnapshot {
                    generation: plan.generation,
                    sustained_iops: sustained,
                    peak_iops: peak,
                    burst_ops,
                    quantum_ops: self.quantum_ops,
                    metric_publish_ns: self.metric_publish_ns,
                    effective_ns: plan.effective_ns,
                    fallback_sustained_iops: split_rate(
                        grant.guaranteed_iops,
                        mailboxes.len(),
                        lane,
                    ),
                    fallback_peak_iops: split_rate(grant.guaranteed_iops, mailboxes.len(), lane),
                    valid_until_ns,
                });
            }
        }
        Ok(())
    }
}

fn split_rate(total: u64, lanes: usize, lane: usize) -> u64 {
    total / lanes as u64 + u64::from((lane as u64) < total % lanes as u64)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iops_policy::LaneLimiter;

    fn controller() -> HtbBorrowingController {
        HtbBorrowingController::new(HtbPolicy {
            revision: 7,
            root_id: "region".into(),
            classes: vec![
                HtbClassSpec {
                    id: "region".into(),
                    parent_id: None,
                    guaranteed_iops: 12_000_000,
                    ceiling_iops: 12_000_000,
                    borrow_weight: 1,
                    burst_seconds: 0,
                    lanes: 0,
                },
                HtbClassSpec {
                    id: "foreground".into(),
                    parent_id: Some("region".into()),
                    guaranteed_iops: 8_000_000,
                    ceiling_iops: 12_000_000,
                    borrow_weight: 9,
                    burst_seconds: 2,
                    lanes: 2,
                },
                HtbClassSpec {
                    id: "snapshot".into(),
                    parent_id: Some("region".into()),
                    guaranteed_iops: 4_000_000,
                    ceiling_iops: 12_000_000,
                    borrow_weight: 1,
                    burst_seconds: 1,
                    lanes: 2,
                },
            ],
        })
        .unwrap()
    }

    #[test]
    fn idle_sibling_lends_all_capacity_up_to_borrower_ceiling() {
        let controller = controller();
        let plan = controller
            .compute_grants(
                9,
                2_000,
                &HtbDemandSnapshot {
                    interval_start_ns: 0,
                    interval_end_ns: 1_000,
                    demand_iops: BTreeMap::from([
                        ("foreground".into(), 20_000_000),
                        ("snapshot".into(), 0),
                    ]),
                },
            )
            .unwrap();
        assert_eq!(plan.grants[0].target_iops, 12_000_000);
        assert_eq!(plan.grants[0].borrowed_iops, 4_000_000);
        assert_eq!(plan.grants[1].target_iops, 0);
    }

    #[test]
    fn active_guarantees_are_protected_before_weighted_borrowing() {
        let controller = controller();
        let plan = controller
            .compute_grants(
                10,
                2_000,
                &HtbDemandSnapshot {
                    interval_start_ns: 0,
                    interval_end_ns: 1_000,
                    demand_iops: BTreeMap::from([
                        ("foreground".into(), 20_000_000),
                        ("snapshot".into(), 20_000_000),
                    ]),
                },
            )
            .unwrap();
        assert_eq!(plan.grants[0].target_iops, 8_000_000);
        assert_eq!(plan.grants[1].target_iops, 4_000_000);
    }

    #[test]
    fn one_effective_time_fences_all_lane_mailboxes() {
        let controller = controller();
        let initial = LaneBudgetSnapshot {
            generation: 1,
            sustained_iops: 1,
            peak_iops: 1,
            burst_ops: 1,
            quantum_ops: 1,
            metric_publish_ns: 100,
            effective_ns: 0,
            fallback_sustained_iops: 1,
            fallback_peak_iops: 1,
            valid_until_ns: 0,
        };
        let foreground = vec![
            Arc::new(LaneBudgetMailbox::new(initial)),
            Arc::new(LaneBudgetMailbox::new(initial)),
        ];
        let snapshot = vec![
            Arc::new(LaneBudgetMailbox::new(initial)),
            Arc::new(LaneBudgetMailbox::new(initial)),
        ];
        let mut publisher = HtbGrantPublisher::new(256, 100_000_000).unwrap();
        publisher
            .register_leaf("foreground", foreground.clone())
            .unwrap();
        publisher.register_leaf("snapshot", snapshot).unwrap();
        let plan = controller
            .compute_grants(
                2,
                5_000,
                &HtbDemandSnapshot {
                    interval_start_ns: 0,
                    interval_end_ns: 1_000,
                    demand_iops: BTreeMap::from([("foreground".into(), 12_000_000)]),
                },
            )
            .unwrap();
        publisher.publish(&controller, &plan).unwrap();
        let mut limiter = LaneLimiter::new(0, initial);
        assert!(!limiter.refresh(4_999, &foreground[0]));
        assert!(limiter.refresh(5_000, &foreground[0]));
        assert_eq!(limiter.budget().generation, 2);
        assert_eq!(limiter.budget().sustained_iops, 6_000_000);
    }
}
