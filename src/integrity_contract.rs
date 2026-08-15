use std::collections::BTreeSet;
use std::io;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrityEvidence {
    Unverified,
    FramingOnly,
    EndToEnd,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeafIntegrityCapability {
    pub leaf_id: String,
    pub fault_domain: String,
    pub memory_ecc: bool,
    pub transport_integrity: bool,
    pub media_integrity: IntegrityEvidence,
    pub durable_flush: bool,
    pub power_fail_atomic_bytes: u64,
}

impl LeafIntegrityCapability {
    pub fn detects_leaf_corruption(&self) -> bool {
        self.memory_ecc
            && self.transport_integrity
            && self.media_integrity == IntegrityEvidence::EndToEnd
    }

    pub fn validates_durable_write(&self, write_bytes: u64) -> bool {
        self.durable_flush
            && self.power_fail_atomic_bytes != 0
            && write_bytes <= self.power_fail_atomic_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserspaceComposition {
    Single,
    Stripe { leaves: usize },
    Replicas { copies: usize },
    Raid10 { groups: usize, copies: usize },
    Erasure { data: usize, parity: usize },
}

/// The userspace operation that is actually present in the read/recovery path.
/// Redundant placement alone is deliberately not treated as correction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CorruptionResolver {
    None,
    ReplicaMajority,
    ErasureDecode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitProtocol {
    /// Flush frame headers and payloads before publishing and flushing the
    /// durable tail. This does not require whole-extent atomic writes.
    PayloadThenCommit,
    /// Publish with one drain only when the complete extent is proven atomic.
    AtomicExtent,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RequiredFaultModel {
    pub known_erasures: usize,
    pub silent_corruptions: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrityAdmission {
    pub independent_fault_domains: usize,
    pub local_detection_converts_corruption_to_erasure: bool,
    pub correction_budget: usize,
    pub resolver: CorruptionResolver,
    pub commit_protocol: CommitProtocol,
    pub max_atomic_write_bytes: u64,
    pub proof: String,
}

pub fn admit_topology(
    composition: UserspaceComposition,
    leaves: &[LeafIntegrityCapability],
    required: RequiredFaultModel,
    resolver: CorruptionResolver,
    commit_protocol: CommitProtocol,
    max_atomic_write_bytes: u64,
) -> io::Result<IntegrityAdmission> {
    let expected = match composition {
        UserspaceComposition::Single => 1,
        UserspaceComposition::Stripe { leaves } => leaves,
        UserspaceComposition::Replicas { copies } => copies,
        UserspaceComposition::Raid10 { groups, copies } => groups
            .checked_mul(copies)
            .ok_or_else(|| invalid("integrity RAID10 width overflow"))?,
        UserspaceComposition::Erasure { data, parity } => data
            .checked_add(parity)
            .ok_or_else(|| invalid("integrity topology width overflow"))?,
    };
    if expected == 0 || leaves.len() != expected {
        return Err(invalid(format!(
            "integrity topology has {} leaves, expected {expected}",
            leaves.len()
        )));
    }
    let domains = leaves
        .iter()
        .map(|leaf| leaf.fault_domain.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    if domains != leaves.len() {
        return Err(invalid(format!(
            "integrity topology aliases fault domains: leaves={} independent_domains={domains}",
            leaves.len()
        )));
    }
    for leaf in leaves {
        let durable = match commit_protocol {
            CommitProtocol::PayloadThenCommit => leaf.durable_flush,
            CommitProtocol::AtomicExtent => {
                max_atomic_write_bytes != 0 && leaf.validates_durable_write(max_atomic_write_bytes)
            }
        };
        if !durable {
            return Err(invalid(format!(
                "leaf {:?} cannot prove commit protocol {commit_protocol:?} for a {}-byte write: durable_flush={} power_fail_atomic_bytes={}",
                leaf.leaf_id,
                max_atomic_write_bytes,
                leaf.durable_flush,
                leaf.power_fail_atomic_bytes
            )));
        }
    }

    let all_locally_detect = leaves
        .iter()
        .all(LeafIntegrityCapability::detects_leaf_corruption);
    let effective_erasures = required
        .known_erasures
        .checked_add(if all_locally_detect {
            required.silent_corruptions
        } else {
            0
        })
        .ok_or_else(|| invalid("integrity erasure budget overflow"))?;
    let effective_silent = if all_locally_detect {
        0
    } else {
        required.silent_corruptions
    };

    let (satisfied, correction_budget, needed_resolver, proof) = match composition {
        UserspaceComposition::Single | UserspaceComposition::Stripe { .. } => {
            let satisfied = effective_erasures == 0 && effective_silent == 0;
            (
                satisfied,
                0,
                CorruptionResolver::None,
                format!(
                    "non-redundant composition requires zero residual faults: erasures={effective_erasures} silent={effective_silent}"
                ),
            )
        }
        UserspaceComposition::Replicas { copies } => {
            let silent_budget = copies.saturating_sub(1) / 2;
            let satisfied = effective_silent <= silent_budget
                && effective_erasures < copies
                && effective_silent
                    .checked_mul(2)
                    .and_then(|value| value.checked_add(effective_erasures))
                    .is_some_and(|cost| cost < copies);
            (
                satisfied,
                silent_budget,
                if effective_silent != 0 || effective_erasures != 0 {
                    CorruptionResolver::ReplicaMajority
                } else {
                    CorruptionResolver::None
                },
                format!(
                    "replica majority requires 2*silent+erasures < copies: 2*{effective_silent}+{effective_erasures} < {copies}"
                ),
            )
        }
        UserspaceComposition::Raid10 { groups, copies } => {
            let silent_budget = copies.saturating_sub(1) / 2;
            let satisfied = effective_silent <= silent_budget
                && effective_erasures < copies
                && effective_silent
                    .checked_mul(2)
                    .and_then(|value| value.checked_add(effective_erasures))
                    .is_some_and(|cost| cost < copies);
            (
                satisfied,
                silent_budget,
                if effective_silent != 0 || effective_erasures != 0 {
                    CorruptionResolver::ReplicaMajority
                } else {
                    CorruptionResolver::None
                },
                format!(
                    "RAID10 worst-case fault concentration requires 2*silent+erasures < copies-per-group: 2*{effective_silent}+{effective_erasures} < {copies} across {groups} groups"
                ),
            )
        }
        UserspaceComposition::Erasure { data, parity } => {
            let cost = effective_silent
                .checked_mul(2)
                .and_then(|value| value.checked_add(effective_erasures))
                .ok_or_else(|| invalid("integrity correction cost overflow"))?;
            (
                cost <= parity,
                parity / 2,
                if effective_silent != 0 || effective_erasures != 0 {
                    CorruptionResolver::ErasureDecode
                } else {
                    CorruptionResolver::None
                },
                format!(
                    "({},{data}) code requires 2*silent+erasures <= parity: 2*{effective_silent}+{effective_erasures} <= {parity}",
                    data + parity
                ),
            )
        }
    };
    if !satisfied {
        return Err(invalid(format!(
            "integrity topology cannot satisfy fault model: {proof}"
        )));
    }
    if needed_resolver != CorruptionResolver::None && resolver != needed_resolver {
        return Err(invalid(format!(
            "integrity topology has redundancy but lacks the required userspace correction operator: required={needed_resolver:?} configured={resolver:?}; {proof}"
        )));
    }
    Ok(IntegrityAdmission {
        independent_fault_domains: domains,
        local_detection_converts_corruption_to_erasure: all_locally_detect,
        correction_budget,
        resolver,
        commit_protocol,
        max_atomic_write_bytes,
        proof: format!(
            "{proof}; resolver={resolver:?}; commit_protocol={commit_protocol:?}; max_atomic_write_bytes={max_atomic_write_bytes}; independent_fault_domains={domains}"
        ),
    })
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(count: usize, protected: bool) -> Vec<LeafIntegrityCapability> {
        (0..count)
            .map(|index| LeafIntegrityCapability {
                leaf_id: format!("leaf{index}"),
                fault_domain: format!("host{index}/controller0"),
                memory_ecc: protected,
                transport_integrity: true,
                media_integrity: if protected {
                    IntegrityEvidence::EndToEnd
                } else {
                    IntegrityEvidence::FramingOnly
                },
                durable_flush: true,
                power_fail_atomic_bytes: 4096,
            })
            .collect()
    }

    #[test]
    fn two_replicas_detect_but_cannot_correct_unknown_corruption() {
        let error = admit_topology(
            UserspaceComposition::Replicas { copies: 2 },
            &leaves(2, false),
            RequiredFaultModel {
                known_erasures: 0,
                silent_corruptions: 1,
            },
            CorruptionResolver::None,
            CommitProtocol::AtomicExtent,
            4096,
        )
        .unwrap_err();
        assert!(error.to_string().contains("2*1+0 < 2"));
    }

    #[test]
    fn three_replicas_correct_one_unknown_corruption() {
        let admission = admit_topology(
            UserspaceComposition::Replicas { copies: 3 },
            &leaves(3, false),
            RequiredFaultModel {
                known_erasures: 0,
                silent_corruptions: 1,
            },
            CorruptionResolver::ReplicaMajority,
            CommitProtocol::AtomicExtent,
            4096,
        )
        .unwrap();
        assert_eq!(admission.correction_budget, 1);
    }

    #[test]
    fn local_detection_makes_one_corruption_a_parity_erasure() {
        let admission = admit_topology(
            UserspaceComposition::Erasure { data: 4, parity: 1 },
            &leaves(5, true),
            RequiredFaultModel {
                known_erasures: 0,
                silent_corruptions: 1,
            },
            CorruptionResolver::ErasureDecode,
            CommitProtocol::AtomicExtent,
            4096,
        )
        .unwrap();
        assert!(admission.local_detection_converts_corruption_to_erasure);
    }

    #[test]
    fn duplicated_fault_domain_is_rejected() {
        let mut leaves = leaves(3, false);
        leaves[2].fault_domain = leaves[1].fault_domain.clone();
        assert!(
            admit_topology(
                UserspaceComposition::Replicas { copies: 3 },
                &leaves,
                RequiredFaultModel::default(),
                CorruptionResolver::None,
                CommitProtocol::AtomicExtent,
                4096,
            )
            .is_err()
        );
    }

    #[test]
    fn redundant_placement_without_a_resolver_is_rejected() {
        let error = admit_topology(
            UserspaceComposition::Replicas { copies: 3 },
            &leaves(3, false),
            RequiredFaultModel {
                known_erasures: 0,
                silent_corruptions: 1,
            },
            CorruptionResolver::None,
            CommitProtocol::AtomicExtent,
            4096,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("lacks the required userspace correction operator")
        );
    }

    #[test]
    fn durable_atomic_unit_is_part_of_admission() {
        let error = admit_topology(
            UserspaceComposition::Single,
            &leaves(1, true),
            RequiredFaultModel::default(),
            CorruptionResolver::None,
            CommitProtocol::AtomicExtent,
            8192,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("AtomicExtent for a 8192-byte write")
        );
    }

    #[test]
    fn two_phase_commit_needs_flush_but_not_whole_extent_atomicity() {
        let admission = admit_topology(
            UserspaceComposition::Single,
            &leaves(1, true),
            RequiredFaultModel::default(),
            CorruptionResolver::None,
            CommitProtocol::PayloadThenCommit,
            1024 * 1024,
        )
        .unwrap();
        assert_eq!(admission.commit_protocol, CommitProtocol::PayloadThenCommit);
    }
}
