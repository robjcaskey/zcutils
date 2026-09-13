//! Lane-owned completion accounting for an optional client-local durable WAL.
//!
//! This lives in the userspace RAID stage, after the block edge. Transport
//! delivery, volatile admission and CQ completions are deliberately not durable
//! receipts. A terminal writer must drain its WAL before publishing one.
//! Policy is compiled off the data path; advancing an HWM allocates nothing,
//! takes no locks and does no shared atomic updates.

use serde::{Deserialize, Serialize};
use std::io;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitScope {
    pub volume: u64,
    pub log: u64,
    pub writer_epoch: u64,
    pub lane: u32,
}

/// IDs/domains are assigned by the placement authority, not by the sender of
/// an ACK. `incarnation` must change when a replica is replaced or loses state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replica {
    pub id: u64,
    pub incarnation: u64,
    pub failure_domain: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub scope: CommitScope,
    /// Explicit opt-in. Omitting it preserves the two-remote-copy contract.
    #[serde(default)]
    pub count_client_local_wal: bool,
    pub local: Replica,
    pub remote: [Replica; 2],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableReceipt {
    pub scope: CommitScope,
    pub replica: u64,
    pub incarnation: u64,
    /// Contiguous, persisted prefix, not the highest out-of-order completion.
    pub through: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Progress {
    pub submitted: u64,
    pub local_durable: u64,
    pub remote_durable: [u64; 2],
    /// Monotone client-visible commitment. Replacement cannot un-ACK data.
    pub acknowledged: u64,
    /// Both *current* remote replicas have this prefix. Only this frontier can
    /// release client custody; `acknowledged` cannot be used for reclamation.
    pub remote_redundant: u64,
}

/// One instance per lane, owned by that lane's existing completion worker.
pub struct Commitment {
    policy: Policy,
    local_eligible: [bool; 2],
    progress: Progress,
}

/// Borrowed proof of both current remote copies. It cannot be constructed by
/// callers, confused with an early-ACK frontier, or held across replacement of
/// a replica (replacement needs an exclusive borrow of the same tracker).
pub struct RemoteRedundancy<'a> {
    commitment: &'a Commitment,
}

impl RemoteRedundancy<'_> {
    pub fn scope(&self) -> CommitScope {
        self.commitment.policy.scope
    }
    pub fn through(&self) -> u64 {
        self.commitment.progress.remote_redundant
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

impl Commitment {
    pub fn new(policy: Policy) -> io::Result<Self> {
        let replicas = [policy.local, policy.remote[0], policy.remote[1]];
        if policy.scope.volume == 0
            || policy.scope.log == 0
            || policy.scope.writer_epoch == 0
            || replicas
                .iter()
                .any(|r| r.id == 0 || r.incarnation == 0 || r.failure_domain == 0)
            || replicas[0].id == replicas[1].id
            || replicas[0].id == replicas[2].id
            || replicas[1].id == replicas[2].id
        {
            return Err(invalid(
                "commit policy requires nonzero identities and distinct replicas",
            ));
        }
        if policy.remote[0].failure_domain == policy.remote[1].failure_domain {
            return Err(invalid(
                "two-remote mirror requires independent failure domains",
            ));
        }
        let local_eligible = policy.remote.map(|r| {
            policy.count_client_local_wal && r.failure_domain != policy.local.failure_domain
        });
        Ok(Self {
            policy,
            local_eligible,
            progress: Progress::default(),
        })
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    pub fn progress(&self) -> Progress {
        self.progress
    }

    pub fn redundant_prefix(&self) -> RemoteRedundancy<'_> {
        RemoteRedundancy { commitment: self }
    }

    /// Called for a whole ordered batch, not once per 4K record. The WAL/lane
    /// owner is responsible for contiguous admission and preserving payload
    /// leases until all consumers (local disk and NICs) finish with the buffer.
    pub fn submitted(&mut self, through: u64) -> io::Result<()> {
        if through < self.progress.submitted {
            return Err(invalid("submitted WAL HWM regressed"));
        }
        self.progress.submitted = through;
        Ok(())
    }

    /// Consume only a receipt verified on the replica's bound/authenticated
    /// connection. Identity/epoch checks here do not replace authentication.
    pub fn durable(&mut self, receipt: DurableReceipt) -> io::Result<Progress> {
        if receipt.scope != self.policy.scope || receipt.through > self.progress.submitted {
            return Err(invalid(
                "durable receipt is stale, foreign, or ahead of admission",
            ));
        }
        let (replica, hwm) = if receipt.replica == self.policy.local.id {
            (self.policy.local, &mut self.progress.local_durable)
        } else if receipt.replica == self.policy.remote[0].id {
            (self.policy.remote[0], &mut self.progress.remote_durable[0])
        } else if receipt.replica == self.policy.remote[1].id {
            (self.policy.remote[1], &mut self.progress.remote_durable[1])
        } else {
            return Err(invalid("durable receipt is not from a configured replica"));
        };
        if receipt.incarnation != replica.incarnation {
            return Err(invalid(
                "durable receipt is from a replaced replica incarnation",
            ));
        }
        // Duplicate and reordered HWM reports are harmless; never regress.
        *hwm = (*hwm).max(receipt.through);
        self.advance();
        Ok(self.progress)
    }

    fn advance(&mut self) {
        self.progress.remote_redundant =
            self.progress.remote_durable[0].min(self.progress.remote_durable[1]);
        let mut committed = self.progress.remote_redundant;
        for (eligible, remote) in self.local_eligible.iter().zip(self.progress.remote_durable) {
            if *eligible {
                committed = committed.max(self.progress.local_durable.min(remote));
            }
        }
        self.progress.acknowledged = self.progress.acknowledged.max(committed);
    }

    /// The orchestrator fences the old process/route before making this call.
    /// A new incarnation starts with **zero** credit. Surviving replica data or
    /// retained local WAL must seed it; its predecessor's HWM is not inherited.
    /// This does not authorize reclamation of data already retained elsewhere.
    pub fn replace_remote(&mut self, slot: usize, replacement: Replica) -> io::Result<()> {
        if slot >= 2 {
            return Err(invalid("remote slot is out of range"));
        }
        let old = self.policy.remote[slot];
        if replacement.id != old.id
            || replacement.incarnation <= old.incarnation
            || replacement.failure_domain == 0
            || replacement.failure_domain == self.policy.remote[1 - slot].failure_domain
        {
            return Err(invalid(
                "replacement requires a newer incarnation and an independent domain",
            ));
        }
        self.policy.remote[slot] = replacement;
        self.local_eligible[slot] = self.policy.count_client_local_wal
            && replacement.failure_domain != self.policy.local.failure_domain;
        self.progress.remote_durable[slot] = 0;
        self.advance();
        Ok(())
    }

    /// Restore only authenticated/persisted checkpoint state. Do not replay
    /// volatile startup reports as client-visible acknowledgements.
    pub fn recover(policy: Policy, checkpoint: Progress) -> io::Result<Self> {
        let mut result = Self::new(policy)?;
        result.submitted(checkpoint.submitted)?;
        for (replica, through) in [
            (policy.local, checkpoint.local_durable),
            (policy.remote[0], checkpoint.remote_durable[0]),
            (policy.remote[1], checkpoint.remote_durable[1]),
        ] {
            result.durable(DurableReceipt {
                scope: policy.scope,
                replica: replica.id,
                incarnation: replica.incarnation,
                through,
            })?;
        }
        if checkpoint.acknowledged > result.progress.acknowledged
            || checkpoint.remote_redundant != result.progress.remote_redundant
        {
            return Err(invalid(
                "checkpoint claims commitment without sufficient custody",
            ));
        }
        result.progress.acknowledged = checkpoint.acknowledged;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(enabled: bool) -> Policy {
        Policy {
            scope: CommitScope {
                volume: 1,
                log: 2,
                writer_epoch: 3,
                lane: 0,
            },
            count_client_local_wal: enabled,
            local: Replica {
                id: 1,
                incarnation: 1,
                failure_domain: 1,
            },
            remote: [
                Replica {
                    id: 2,
                    incarnation: 1,
                    failure_domain: 2,
                },
                Replica {
                    id: 3,
                    incarnation: 1,
                    failure_domain: 3,
                },
            ],
        }
    }

    fn receipt(c: &Commitment, r: Replica, through: u64) -> DurableReceipt {
        DurableReceipt {
            scope: c.policy.scope,
            replica: r.id,
            incarnation: r.incarnation,
            through,
        }
    }

    fn report(c: &mut Commitment, which: usize, through: u64) -> Progress {
        let p = c.policy();
        c.durable(receipt(
            c,
            [p.local, p.remote[0], p.remote[1]][which],
            through,
        ))
        .unwrap()
    }

    #[test]
    fn default_requires_both_remotes() {
        let mut c = Commitment::new(policy(false)).unwrap();
        c.submitted(100).unwrap();
        report(&mut c, 0, 100);
        assert_eq!(report(&mut c, 1, 100).acknowledged, 0);
        assert_eq!(report(&mut c, 2, 60).acknowledged, 60);
    }

    #[test]
    fn early_ack_is_not_permission_to_reclaim() {
        let mut c = Commitment::new(policy(true)).unwrap();
        c.submitted(100).unwrap();
        report(&mut c, 0, 100);
        let progress = report(&mut c, 1, 80);
        assert_eq!(progress.acknowledged, 80);
        assert_eq!(progress.remote_redundant, 0);
        let progress = report(&mut c, 2, 50);
        assert_eq!(progress.acknowledged, 80);
        assert_eq!(progress.remote_redundant, 50);
    }

    #[test]
    fn all_receipt_orders_and_duplicates_have_the_same_frontier() {
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let mut c = Commitment::new(policy(true)).unwrap();
            c.submitted(100).unwrap();
            for which in order {
                let p = report(&mut c, which, 100);
                assert_eq!(report(&mut c, which, 50), p);
            }
            assert_eq!(c.progress().acknowledged, 100);
            assert_eq!(c.progress().remote_redundant, 100);
        }
    }

    #[test]
    fn same_domain_local_wal_cannot_double_count_the_middle_node() {
        let mut p = policy(true);
        p.local.failure_domain = p.remote[0].failure_domain;
        let mut c = Commitment::new(p).unwrap();
        c.submitted(100).unwrap();
        report(&mut c, 0, 100);
        assert_eq!(report(&mut c, 1, 100).acknowledged, 0);
        assert_eq!(report(&mut c, 2, 75).acknowledged, 75);
    }

    #[test]
    fn replacing_middle_preserves_ack_but_needs_repair() {
        let mut c = Commitment::new(policy(true)).unwrap();
        c.submitted(100).unwrap();
        report(&mut c, 0, 100);
        report(&mut c, 1, 100);
        report(&mut c, 2, 40);
        let stale = receipt(&c, c.policy.remote[0], 100);
        let mut replacement = c.policy.remote[0];
        replacement.incarnation += 1;
        c.replace_remote(0, replacement).unwrap();
        assert_eq!(c.progress().acknowledged, 100);
        assert_eq!(c.progress().remote_redundant, 0);
        assert!(c.durable(stale).is_err());
        // The surviving remote seeds [0,40]; local custody supplies (40,100].
        assert_eq!(report(&mut c, 1, 100).remote_redundant, 40);
        assert_eq!(report(&mut c, 2, 100).remote_redundant, 100);
    }

    #[test]
    fn unsubmitted_foreign_or_stale_receipts_cannot_change_state() {
        let mut c = Commitment::new(policy(true)).unwrap();
        c.submitted(100).unwrap();
        let valid = receipt(&c, c.policy.remote[0], 100);
        let mut bad = [valid; 5];
        bad[0].through = 101;
        bad[1].scope.writer_epoch += 1;
        bad[2].scope.lane += 1;
        bad[3].incarnation += 1;
        bad[4].replica = 4;
        let before = c.progress();
        for receipt in bad {
            assert!(c.durable(receipt).is_err());
            assert_eq!(c.progress(), before);
        }
    }

    #[test]
    fn invalid_policies_and_unbacked_checkpoints_fail_closed() {
        let p = policy(true);
        let mut bad = p;
        bad.remote[1].failure_domain = bad.remote[0].failure_domain;
        assert!(Commitment::new(bad).is_err());
        let progress = Progress {
            submitted: 100,
            local_durable: 100,
            acknowledged: 100,
            ..Progress::default()
        };
        assert!(Commitment::recover(p, progress).is_err());
    }
}
