//! Bridge consensus authority to node-local HTB grant leases.
//!
//! Raft and wall-clock validation happen in the regional controller task. Lane
//! workers receive only a local-monotonic expiry in their existing mailboxes.

use crate::ha_metadata::PublishedGroupView;
use crate::htb_controller::{HtbBorrowingController, HtbGrantPlan, HtbGrantPublisher};
use std::io;

pub struct RegionalHtbLeaseBridge;

impl RegionalHtbLeaseBridge {
    #[allow(clippy::too_many_arguments)]
    pub fn publish_from_raft_authority(
        publisher: &HtbGrantPublisher,
        controller: &HtbBorrowingController,
        plan: &HtbGrantPlan,
        authority: &PublishedGroupView,
        leader_id: &str,
        term: u64,
        config_epoch: u64,
        now_unix_ns: u64,
        now_monotonic_ns: u64,
        activation_delay_ns: u64,
        max_grant_lease_ns: u64,
    ) -> io::Result<HtbGrantPlan> {
        if max_grant_lease_ns == 0
            || !authority.authorizes(leader_id, term, config_epoch, now_unix_ns, 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "regional HTB publication lacks current Raft lease authority",
            ));
        }
        let raft_view = authority.load();
        let raft_remaining_ns = raft_view
            .lease_expires_unix_nanos
            .saturating_sub(now_unix_ns);
        let effective_ns = now_monotonic_ns.saturating_add(activation_delay_ns);
        let valid_until_ns =
            now_monotonic_ns.saturating_add(raft_remaining_ns.min(max_grant_lease_ns));
        if valid_until_ns <= effective_ns {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "regional HTB Raft lease is too short for coherent activation",
            ));
        }

        // A persisted plan never carries a foreign host's monotonic clock into
        // lane state. Each authorized publisher rebases activation locally.
        let mut local_plan = plan.clone();
        local_plan.effective_ns = effective_ns;
        publisher.publish_with_lease(controller, &local_plan, valid_until_ns)?;
        Ok(local_plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha_metadata::{
        CommittedHaBatch, DataReplica, DataReplicaRole, DurabilityPolicy, GroupConfig, HaCommand,
        HaMetadataStore,
    };
    use crate::htb_controller::{HtbClassSpec, HtbDemandSnapshot, HtbPolicy};
    use crate::iops_policy::{LaneBudgetMailbox, LaneBudgetSnapshot, LaneLimiter};
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("zc-regional-htb-{nonce}.log"))
    }

    #[test]
    fn raft_authority_is_rebased_to_a_local_expiring_grant() {
        let path = temp_path();
        let store = HaMetadataStore::open(&path).unwrap();
        let config = GroupConfig {
            group_id: "regional-control".into(),
            volume_id: "control".into(),
            log_id: "regional-control-log".into(),
            config_epoch: 1,
            placement_epoch: 1,
            voters: vec!["a".into(), "b".into(), "c".into()],
            data_replicas: vec![
                DataReplica {
                    replica_id: "a".into(),
                    role: DataReplicaRole::Hop,
                    failure_domain: "az-a".into(),
                },
                DataReplica {
                    replica_id: "b".into(),
                    role: DataReplicaRole::Leaf,
                    failure_domain: "az-b".into(),
                },
                DataReplica {
                    replica_id: "c".into(),
                    role: DataReplicaRole::Leaf,
                    failure_domain: "az-c".into(),
                },
            ],
            durability: DurabilityPolicy {
                required_distinct_failure_domains: 2,
                required_hop_witnesses: 1,
                required_leaf_witnesses: 1,
            },
        };
        store
            .apply_committed_batch(&CommittedHaBatch {
                index: 1,
                term: 4,
                transaction_id: "configure-authority".into(),
                commands: vec![
                    HaCommand::ConfigureGroup { config },
                    HaCommand::GrantLease {
                        group_id: "regional-control".into(),
                        leader_id: "a".into(),
                        term: 4,
                        config_epoch: 1,
                        issued_unix_nanos: 100,
                        expires_unix_nanos: 200,
                        quorum_voters: vec!["a".into(), "b".into()],
                    },
                ],
            })
            .unwrap();

        let controller = HtbBorrowingController::new(HtbPolicy {
            revision: 1,
            root_id: "region".into(),
            classes: vec![
                HtbClassSpec {
                    id: "region".into(),
                    parent_id: None,
                    guaranteed_iops: 2_000_000,
                    ceiling_iops: 8_000_000,
                    borrow_weight: 1,
                    burst_seconds: 0,
                    lanes: 0,
                },
                HtbClassSpec {
                    id: "volume".into(),
                    parent_id: Some("region".into()),
                    guaranteed_iops: 2_000_000,
                    ceiling_iops: 8_000_000,
                    borrow_weight: 1,
                    burst_seconds: 0,
                    lanes: 1,
                },
            ],
        })
        .unwrap();
        let plan = controller
            .compute_grants(
                2,
                0,
                &HtbDemandSnapshot {
                    interval_start_ns: 0,
                    interval_end_ns: 1,
                    demand_iops: BTreeMap::from([("volume".into(), 8_000_000)]),
                },
            )
            .unwrap();
        let initial = LaneBudgetSnapshot {
            generation: 1,
            sustained_iops: 2_000_000,
            peak_iops: 2_000_000,
            burst_ops: 1,
            quantum_ops: 1,
            metric_publish_ns: 100,
            effective_ns: 0,
            fallback_sustained_iops: 2_000_000,
            fallback_peak_iops: 2_000_000,
            valid_until_ns: 0,
        };
        let mailbox = Arc::new(LaneBudgetMailbox::new(initial));
        let mut publisher = HtbGrantPublisher::new(1, 100).unwrap();
        publisher
            .register_leaf("volume", vec![mailbox.clone()])
            .unwrap();
        let authority = store.published_view("regional-control").unwrap();
        let local = RegionalHtbLeaseBridge::publish_from_raft_authority(
            &publisher,
            &controller,
            &plan,
            &authority,
            "a",
            4,
            1,
            150,
            1_000,
            10,
            1_000,
        )
        .unwrap();
        assert_eq!(local.effective_ns, 1_010);
        let mut limiter = LaneLimiter::new(1_000, initial);
        assert!(!limiter.refresh(1_009, &mailbox));
        assert!(limiter.refresh(1_010, &mailbox));
        assert_eq!(limiter.budget().sustained_iops, 8_000_000);
        assert!(limiter.refresh(1_050, &mailbox));
        assert_eq!(limiter.budget().sustained_iops, 2_000_000);
        assert!(
            RegionalHtbLeaseBridge::publish_from_raft_authority(
                &publisher,
                &controller,
                &plan,
                &authority,
                "a",
                4,
                1,
                200,
                2_000,
                10,
                1_000,
            )
            .is_err()
        );
        drop(store);
        fs::remove_file(path).unwrap();
    }
}
