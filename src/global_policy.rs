//! Global, Raft-replicated policy state above regional HTB controllers.
//!
//! This state is deliberately low-rate.  A global leader commits capacity and
//! cross-region cluster-link changes, then regional controllers translate the
//! resulting grants into their existing short local-monotonic lane leases.

use crate::global_failover::{FailoverState, GlobalFailoverCommand};
use crate::htb_controller::{
    HtbBorrowingController, HtbClassSpec, HtbDemandSnapshot, HtbGrantPlan, HtbPolicy,
};
use crate::transport_security::{LinkTransportSecurity, NetworkTrustPolicy};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;

const MAX_FEDERATION_RECORDS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalRegionSpec {
    pub region_id: String,
    pub guaranteed_iops: u64,
    pub ceiling_iops: u64,
    pub borrow_weight: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalRatePolicy {
    pub revision: u64,
    pub guaranteed_iops: u64,
    pub ceiling_iops: u64,
    pub regions: Vec<GlobalRegionSpec>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalDemandSnapshot {
    pub interval_start_ns: u64,
    pub interval_end_ns: u64,
    pub region_demand_iops: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalRegionGrant {
    pub region_id: String,
    pub target_iops: u64,
    pub guaranteed_iops: u64,
    pub borrowed_iops: u64,
    pub ceiling_iops: u64,
}

/// The slow Raft plane's input to a regional controller. Regional controllers
/// may subdivide this envelope, but cannot exceed it while the lease is valid.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionalCapacityEnvelope {
    pub policy_revision: u64,
    pub generation: u64,
    pub region_id: String,
    pub authorized_iops: u64,
    pub protected_iops: u64,
    pub authority_valid: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalGrantPlan {
    pub generation: u64,
    pub policy_revision: u64,
    pub observed_interval_end_ns: u64,
    pub grants: Vec<GlobalRegionGrant>,
}

impl GlobalGrantPlan {
    pub fn grant_for(&self, region_id: &str) -> Option<&GlobalRegionGrant> {
        self.grants
            .iter()
            .find(|grant| grant.region_id == region_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterLink {
    pub link_id: String,
    pub source_cluster_id: String,
    pub target_cluster_id: String,
    pub source_region_id: String,
    pub target_region_id: String,
    pub generation: u64,
    pub reserved_iops: u64,
    pub ceiling_iops: u64,
    /// Exact directional trust grant authorizing ciphertext placement.
    pub trust_grant_id: String,
    pub trust_grant_generation: u64,
    /// User data is always protected. TLS is an optional outer compliance
    /// layer and is deliberately distinct from native framing encryption.
    #[serde(default)]
    pub transport_security: LinkTransportSecurity,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyEscrowMode {
    #[default]
    Denied,
    OnDemand,
    AutomaticOnLoss,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionalTrustPermissions {
    /// The delegate may retain ciphertext, but receives no key by implication.
    pub store_encrypted_replicas: bool,
    /// Plaintext placement is a distinct and normally-disabled capability.
    #[serde(default)]
    pub store_unencrypted_replicas: bool,
    /// The delegate may serve retained ciphertext during restore.
    pub serve_encrypted_restore: bool,
    pub key_escrow: KeyEscrowMode,
    /// Regions to which this delegate may release the owner's recovery key.
    /// This is intentionally explicit and never transitive.
    #[serde(default)]
    pub key_release_regions: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionalTrustGrant {
    pub grant_id: String,
    /// Region that owns the data and keys.
    pub owner_region_id: String,
    /// Region receiving narrowly delegated authority.
    pub delegate_region_id: String,
    pub generation: u64,
    pub permissions: RegionalTrustPermissions,
}

/// Expiring authorization handed to a regional worker. This contains policy
/// only: decryption keys and wrapped key material must never enter Raft state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionalTrustEnvelope {
    pub grant_id: String,
    pub generation: u64,
    pub owner_region_id: String,
    pub delegate_region_id: String,
    pub permissions: RegionalTrustPermissions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeEncryption {
    Encrypted,
    Unencrypted,
}

/// Destination-owned admission policy. Source grants and destination
/// admission must both allow an operation; neither implies the other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionalInboundPolicy {
    pub region_id: String,
    pub generation: u64,
    #[serde(default)]
    pub allowed_source_regions: BTreeSet<String>,
    pub accept_encrypted_volumes: bool,
    pub accept_unencrypted_volumes: bool,
    pub accept_key_escrow: KeyEscrowMode,
    /// Zero means no size limit at this policy layer.
    pub max_volume_bytes: u64,
    #[serde(default)]
    pub allowed_data_classes: BTreeSet<String>,
    /// Non-sensitive, exact-match placement attributes such as tier or tenant
    /// class. Empty means no required attributes.
    #[serde(default)]
    pub required_attributes: BTreeMap<String, String>,
    /// Any exact match here is an explicit denial and wins over every allow.
    #[serde(default)]
    pub denied_attributes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumePlacementRequest {
    pub source_region_id: String,
    pub destination_region_id: String,
    pub encryption: VolumeEncryption,
    pub volume_bytes: u64,
    pub data_class: String,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FederationDenial {
    RaftAuthorityExpired,
    MissingOutboundGrant,
    MissingInboundPolicy,
    SourceRegionDenied,
    EncryptionDeniedByOwner,
    EncryptionDeclinedByDestination,
    VolumeTooLarge,
    DataClassDenied,
    RequiredAttributeMissing,
    ExplicitAttributeDenial,
    KeyEscrowDeniedByOwner,
    KeyEscrowDeclinedByDestination,
    KeyReleaseRegionDenied,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum GlobalPolicyCommand {
    SetRatePolicy {
        policy: GlobalRatePolicy,
        demand: GlobalDemandSnapshot,
    },
    LinkClusters {
        link: ClusterLink,
    },
    UnlinkClusters {
        link_id: String,
        generation: u64,
    },
    SetRegionTrust {
        grant: RegionalTrustGrant,
    },
    RevokeRegionTrust {
        grant_id: String,
        generation: u64,
    },
    SetRegionalInboundPolicy {
        policy: RegionalInboundPolicy,
    },
    RevokeRegionalInboundPolicy {
        region_id: String,
        generation: u64,
    },
    SetNetworkTrustPolicy {
        policy: NetworkTrustPolicy,
    },
    ApplyFailover {
        command: GlobalFailoverCommand,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalPolicyState {
    pub applied_index: u64,
    pub applied_term: u64,
    pub rate_policy: Option<GlobalRatePolicy>,
    pub grant_plan: Option<GlobalGrantPlan>,
    pub cluster_links: BTreeMap<String, ClusterLink>,
    #[serde(default)]
    pub unlinked_generations: BTreeMap<String, u64>,
    #[serde(default)]
    pub region_trust_grants: BTreeMap<String, RegionalTrustGrant>,
    #[serde(default)]
    pub revoked_trust_generations: BTreeMap<String, u64>,
    #[serde(default)]
    pub regional_inbound_policies: BTreeMap<String, RegionalInboundPolicy>,
    #[serde(default)]
    pub revoked_inbound_policy_generations: BTreeMap<String, u64>,
    #[serde(default)]
    pub network_trust_policy: NetworkTrustPolicy,
    #[serde(default)]
    pub failover: FailoverState,
}

impl GlobalPolicyState {
    pub fn apply(
        &mut self,
        index: u64,
        term: u64,
        command: &GlobalPolicyCommand,
    ) -> io::Result<()> {
        if index != self.applied_index.saturating_add(1) {
            return Err(invalid(format!(
                "global policy index gap expected={} got={index}",
                self.applied_index.saturating_add(1)
            )));
        }
        if term < self.applied_term {
            return Err(invalid("global policy term regressed"));
        }
        let mut staged = self.clone();
        staged.apply_command(index, command)?;
        staged.applied_index = index;
        staged.applied_term = term;
        *self = staged;
        Ok(())
    }

    fn apply_command(&mut self, index: u64, command: &GlobalPolicyCommand) -> io::Result<()> {
        match command {
            GlobalPolicyCommand::SetRatePolicy { policy, demand } => {
                if demand.interval_end_ns < demand.interval_start_ns
                    || demand.region_demand_iops.len() > MAX_FEDERATION_RECORDS
                    || demand
                        .region_demand_iops
                        .keys()
                        .any(|region| !valid_identifier(region))
                {
                    return Err(invalid("invalid global demand snapshot"));
                }
                if self.rate_policy.as_ref() == Some(policy)
                    && self
                        .grant_plan
                        .as_ref()
                        .is_some_and(|plan| plan.observed_interval_end_ns == demand.interval_end_ns)
                {
                    return Ok(());
                }
                if self
                    .rate_policy
                    .as_ref()
                    .is_some_and(|current| policy.revision <= current.revision)
                {
                    return Err(invalid("global rate-policy revision did not advance"));
                }
                let controller = GlobalRateController::new(policy.clone())?;
                let plan = controller.compute_grants(index, demand)?;
                self.rate_policy = Some(policy.clone());
                self.grant_plan = Some(plan);
                Ok(())
            }
            GlobalPolicyCommand::LinkClusters { link } => {
                validate_link(link)?;
                let trust = self
                    .region_trust_grants
                    .get(&link.trust_grant_id)
                    .ok_or_else(|| invalid("cluster link has no active regional trust grant"))?;
                if trust.owner_region_id != link.source_region_id
                    || trust.delegate_region_id != link.target_region_id
                    || trust.generation != link.trust_grant_generation
                    || !trust.permissions.store_encrypted_replicas
                {
                    return Err(invalid(
                        "cluster link is not authorized for encrypted replica placement",
                    ));
                }
                let inbound = self
                    .regional_inbound_policies
                    .get(&link.target_region_id)
                    .ok_or_else(|| invalid("cluster link has no destination admission policy"))?;
                if !inbound.accept_encrypted_volumes
                    || !inbound
                        .allowed_source_regions
                        .contains(&link.source_region_id)
                {
                    return Err(invalid("destination declined encrypted replica placement"));
                }
                if self.cluster_links.get(&link.link_id) == Some(link) {
                    return Ok(());
                }
                let last_unlinked = self
                    .unlinked_generations
                    .get(&link.link_id)
                    .copied()
                    .unwrap_or(0);
                if link.generation <= last_unlinked
                    || self
                        .cluster_links
                        .get(&link.link_id)
                        .is_some_and(|current| link.generation <= current.generation)
                {
                    return Err(invalid("cluster-link generation did not advance"));
                }
                if self.cluster_links.len() >= MAX_FEDERATION_RECORDS
                    && !self.cluster_links.contains_key(&link.link_id)
                {
                    return Err(invalid("cluster-link limit reached"));
                }
                self.cluster_links
                    .insert(link.link_id.clone(), link.clone());
                Ok(())
            }
            GlobalPolicyCommand::UnlinkClusters {
                link_id,
                generation,
            } => {
                if link_id.is_empty() || *generation == 0 {
                    return Err(invalid("invalid cluster unlink"));
                }
                if !self.cluster_links.contains_key(link_id)
                    && self.unlinked_generations.get(link_id) == Some(generation)
                {
                    return Ok(());
                }
                let current = self
                    .cluster_links
                    .get(link_id)
                    .ok_or_else(|| invalid("unknown cluster link"))?;
                if *generation <= current.generation {
                    return Err(invalid("cluster unlink generation did not advance"));
                }
                self.cluster_links.remove(link_id);
                self.unlinked_generations
                    .insert(link_id.clone(), *generation);
                Ok(())
            }
            GlobalPolicyCommand::SetRegionTrust { grant } => {
                validate_trust_grant(grant)?;
                if self.region_trust_grants.get(&grant.grant_id) == Some(grant) {
                    return Ok(());
                }
                let revoked = self
                    .revoked_trust_generations
                    .get(&grant.grant_id)
                    .copied()
                    .unwrap_or(0);
                if grant.generation <= revoked
                    || self
                        .region_trust_grants
                        .get(&grant.grant_id)
                        .is_some_and(|current| grant.generation <= current.generation)
                {
                    return Err(invalid("regional trust generation did not advance"));
                }
                if self.region_trust_grants.len() >= MAX_FEDERATION_RECORDS
                    && !self.region_trust_grants.contains_key(&grant.grant_id)
                {
                    return Err(invalid("regional trust-grant limit reached"));
                }
                // A narrower replacement must not leave a formerly-authorized
                // data link alive. Re-linking is always an explicit operation.
                remove_links_for_grant(
                    &mut self.cluster_links,
                    &mut self.unlinked_generations,
                    &grant.grant_id,
                );
                self.region_trust_grants
                    .insert(grant.grant_id.clone(), grant.clone());
                Ok(())
            }
            GlobalPolicyCommand::RevokeRegionTrust {
                grant_id,
                generation,
            } => {
                if grant_id.is_empty() || *generation == 0 {
                    return Err(invalid("invalid regional trust revocation"));
                }
                if !self.region_trust_grants.contains_key(grant_id)
                    && self.revoked_trust_generations.get(grant_id) == Some(generation)
                {
                    return Ok(());
                }
                let current = self
                    .region_trust_grants
                    .get(grant_id)
                    .ok_or_else(|| invalid("unknown regional trust grant"))?;
                if *generation <= current.generation {
                    return Err(invalid(
                        "regional trust revocation generation did not advance",
                    ));
                }
                self.region_trust_grants.remove(grant_id);
                self.revoked_trust_generations
                    .insert(grant_id.clone(), *generation);
                remove_links_for_grant(
                    &mut self.cluster_links,
                    &mut self.unlinked_generations,
                    grant_id,
                );
                Ok(())
            }
            GlobalPolicyCommand::SetRegionalInboundPolicy { policy } => {
                validate_inbound_policy(policy)?;
                if self.regional_inbound_policies.get(&policy.region_id) == Some(policy) {
                    return Ok(());
                }
                if self.regional_inbound_policies.len() >= MAX_FEDERATION_RECORDS
                    && !self
                        .regional_inbound_policies
                        .contains_key(&policy.region_id)
                {
                    return Err(invalid("regional inbound-policy limit reached"));
                }
                let revoked = self
                    .revoked_inbound_policy_generations
                    .get(&policy.region_id)
                    .copied()
                    .unwrap_or(0);
                if policy.generation <= revoked
                    || self
                        .regional_inbound_policies
                        .get(&policy.region_id)
                        .is_some_and(|current| policy.generation <= current.generation)
                {
                    return Err(invalid(
                        "regional inbound-policy generation did not advance",
                    ));
                }
                self.regional_inbound_policies
                    .insert(policy.region_id.clone(), policy.clone());
                Ok(())
            }
            GlobalPolicyCommand::RevokeRegionalInboundPolicy {
                region_id,
                generation,
            } => {
                if region_id.is_empty() || *generation == 0 {
                    return Err(invalid("invalid inbound-policy revocation"));
                }
                if !self.regional_inbound_policies.contains_key(region_id)
                    && self.revoked_inbound_policy_generations.get(region_id) == Some(generation)
                {
                    return Ok(());
                }
                let current = self
                    .regional_inbound_policies
                    .get(region_id)
                    .ok_or_else(|| invalid("unknown regional inbound policy"))?;
                if *generation <= current.generation {
                    return Err(invalid(
                        "inbound-policy revocation generation did not advance",
                    ));
                }
                self.regional_inbound_policies.remove(region_id);
                self.revoked_inbound_policy_generations
                    .insert(region_id.clone(), *generation);
                Ok(())
            }
            GlobalPolicyCommand::SetNetworkTrustPolicy { policy } => {
                if policy.generation == 0 {
                    return Err(invalid("network trust-policy generation must be non-zero"));
                }
                if policy.generation <= self.network_trust_policy.generation {
                    if policy == &self.network_trust_policy {
                        return Ok(());
                    }
                    return Err(invalid("network trust-policy generation did not advance"));
                }
                self.network_trust_policy = policy.clone();
                Ok(())
            }
            GlobalPolicyCommand::ApplyFailover { command } => self.failover.apply(command),
        }
    }

    pub fn protected_iops(&self, region_id: &str) -> u64 {
        self.rate_policy
            .as_ref()
            .and_then(|policy| {
                policy
                    .regions
                    .iter()
                    .find(|region| region.region_id == region_id)
            })
            .map_or(0, |region| region.guaranteed_iops)
    }

    pub fn authorized_iops(&self, region_id: &str) -> u64 {
        self.grant_plan
            .as_ref()
            .and_then(|plan| plan.grant_for(region_id))
            .map_or_else(|| self.protected_iops(region_id), |grant| grant.target_iops)
    }

    pub fn capacity_envelope(
        &self,
        region_id: &str,
        authority_valid: bool,
    ) -> RegionalCapacityEnvelope {
        let protected_iops = self.protected_iops(region_id);
        RegionalCapacityEnvelope {
            policy_revision: self
                .rate_policy
                .as_ref()
                .map_or(0, |policy| policy.revision),
            generation: self.grant_plan.as_ref().map_or(0, |plan| plan.generation),
            region_id: region_id.to_owned(),
            authorized_iops: if authority_valid {
                self.authorized_iops(region_id)
            } else {
                protected_iops
            },
            protected_iops,
            authority_valid,
        }
    }

    pub fn encrypted_replica_authorized(
        &self,
        grant_id: &str,
        owner_region_id: &str,
        delegate_region_id: &str,
        authority_valid: bool,
    ) -> bool {
        self.trust_envelope(grant_id, authority_valid)
            .is_some_and(|grant| {
                grant.owner_region_id == owner_region_id
                    && grant.delegate_region_id == delegate_region_id
                    && grant.permissions.store_encrypted_replicas
            })
    }

    pub fn key_escrow_authorized(
        &self,
        grant_id: &str,
        owner_region_id: &str,
        delegate_region_id: &str,
        release_region_id: &str,
        automatic: bool,
        authority_valid: bool,
    ) -> bool {
        self.trust_envelope(grant_id, authority_valid)
            .is_some_and(|grant| {
                grant.owner_region_id == owner_region_id
                    && grant.delegate_region_id == delegate_region_id
                    && grant
                        .permissions
                        .key_release_regions
                        .contains(release_region_id)
                    && match (grant.permissions.key_escrow, automatic) {
                        (KeyEscrowMode::AutomaticOnLoss, _) => true,
                        (KeyEscrowMode::OnDemand, false) => true,
                        _ => false,
                    }
            })
    }

    pub fn trust_envelope(
        &self,
        grant_id: &str,
        authority_valid: bool,
    ) -> Option<RegionalTrustEnvelope> {
        if !authority_valid {
            return None;
        }
        self.region_trust_grants
            .get(grant_id)
            .map(|grant| RegionalTrustEnvelope {
                grant_id: grant.grant_id.clone(),
                generation: grant.generation,
                owner_region_id: grant.owner_region_id.clone(),
                delegate_region_id: grant.delegate_region_id.clone(),
                permissions: grant.permissions.clone(),
            })
    }

    pub fn authorize_volume_placement(
        &self,
        grant_id: &str,
        request: &VolumePlacementRequest,
        authority_valid: bool,
    ) -> Result<(), FederationDenial> {
        let grant = self
            .trust_envelope(grant_id, authority_valid)
            .ok_or(if authority_valid {
                FederationDenial::MissingOutboundGrant
            } else {
                FederationDenial::RaftAuthorityExpired
            })?;
        if grant.owner_region_id != request.source_region_id
            || grant.delegate_region_id != request.destination_region_id
        {
            return Err(FederationDenial::MissingOutboundGrant);
        }
        let inbound = self
            .regional_inbound_policies
            .get(&request.destination_region_id)
            .ok_or(FederationDenial::MissingInboundPolicy)?;
        if !inbound
            .allowed_source_regions
            .contains(&request.source_region_id)
        {
            return Err(FederationDenial::SourceRegionDenied);
        }
        match request.encryption {
            VolumeEncryption::Encrypted => {
                if !grant.permissions.store_encrypted_replicas {
                    return Err(FederationDenial::EncryptionDeniedByOwner);
                }
                if !inbound.accept_encrypted_volumes {
                    return Err(FederationDenial::EncryptionDeclinedByDestination);
                }
            }
            VolumeEncryption::Unencrypted => {
                if !grant.permissions.store_unencrypted_replicas {
                    return Err(FederationDenial::EncryptionDeniedByOwner);
                }
                if !inbound.accept_unencrypted_volumes {
                    return Err(FederationDenial::EncryptionDeclinedByDestination);
                }
            }
        }
        if inbound.max_volume_bytes != 0 && request.volume_bytes > inbound.max_volume_bytes {
            return Err(FederationDenial::VolumeTooLarge);
        }
        if !inbound.allowed_data_classes.is_empty()
            && !inbound.allowed_data_classes.contains(&request.data_class)
        {
            return Err(FederationDenial::DataClassDenied);
        }
        if inbound
            .denied_attributes
            .iter()
            .any(|(key, value)| request.attributes.get(key) == Some(value))
        {
            return Err(FederationDenial::ExplicitAttributeDenial);
        }
        if !inbound
            .required_attributes
            .iter()
            .all(|(key, value)| request.attributes.get(key) == Some(value))
        {
            return Err(FederationDenial::RequiredAttributeMissing);
        }
        Ok(())
    }

    pub fn authorize_key_escrow_request(
        &self,
        grant_id: &str,
        request: &VolumePlacementRequest,
        release_region_id: &str,
        automatic: bool,
        authority_valid: bool,
    ) -> Result<(), FederationDenial> {
        self.authorize_volume_placement(grant_id, request, authority_valid)?;
        let grant = self
            .region_trust_grants
            .get(grant_id)
            .ok_or(FederationDenial::MissingOutboundGrant)?;
        let inbound = self
            .regional_inbound_policies
            .get(&request.destination_region_id)
            .ok_or(FederationDenial::MissingInboundPolicy)?;
        let owner_allows = matches!(
            (grant.permissions.key_escrow, automatic),
            (KeyEscrowMode::AutomaticOnLoss, _) | (KeyEscrowMode::OnDemand, false)
        );
        if !owner_allows {
            return Err(FederationDenial::KeyEscrowDeniedByOwner);
        }
        let destination_accepts = matches!(
            (inbound.accept_key_escrow, automatic),
            (KeyEscrowMode::AutomaticOnLoss, _) | (KeyEscrowMode::OnDemand, false)
        );
        if !destination_accepts {
            return Err(FederationDenial::KeyEscrowDeclinedByDestination);
        }
        if !grant
            .permissions
            .key_release_regions
            .contains(release_region_id)
        {
            return Err(FederationDenial::KeyReleaseRegionDenied);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct GlobalRateController {
    policy: GlobalRatePolicy,
    htb: HtbBorrowingController,
}

impl GlobalRateController {
    pub fn new(mut policy: GlobalRatePolicy) -> io::Result<Self> {
        if policy.revision == 0
            || policy.regions.is_empty()
            || policy.regions.len() > MAX_FEDERATION_RECORDS
            || policy.ceiling_iops < policy.guaranteed_iops
        {
            return Err(invalid("invalid global rate policy"));
        }
        policy.regions.sort_by(|a, b| a.region_id.cmp(&b.region_id));
        let mut ids = BTreeSet::new();
        let mut protected = 0u64;
        for region in &policy.regions {
            if region.region_id.is_empty()
                || !ids.insert(region.region_id.as_str())
                || region.ceiling_iops < region.guaranteed_iops
                || region.borrow_weight == 0
            {
                return Err(invalid("invalid or duplicate global region"));
            }
            protected = protected
                .checked_add(region.guaranteed_iops)
                .ok_or_else(|| invalid("global guarantee sum overflow"))?;
        }
        if protected > policy.guaranteed_iops {
            return Err(invalid("regional guarantees exceed global guarantee"));
        }
        let mut classes = vec![HtbClassSpec {
            id: "global".into(),
            parent_id: None,
            guaranteed_iops: policy.guaranteed_iops,
            ceiling_iops: policy.ceiling_iops,
            borrow_weight: 1,
            burst_seconds: 0,
            lanes: 0,
        }];
        classes.extend(policy.regions.iter().map(|region| HtbClassSpec {
            id: region.region_id.clone(),
            parent_id: Some("global".into()),
            guaranteed_iops: region.guaranteed_iops,
            ceiling_iops: region.ceiling_iops,
            borrow_weight: region.borrow_weight,
            burst_seconds: 0,
            lanes: 1,
        }));
        let htb = HtbBorrowingController::new(HtbPolicy {
            revision: policy.revision,
            root_id: "global".into(),
            classes,
        })?;
        Ok(Self { policy, htb })
    }

    pub fn compute_grants(
        &self,
        generation: u64,
        demand: &GlobalDemandSnapshot,
    ) -> io::Result<GlobalGrantPlan> {
        let htb_plan = self.htb.compute_grants(
            generation,
            0,
            &HtbDemandSnapshot {
                interval_start_ns: demand.interval_start_ns,
                interval_end_ns: demand.interval_end_ns,
                demand_iops: demand.region_demand_iops.clone(),
            },
        )?;
        Ok(global_plan_from_htb(&htb_plan))
    }

    pub fn policy(&self) -> &GlobalRatePolicy {
        &self.policy
    }
}

fn global_plan_from_htb(plan: &HtbGrantPlan) -> GlobalGrantPlan {
    GlobalGrantPlan {
        generation: plan.generation,
        policy_revision: plan.policy_revision,
        observed_interval_end_ns: plan.observed_interval_end_ns,
        grants: plan
            .grants
            .iter()
            .map(|grant| GlobalRegionGrant {
                region_id: grant.class_id.clone(),
                target_iops: grant.target_iops,
                guaranteed_iops: grant.guaranteed_iops,
                borrowed_iops: grant.borrowed_iops,
                ceiling_iops: grant.ceiling_iops,
            })
            .collect(),
    }
}

fn validate_link(link: &ClusterLink) -> io::Result<()> {
    if !valid_identifier(&link.link_id)
        || !valid_identifier(&link.source_cluster_id)
        || !valid_identifier(&link.target_cluster_id)
        || !valid_identifier(&link.source_region_id)
        || !valid_identifier(&link.target_region_id)
        || link.source_cluster_id == link.target_cluster_id
        || link.source_region_id == link.target_region_id
        || link.generation == 0
        || link.ceiling_iops < link.reserved_iops
        || !valid_identifier(&link.trust_grant_id)
        || link.trust_grant_generation == 0
    {
        return Err(invalid("invalid cross-region cluster link"));
    }
    link.transport_security.validate()
}

fn validate_trust_grant(grant: &RegionalTrustGrant) -> io::Result<()> {
    if grant.grant_id.is_empty()
        || grant.owner_region_id.is_empty()
        || grant.delegate_region_id.is_empty()
        || grant.owner_region_id == grant.delegate_region_id
        || grant.generation == 0
        || (grant.permissions.serve_encrypted_restore
            && !grant.permissions.store_encrypted_replicas)
        || (grant.permissions.key_escrow == KeyEscrowMode::Denied
            && !grant.permissions.key_release_regions.is_empty())
        || (grant.permissions.key_escrow != KeyEscrowMode::Denied
            && grant.permissions.key_release_regions.is_empty())
        || grant
            .permissions
            .key_release_regions
            .iter()
            .any(String::is_empty)
        || grant.permissions.key_release_regions.len() > 32
        || !valid_identifier(&grant.grant_id)
        || !valid_identifier(&grant.owner_region_id)
        || !valid_identifier(&grant.delegate_region_id)
    {
        return Err(invalid("invalid regional trust grant"));
    }
    Ok(())
}

fn validate_inbound_policy(policy: &RegionalInboundPolicy) -> io::Result<()> {
    if !valid_identifier(&policy.region_id)
        || policy.generation == 0
        || policy.allowed_source_regions.is_empty()
        || policy.allowed_source_regions.len() > 128
        || policy.allowed_data_classes.len() > 128
        || policy.required_attributes.len() > 32
        || policy.denied_attributes.len() > 32
        || policy
            .allowed_source_regions
            .iter()
            .any(|id| !valid_identifier(id))
        || policy
            .allowed_data_classes
            .iter()
            .any(|value| !valid_policy_value(value))
        || policy
            .required_attributes
            .iter()
            .chain(&policy.denied_attributes)
            .any(|(key, value)| !valid_policy_value(key) || !valid_policy_value(value))
    {
        return Err(invalid("invalid regional inbound policy"));
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn valid_policy_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn remove_links_for_grant(
    links: &mut BTreeMap<String, ClusterLink>,
    tombstones: &mut BTreeMap<String, u64>,
    grant_id: &str,
) {
    let removed = links
        .iter()
        .filter(|(_, link)| link.trust_grant_id == grant_id)
        .map(|(id, link)| (id.clone(), link.generation.saturating_add(1)))
        .collect::<Vec<_>>();
    for (link_id, generation) in removed {
        links.remove(&link_id);
        tombstones
            .entry(link_id)
            .and_modify(|current| *current = (*current).max(generation))
            .or_insert(generation);
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::global_failover::{
        AdapterKind, GlobalFailoverCommand, VolumeSpec, WorkloadBinding, WorkloadFailoverPolicy,
    };
    use crate::transport_security::{NetworkSegmentScope, SegmentTrust};

    fn policy(revision: u64) -> GlobalRatePolicy {
        GlobalRatePolicy {
            revision,
            guaranteed_iops: 10_000_000,
            ceiling_iops: 20_000_000,
            regions: vec![
                GlobalRegionSpec {
                    region_id: "region-a".into(),
                    guaranteed_iops: 6_000_000,
                    ceiling_iops: 14_000_000,
                    borrow_weight: 3,
                },
                GlobalRegionSpec {
                    region_id: "region-b".into(),
                    guaranteed_iops: 4_000_000,
                    ceiling_iops: 10_000_000,
                    borrow_weight: 1,
                },
            ],
        }
    }

    fn trust_grant(
        id: &str,
        owner: &str,
        delegate: &str,
        generation: u64,
        escrow: KeyEscrowMode,
        release_regions: &[&str],
    ) -> RegionalTrustGrant {
        RegionalTrustGrant {
            grant_id: id.into(),
            owner_region_id: owner.into(),
            delegate_region_id: delegate.into(),
            generation,
            permissions: RegionalTrustPermissions {
                store_encrypted_replicas: true,
                store_unencrypted_replicas: false,
                serve_encrypted_restore: true,
                key_escrow: escrow,
                key_release_regions: release_regions
                    .iter()
                    .map(|region| (*region).to_owned())
                    .collect(),
            },
        }
    }

    fn inbound_policy(region: &str, sources: &[&str]) -> RegionalInboundPolicy {
        RegionalInboundPolicy {
            region_id: region.into(),
            generation: 1,
            allowed_source_regions: sources.iter().map(|source| (*source).to_owned()).collect(),
            accept_encrypted_volumes: true,
            accept_unencrypted_volumes: false,
            accept_key_escrow: KeyEscrowMode::AutomaticOnLoss,
            max_volume_bytes: 1 << 40,
            allowed_data_classes: BTreeSet::from(["backup".into()]),
            required_attributes: BTreeMap::from([("residency".into(), "approved".into())]),
            denied_attributes: BTreeMap::from([("legal_hold".into(), "deny_export".into())]),
        }
    }

    #[test]
    fn global_borrowing_preserves_regional_guarantees() {
        let controller = GlobalRateController::new(policy(1)).unwrap();
        let plan = controller
            .compute_grants(
                1,
                &GlobalDemandSnapshot {
                    interval_start_ns: 1,
                    interval_end_ns: 2,
                    region_demand_iops: BTreeMap::from([
                        ("region-a".into(), 14_000_000),
                        ("region-b".into(), 1_000_000),
                    ]),
                },
            )
            .unwrap();
        assert_eq!(plan.grant_for("region-a").unwrap().target_iops, 14_000_000);
        assert_eq!(plan.grant_for("region-b").unwrap().target_iops, 1_000_000);
    }

    #[test]
    fn network_trust_is_default_deny_monotonic_and_cross_region_invariant() {
        let mut state = GlobalPolicyState::default();
        assert_eq!(
            state
                .network_trust_policy
                .segment_trust(NetworkSegmentScope::SameAz),
            SegmentTrust::Untrusted
        );
        state
            .apply(
                1,
                1,
                &GlobalPolicyCommand::SetNetworkTrustPolicy {
                    policy: NetworkTrustPolicy {
                        generation: 1,
                        same_az: SegmentTrust::Trusted,
                        same_region: SegmentTrust::Untrusted,
                    },
                },
            )
            .unwrap();
        assert_eq!(
            state
                .network_trust_policy
                .segment_trust(NetworkSegmentScope::SameAz),
            SegmentTrust::Trusted
        );
        assert_eq!(
            state
                .network_trust_policy
                .segment_trust(NetworkSegmentScope::CrossRegion),
            SegmentTrust::Untrusted
        );
        assert!(
            state
                .apply(
                    2,
                    1,
                    &GlobalPolicyCommand::SetNetworkTrustPolicy {
                        policy: NetworkTrustPolicy {
                            generation: 1,
                            same_az: SegmentTrust::Untrusted,
                            same_region: SegmentTrust::Untrusted,
                        },
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn link_and_unlink_are_monotonic_replayable_commands() {
        let mut state = GlobalPolicyState::default();
        state
            .apply(
                1,
                1,
                &GlobalPolicyCommand::SetRegionTrust {
                    grant: trust_grant(
                        "trust-a-b",
                        "region-a",
                        "region-b",
                        1,
                        KeyEscrowMode::Denied,
                        &[],
                    ),
                },
            )
            .unwrap();
        state
            .apply(
                2,
                1,
                &GlobalPolicyCommand::SetRegionalInboundPolicy {
                    policy: inbound_policy("region-b", &["region-a"]),
                },
            )
            .unwrap();
        let link = ClusterLink {
            link_id: "a-to-b".into(),
            source_cluster_id: "cluster-a".into(),
            target_cluster_id: "cluster-b".into(),
            source_region_id: "region-a".into(),
            target_region_id: "region-b".into(),
            generation: 1,
            reserved_iops: 100_000,
            ceiling_iops: 1_000_000,
            trust_grant_id: "trust-a-b".into(),
            trust_grant_generation: 1,
            transport_security: LinkTransportSecurity::default(),
        };
        let link_command = GlobalPolicyCommand::LinkClusters { link };
        state.apply(3, 1, &link_command).unwrap();
        state.apply(4, 1, &link_command).unwrap();
        state
            .apply(
                5,
                1,
                &GlobalPolicyCommand::UnlinkClusters {
                    link_id: "a-to-b".into(),
                    generation: 2,
                },
            )
            .unwrap();
        assert!(state.cluster_links.is_empty());
        assert_eq!(state.unlinked_generations["a-to-b"], 2);
        state
            .apply(
                6,
                1,
                &GlobalPolicyCommand::UnlinkClusters {
                    link_id: "a-to-b".into(),
                    generation: 2,
                },
            )
            .unwrap();
    }

    #[test]
    fn federation_permissions_are_directional_non_transitive_and_key_separated() {
        let mut state = GlobalPolicyState::default();
        let grants = [
            trust_grant("us-cn", "us", "china", 1, KeyEscrowMode::Denied, &[]),
            trust_grant(
                "us-uk",
                "us",
                "uk",
                1,
                KeyEscrowMode::AutomaticOnLoss,
                &["us"],
            ),
            trust_grant(
                "uk-us",
                "uk",
                "us",
                1,
                KeyEscrowMode::AutomaticOnLoss,
                &["uk"],
            ),
        ];
        for (offset, grant) in grants.into_iter().enumerate() {
            state
                .apply(
                    offset as u64 + 1,
                    1,
                    &GlobalPolicyCommand::SetRegionTrust { grant },
                )
                .unwrap();
        }

        assert!(state.encrypted_replica_authorized("us-cn", "us", "china", true));
        assert!(!state.encrypted_replica_authorized("us-cn", "us", "china", false));
        assert!(!state.key_escrow_authorized("us-cn", "us", "china", "us", false, true));
        assert!(state.key_escrow_authorized("us-uk", "us", "uk", "us", true, true));
        assert!(!state.key_escrow_authorized("us-uk", "us", "uk", "us", true, false));
        assert!(state.key_escrow_authorized("uk-us", "uk", "us", "uk", true, true));
        assert!(!state.key_escrow_authorized("us-uk", "us", "uk", "china", true, true));
        assert!(!state.key_escrow_authorized("us-uk", "uk", "china", "us", true, true));
    }

    #[test]
    fn revoking_trust_atomically_removes_dependent_links() {
        let mut state = GlobalPolicyState::default();
        state
            .apply(
                1,
                1,
                &GlobalPolicyCommand::SetRegionTrust {
                    grant: trust_grant("us-cn", "us", "china", 1, KeyEscrowMode::Denied, &[]),
                },
            )
            .unwrap();
        state
            .apply(
                2,
                1,
                &GlobalPolicyCommand::SetRegionalInboundPolicy {
                    policy: inbound_policy("china", &["us"]),
                },
            )
            .unwrap();
        state
            .apply(
                3,
                1,
                &GlobalPolicyCommand::LinkClusters {
                    link: ClusterLink {
                        link_id: "backup".into(),
                        source_cluster_id: "us-primary".into(),
                        target_cluster_id: "cn-vault".into(),
                        source_region_id: "us".into(),
                        target_region_id: "china".into(),
                        generation: 1,
                        reserved_iops: 0,
                        ceiling_iops: 1_000_000,
                        trust_grant_id: "us-cn".into(),
                        trust_grant_generation: 1,
                        transport_security: LinkTransportSecurity::default(),
                    },
                },
            )
            .unwrap();
        state
            .apply(
                4,
                1,
                &GlobalPolicyCommand::RevokeRegionTrust {
                    grant_id: "us-cn".into(),
                    generation: 2,
                },
            )
            .unwrap();
        assert!(state.cluster_links.is_empty());
        assert!(!state.encrypted_replica_authorized("us-cn", "us", "china", true));
    }

    #[test]
    fn destination_can_decline_escrow_plaintext_and_policy_factors() {
        let mut state = GlobalPolicyState::default();
        let mut grant = trust_grant(
            "us-cn",
            "us",
            "china",
            1,
            KeyEscrowMode::AutomaticOnLoss,
            &["us"],
        );
        grant.permissions.store_unencrypted_replicas = true;
        state
            .apply(1, 1, &GlobalPolicyCommand::SetRegionTrust { grant })
            .unwrap();
        let mut inbound = inbound_policy("china", &["us"]);
        inbound.accept_key_escrow = KeyEscrowMode::Denied;
        state
            .apply(
                2,
                1,
                &GlobalPolicyCommand::SetRegionalInboundPolicy { policy: inbound },
            )
            .unwrap();
        let mut request = VolumePlacementRequest {
            source_region_id: "us".into(),
            destination_region_id: "china".into(),
            encryption: VolumeEncryption::Encrypted,
            volume_bytes: 1 << 30,
            data_class: "backup".into(),
            attributes: BTreeMap::from([("residency".into(), "approved".into())]),
        };
        assert_eq!(
            state.authorize_volume_placement("us-cn", &request, true),
            Ok(())
        );
        assert_eq!(
            state.authorize_key_escrow_request("us-cn", &request, "us", true, true),
            Err(FederationDenial::KeyEscrowDeclinedByDestination)
        );
        request.encryption = VolumeEncryption::Unencrypted;
        assert_eq!(
            state.authorize_volume_placement("us-cn", &request, true),
            Err(FederationDenial::EncryptionDeclinedByDestination)
        );
        request.encryption = VolumeEncryption::Encrypted;
        request
            .attributes
            .insert("legal_hold".into(), "deny_export".into());
        assert_eq!(
            state.authorize_volume_placement("us-cn", &request, true),
            Err(FederationDenial::ExplicitAttributeDenial)
        );
    }

    #[test]
    fn expired_authority_falls_back_to_the_protected_envelope() {
        let mut state = GlobalPolicyState::default();
        state
            .apply(
                1,
                1,
                &GlobalPolicyCommand::SetRatePolicy {
                    policy: policy(1),
                    demand: GlobalDemandSnapshot {
                        interval_start_ns: 1,
                        interval_end_ns: 2,
                        region_demand_iops: BTreeMap::from([
                            ("region-a".into(), 14_000_000),
                            ("region-b".into(), 1_000_000),
                        ]),
                    },
                },
            )
            .unwrap();
        let live = state.capacity_envelope("region-a", true);
        assert_eq!(live.authorized_iops, 14_000_000);
        let expired = state.capacity_envelope("region-a", false);
        assert_eq!(expired.authorized_iops, 6_000_000);
        assert_eq!(expired.protected_iops, 6_000_000);
    }

    #[test]
    fn failover_commands_are_transactional_raft_state() {
        let mut state = GlobalPolicyState::default();
        let put = GlobalPolicyCommand::ApplyFailover {
            command: GlobalFailoverCommand::PutVolume {
                expected_revision: 0,
                spec: VolumeSpec {
                    volume_id: "postgres".into(),
                    authority_region: "region-a".into(),
                    placement_epoch: 7,
                    consistency_set_id: None,
                    workload_bindings: vec![WorkloadBinding {
                        binding_id: "postgres-kube".into(),
                        adapter_id: "kube-a-b".into(),
                        adapter_kind: AdapterKind::Kubernetes,
                        policy: WorkloadFailoverPolicy::FollowVolume,
                        source_replicas: 1,
                        target_replicas: 0,
                    }],
                },
            },
        };
        state.apply(1, 4, &put).unwrap();
        assert_eq!(state.failover.revision, 1);
        assert_eq!(
            state.failover.volumes["postgres"].authority_region,
            "region-a"
        );

        let before = state.clone();
        let conflict = GlobalPolicyCommand::ApplyFailover {
            command: GlobalFailoverCommand::PutVolume {
                expected_revision: 0,
                spec: state.failover.volumes["postgres"].clone(),
            },
        };
        assert!(state.apply(2, 4, &conflict).is_err());
        assert_eq!(state, before);
    }
}
