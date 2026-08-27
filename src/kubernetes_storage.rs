//! Kubernetes storage intent types shared by the operator and CSI adapter.
//!
//! The CRDs describe placement intent. They never move mirror or stripe
//! decisions into the zcnblk kernel client: `/dev/zcnblk0` remains the client
//! edge and the reconciled fan process remains a distinct userspace stage.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const API_GROUP: &str = "storage.zcutils.io";
pub const API_VERSION: &str = "v1alpha1";
pub const VOLUME_FINALIZER: &str = "storage.zcutils.io/runtime-cleanup";
pub const CROSS_REGION_FINALIZER: &str = "storage.zcutils.io/cross-region-runtime-cleanup";
pub const MANAGED_BY_LABEL: &str = "storage.zcutils.io/managed-by";
pub const VOLUME_LABEL: &str = "storage.zcutils.io/volume";
pub const CROSS_REGION_LABEL: &str = "storage.zcutils.io/cross-region-replication";
pub const STAGE_LABEL: &str = "storage.zcutils.io/stage";

fn default_true() -> bool {
    true
}

fn default_two() -> u16 {
    2
}

fn default_chunk_bytes() -> u32 {
    4096
}

fn default_lanes() -> u16 {
    1
}

fn default_connections_per_lane() -> u16 {
    1
}

fn default_address_source() -> String {
    "NodeAnnotationThenInternalIP".to_string()
}

fn default_backplane_annotation() -> String {
    "storage.zcutils.io/backplane-address".to_string()
}

fn default_tcp_mux() -> String {
    "TcpMux".to_string()
}

fn default_ofi_provider() -> String {
    "efa".to_string()
}

fn default_ofi_endpoint() -> String {
    "rdm".to_string()
}

fn default_ofi_domain_annotation() -> String {
    "storage.zcutils.io/ofi-domains".to_string()
}

fn default_linux_block() -> String {
    "LinuxBlock".to_string()
}

fn default_mirror() -> String {
    "Mirror".to_string()
}

fn default_userspace() -> String {
    "Userspace".to_string()
}

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[kube(
    group = "storage.zcutils.io",
    version = "v1alpha1",
    kind = "StorageProfile",
    plural = "storageprofiles",
    shortname = "zcsp",
    status = "StorageProfileStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct StorageProfileSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub storage_class: Option<PublishedStorageClass>,
    #[serde(default = "default_linux_block")]
    pub frontend: String,
    pub placement: PlacementSpec,
    #[serde(default)]
    pub tiering_policy_ref: Option<String>,
    #[serde(default)]
    pub transport: BackplaneTransportSpec,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublishedStorageClass {
    pub name: String,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlacementSpec {
    #[serde(default = "default_mirror")]
    pub primitive: String,
    #[serde(default = "default_userspace")]
    pub execution: String,
    #[serde(default = "default_two")]
    pub copies: u16,
    pub media_class: String,
    #[serde(default = "default_true")]
    pub exclude_client_node: bool,
    #[serde(default)]
    pub distinct_topology_keys: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackplaneTransportSpec {
    #[serde(default = "default_tcp_mux")]
    pub kind: String,
    #[serde(default = "default_address_source")]
    pub address_source: String,
    #[serde(default = "default_backplane_annotation")]
    pub node_address_annotation: String,
    #[serde(default = "default_lanes")]
    pub lanes: u16,
    #[serde(default = "default_connections_per_lane")]
    pub connections_per_lane: u16,
    #[serde(default = "default_chunk_bytes")]
    pub chunk_bytes: u32,
    /// Libfabric provider used when kind=OfiRdm (for example efa, verbs, or
    /// sockets for a non-representative software smoke).
    #[serde(default = "default_ofi_provider")]
    pub ofi_provider: String,
    #[serde(default = "default_ofi_endpoint")]
    pub ofi_endpoint: String,
    /// Optional node annotation whose comma-separated values map lanes to
    /// libfabric domains. Absence means provider-selected locality.
    #[serde(default = "default_ofi_domain_annotation")]
    pub ofi_domain_annotation: String,
    /// Kubernetes extended resource handed out by the installed RDMA device
    /// plugin, such as vpc.amazonaws.com/efa. OfiRdm profiles require it so
    /// the operator can prove every selected node owns a usable device.
    #[serde(default)]
    pub device_resource_name: Option<String>,
    /// Reserved for the one-sided registered-arena fast path. The current
    /// mirror rejects true rather than allowing a payload to bypass a leg.
    #[serde(default)]
    pub require_one_sided_rma: bool,
}

impl Default for BackplaneTransportSpec {
    fn default() -> Self {
        Self {
            kind: default_tcp_mux(),
            address_source: default_address_source(),
            node_address_annotation: default_backplane_annotation(),
            lanes: default_lanes(),
            connections_per_lane: default_connections_per_lane(),
            chunk_bytes: default_chunk_bytes(),
            ofi_provider: default_ofi_provider(),
            ofi_endpoint: default_ofi_endpoint(),
            ofi_domain_annotation: default_ofi_domain_annotation(),
            device_resource_name: None,
            require_one_sided_rma: false,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageProfileStatus {
    #[serde(default)]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub storage_class_name: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[kube(
    group = "storage.zcutils.io",
    version = "v1alpha1",
    kind = "MediaGrant",
    plural = "mediagrants",
    shortname = "zcmg",
    status = "MediaGrantStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct MediaGrantSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub node_selector: NodeSelector,
    pub media_sets: Vec<MediaSet>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeSelector {
    #[serde(default)]
    pub match_labels: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaSet {
    pub name: String,
    pub publish_as: PublishedMedia,
    #[serde(default)]
    pub dynamic_sources: Vec<DynamicMediaSource>,
    #[serde(default)]
    pub static_sources: Vec<StaticMediaSource>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublishedMedia {
    pub media_class: String,
    pub durability: String,
    #[serde(default)]
    pub may_contribute_to_durable_acknowledgement: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DynamicMediaSource {
    pub kind: String,
    pub maximum_volume_size: String,
    /// Optional aggregate byte budget contributed by every matching node.
    /// Omitting it preserves the legacy unbounded-count behavior; production
    /// profiles should declare it so admission can fail closed.
    #[serde(default)]
    pub total_capacity_per_node: Option<String>,
    /// Optional aggregate provisioned-IOPS budget contributed by every
    /// matching node. Burst IOPS are deliberately not charged here.
    #[serde(default)]
    pub total_provisioned_iops_per_node: Option<u64>,
    #[serde(default)]
    pub huge_page_size: Option<String>,
    #[serde(default)]
    pub pinned: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StaticMediaSource {
    pub kind: String,
    pub node_name: String,
    pub part_uuid: String,
    #[serde(default)]
    pub destructive_preparation: DestructivePreparation,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DestructivePreparation {
    #[serde(default)]
    pub allow_raw_writes: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaGrantStatus {
    #[serde(default)]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub eligible_nodes: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
}

fn default_memory_empty_dir() -> String {
    "MemoryEmptyDir".to_string()
}

fn default_host_path_file() -> String {
    "HostPathFile".to_string()
}

fn default_retain() -> String {
    "Retain".to_string()
}

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[kube(
    group = "storage.zcutils.io",
    version = "v1alpha1",
    kind = "TieringPolicy",
    plural = "tieringpolicies",
    shortname = "zctp",
    status = "TieringPolicyStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct TieringPolicySpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub hot: HotTierSpec,
    pub spill: SpillTierSpec,
    pub backpressure_bytes: String,
    #[serde(default = "default_true")]
    pub rehydrate_on_cold_start: bool,
    #[serde(default = "default_retain")]
    pub reclaim_policy: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HotTierSpec {
    #[serde(default = "default_memory_empty_dir")]
    pub kind: String,
    pub maximum_volume_size: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpillTierSpec {
    #[serde(default = "default_host_path_file")]
    pub kind: String,
    pub root_path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TieringPolicyStatus {
    #[serde(default)]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub message: Option<String>,
}

fn default_async_checkpoint() -> String {
    "AsynchronousCheckpoint".to_string()
}

fn default_encrypted_tcp() -> String {
    "Aes256AuthenticatedTcp".to_string()
}

fn default_secret_key() -> String {
    "token".to_string()
}

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[kube(
    group = "storage.zcutils.io",
    version = "v1alpha1",
    kind = "CrossRegionReplication",
    plural = "crossregionreplications",
    shortname = "zcxr",
    namespaced,
    status = "CrossRegionReplicationStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct CrossRegionReplicationSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_async_checkpoint")]
    pub mode: String,
    pub source: RegionFileEndpoint,
    pub target: RegionFileEndpoint,
    pub bytes: u64,
    pub credential_secret_ref: CredentialSecretReference,
    #[serde(default)]
    pub transport: CrossRegionTransportSpec,
    #[serde(default)]
    pub allow_target_overwrite: bool,
    #[serde(default)]
    pub automatic_failover: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegionFileEndpoint {
    pub region: String,
    pub node_name: String,
    pub path: String,
    #[serde(default)]
    pub address: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CredentialSecretReference {
    pub name: String,
    #[serde(default = "default_secret_key")]
    pub key: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CrossRegionTransportSpec {
    #[serde(default = "default_encrypted_tcp")]
    pub kind: String,
    #[serde(default = "default_backplane_annotation")]
    pub node_address_annotation: String,
}

impl Default for CrossRegionTransportSpec {
    fn default() -> Self {
        Self {
            kind: default_encrypted_tcp(),
            node_address_annotation: default_backplane_annotation(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CrossRegionReplicationStatus {
    #[serde(default)]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub sender_pod: Option<String>,
    #[serde(default)]
    pub receiver_pod: Option<String>,
    #[serde(default)]
    pub accepted_hwm: u64,
    #[serde(default)]
    pub remote_durable_hwm: u64,
    #[serde(default)]
    pub remote_applied_hwm: u64,
    #[serde(default)]
    pub transport: Option<String>,
}

#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[kube(
    group = "storage.zcutils.io",
    version = "v1alpha1",
    kind = "ZcVolume",
    plural = "zcvolumes",
    shortname = "zcv",
    namespaced,
    status = "ZcVolumeStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct ZcVolumeSpec {
    pub profile_ref: String,
    pub capacity_bytes: u64,
    /// Hard IOPS reservation used by control-plane admission. Zero means the
    /// caller requested byte-capacity admission only.
    #[serde(default)]
    pub provisioned_iops: u64,
    pub client_node: String,
    #[serde(default = "default_linux_block")]
    pub frontend: String,
    #[serde(default)]
    pub claim_ref: Option<ObjectReference>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ObjectReference {
    pub namespace: String,
    pub name: String,
    #[serde(default)]
    pub uid: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ZcVolumeStatus {
    #[serde(default)]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub runtime: Option<VolumeRuntimeStatus>,
    #[serde(default)]
    pub conditions: Vec<ZcCondition>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VolumeRuntimeStatus {
    pub placement: String,
    pub fan_node: String,
    pub fan_address: String,
    pub fan_port: u16,
    pub transport: String,
    pub leaves: Vec<LeafRuntimeStatus>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeafRuntimeStatus {
    pub node_name: String,
    pub address: String,
    pub port: u16,
    pub media_class: String,
    pub source_kind: String,
    #[serde(default)]
    pub part_uuid: Option<String>,
    #[serde(default)]
    pub tier: Option<TierRuntimeStatus>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TierRuntimeStatus {
    pub policy_name: String,
    pub hot_kind: String,
    pub spill_kind: String,
    pub spill_path: String,
    pub backpressure_bytes: u64,
    pub acknowledgement: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ZcCondition {
    pub r#type: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    pub observed_generation: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn generated_crds_are_structural() {
        for crd in [
            StorageProfile::crd(),
            MediaGrant::crd(),
            TieringPolicy::crd(),
            CrossRegionReplication::crd(),
            ZcVolume::crd(),
        ] {
            let versions = crd.spec.versions;
            assert_eq!(versions.len(), 1);
            assert!(versions[0].schema.is_some());
        }
    }

    #[test]
    fn getting_started_storage_intent_deserializes_against_the_rust_types() {
        for source in [
            include_str!("../zccusan/deploy/zcblock-csi/getting-started/mirror-ram.yaml"),
            include_str!(
                "../zccusan/deploy/zcblock-csi/getting-started/mirror-block.template.yaml"
            ),
            include_str!("../zccusan/deploy/zcblock-csi/getting-started/mirror-rdma.template.yaml"),
        ] {
            let documents = serde_yaml::Deserializer::from_str(source);
            let mut kinds = Vec::new();
            for document in documents {
                let value = serde_yaml::Value::deserialize(document).unwrap();
                let kind = value
                    .get("kind")
                    .and_then(serde_yaml::Value::as_str)
                    .unwrap()
                    .to_string();
                match kind.as_str() {
                    "MediaGrant" => {
                        serde_yaml::from_value::<MediaGrant>(value).unwrap();
                    }
                    "StorageProfile" => {
                        serde_yaml::from_value::<StorageProfile>(value).unwrap();
                    }
                    other => panic!("unexpected storage intent kind {other}"),
                }
                kinds.push(kind);
            }
            assert_eq!(kinds, ["MediaGrant", "StorageProfile"]);
        }
    }

    #[test]
    fn tier_and_cross_region_examples_deserialize_against_the_rust_types() {
        let tier =
            include_str!("../zccusan/deploy/zcblock-csi/getting-started/tiered-mirror-ram.yaml");
        let tier_kinds = serde_yaml::Deserializer::from_str(tier)
            .map(|document| {
                let value = serde_yaml::Value::deserialize(document).unwrap();
                let kind = value["kind"].as_str().unwrap().to_string();
                match kind.as_str() {
                    "TieringPolicy" => {
                        serde_yaml::from_value::<TieringPolicy>(value).unwrap();
                    }
                    "MediaGrant" => {
                        serde_yaml::from_value::<MediaGrant>(value).unwrap();
                    }
                    "StorageProfile" => {
                        serde_yaml::from_value::<StorageProfile>(value).unwrap();
                    }
                    other => panic!("unexpected tier intent kind {other}"),
                }
                kind
            })
            .collect::<Vec<_>>();
        assert_eq!(
            tier_kinds,
            ["TieringPolicy", "MediaGrant", "StorageProfile"]
        );

        let cross = include_str!(
            "../zccusan/deploy/zcblock-csi/getting-started/cross-region-checkpoint.template.yaml"
        );
        let value = serde_yaml::Deserializer::from_str(cross)
            .nth(1)
            .map(|document| serde_yaml::Value::deserialize(document).unwrap())
            .unwrap();
        serde_yaml::from_value::<CrossRegionReplication>(value).unwrap();
    }
}
