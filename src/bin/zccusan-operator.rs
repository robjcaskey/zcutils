use futures::StreamExt;
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::api::storage::v1::StorageClass;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::runtime::{Controller, controller::Action, watcher};
use kube::{Api, Client, Resource, ResourceExt};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use zcutils::kubernetes_storage::{
    API_GROUP, API_VERSION, CROSS_REGION_FINALIZER, CROSS_REGION_LABEL, CrossRegionReplication,
    CrossRegionReplicationStatus, MANAGED_BY_LABEL, MediaGrant, MediaGrantStatus, MediaSet,
    STAGE_LABEL, StaticMediaSource, StorageProfile, StorageProfileStatus, TierRuntimeStatus,
    TieringPolicy, TieringPolicyStatus, VOLUME_FINALIZER, VOLUME_LABEL, VolumeRuntimeStatus,
    ZcCondition, ZcVolume, ZcVolumeStatus,
};

const FIELD_MANAGER: &str = "zccusan-operator";

#[derive(Clone)]
struct Context {
    client: Client,
    image: String,
    image_pull_policy: String,
    csi_provisioner: String,
}

#[derive(Debug)]
struct OperatorError(String);

impl fmt::Display for OperatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for OperatorError {}

impl From<kube::Error> for OperatorError {
    fn from(value: kube::Error) -> Self {
        Self(value.to_string())
    }
}

fn fail(message: impl Into<String>) -> OperatorError {
    OperatorError(message.into())
}

#[derive(Clone)]
enum CandidateSource {
    Memory,
    Block(StaticMediaSource),
}

#[derive(Clone)]
struct Candidate {
    node: Node,
    address: String,
    media_class: String,
    durability: String,
    source: CandidateSource,
}

impl Candidate {
    fn node_name(&self) -> String {
        self.node.name_any()
    }
}

fn generation<T: ResourceExt>(resource: &T) -> i64 {
    resource.meta().generation.unwrap_or(0)
}

fn owned_by_profile(storage_class: &StorageClass, profile: &StorageProfile) -> bool {
    let Some(uid) = profile.meta().uid.as_deref() else {
        return false;
    };
    storage_class
        .metadata
        .owner_references
        .as_ref()
        .is_some_and(|references| {
            references.iter().any(|reference| {
                reference.uid == uid
                    && reference.kind == "StorageProfile"
                    && reference.api_version == format!("{API_GROUP}/{API_VERSION}")
            })
        })
}

async fn prune_owned_storage_classes(
    storage_classes: &Api<StorageClass>,
    profile: &StorageProfile,
    retain: Option<&str>,
) -> Result<(), OperatorError> {
    for storage_class in storage_classes.list(&ListParams::default()).await? {
        if owned_by_profile(&storage_class, profile)
            && retain != Some(storage_class.name_any().as_str())
        {
            match storage_classes
                .delete(&storage_class.name_any(), &DeleteParams::default())
                .await
            {
                Ok(_) => {}
                Err(kube::Error::Api(response)) if response.code == 404 => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

fn selector_matches(node: &Node, expected: &BTreeMap<String, String>) -> bool {
    let labels = node.metadata.labels.as_ref();
    expected
        .iter()
        .all(|(key, value)| labels.and_then(|labels| labels.get(key)) == Some(value))
}

fn node_address(node: &Node, profile: &StorageProfile) -> Result<String, OperatorError> {
    let transport = &profile.spec.transport;
    if !matches!(
        transport.address_source.as_str(),
        "NodeAnnotation" | "NodeAnnotationThenInternalIP" | "InternalIP"
    ) {
        return Err(fail(format!(
            "unsupported transport.addressSource {:?}",
            transport.address_source
        )));
    }
    if transport.address_source != "InternalIP"
        && let Some(value) = node
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(&transport.node_address_annotation))
        && !value.trim().is_empty()
    {
        return validate_backplane_address(value.trim());
    }
    if transport.address_source == "NodeAnnotation" {
        return Err(fail(format!(
            "node {} lacks required backplane address annotation {}",
            node.name_any(),
            transport.node_address_annotation
        )));
    }
    let address = node
        .status
        .as_ref()
        .and_then(|status| status.addresses.as_ref())
        .and_then(|addresses| {
            addresses
                .iter()
                .find(|address| address.type_ == "InternalIP")
        })
        .map(|address| address.address.as_str())
        .ok_or_else(|| fail(format!("node {} has no InternalIP", node.name_any())))?;
    validate_backplane_address(address)
}

fn validate_backplane_address(value: &str) -> Result<String, OperatorError> {
    let address = value
        .parse::<std::net::IpAddr>()
        .map_err(|_| fail(format!("backplane address {value:?} is not an IP address")))?;
    if !address.is_ipv4() {
        return Err(fail(
            "this first direct-backplane reconciler supports IPv4 endpoints only",
        ));
    }
    Ok(address.to_string())
}

fn parse_bytes(value: &str) -> Result<u64, OperatorError> {
    let value = value.trim();
    let digit_end = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    if digit_end == 0 {
        return Err(fail(format!("invalid byte quantity {value:?}")));
    }
    let number = value[..digit_end]
        .parse::<u64>()
        .map_err(|_| fail(format!("invalid byte quantity {value:?}")))?;
    let multiplier = match &value[digit_end..] {
        "" | "B" => 1,
        "Ki" | "KiB" => 1024,
        "Mi" | "MiB" => 1024u64.pow(2),
        "Gi" | "GiB" => 1024u64.pow(3),
        "Ti" | "TiB" => 1024u64.pow(4),
        "K" | "KB" => 1000,
        "M" | "MB" => 1000u64.pow(2),
        "G" | "GB" => 1000u64.pow(3),
        "T" | "TB" => 1000u64.pow(4),
        suffix => return Err(fail(format!("unsupported byte quantity suffix {suffix:?}"))),
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| fail(format!("byte quantity {value:?} overflows u64")))
}

fn validate_profile(profile: &StorageProfile) -> Result<(), OperatorError> {
    if profile.spec.frontend != "LinuxBlock" {
        return Err(fail(
            "this first Kubernetes reconciler supports frontend=LinuxBlock",
        ));
    }
    if profile.spec.placement.primitive != "Mirror"
        || profile.spec.placement.execution != "Userspace"
    {
        return Err(fail(
            "this first reconciler requires placement.primitive=Mirror and execution=Userspace",
        ));
    }
    if profile.spec.placement.copies != 2 {
        return Err(fail(
            "this first userspace mirror reconciler requires placement.copies=2",
        ));
    }
    if profile.spec.placement.media_class.trim().is_empty() {
        return Err(fail("placement.mediaClass must not be empty"));
    }
    let transport = &profile.spec.transport;
    if transport.kind != "TcpMux" {
        return Err(fail(
            "this first Kubernetes runtime reconciler supports transport.kind=TcpMux",
        ));
    }
    if !matches!(
        transport.address_source.as_str(),
        "NodeAnnotation" | "NodeAnnotationThenInternalIP" | "InternalIP"
    ) {
        return Err(fail(format!(
            "unsupported transport.addressSource {:?}",
            transport.address_source
        )));
    }
    if transport.address_source != "InternalIP"
        && transport.node_address_annotation.trim().is_empty()
    {
        return Err(fail(
            "transport.nodeAddressAnnotation must not be empty for annotation address discovery",
        ));
    }
    if transport.lanes == 0 || transport.connections_per_lane == 0 {
        return Err(fail(
            "transport lanes and connectionsPerLane must both be greater than zero",
        ));
    }
    if transport.connections_per_lane != 1 {
        return Err(fail(
            "this first userspace WAL mirror reconciler requires transport.connectionsPerLane=1",
        ));
    }
    let workers = u32::from(transport.lanes) * u32::from(transport.connections_per_lane);
    if workers > 4096 {
        return Err(fail(
            "transport lanes multiplied by connectionsPerLane must not exceed 4096",
        ));
    }
    if transport.chunk_bytes < 512 || !transport.chunk_bytes.is_power_of_two() {
        return Err(fail(
            "transport.chunkBytes must be a power of two and at least 512",
        ));
    }
    if let Some(published) = &profile.spec.storage_class
        && published.name.trim().is_empty()
    {
        return Err(fail("storageClass.name must not be empty"));
    }
    Ok(())
}

fn validate_tiering_policy(policy: &TieringPolicy) -> Result<(), OperatorError> {
    if !policy.spec.enabled {
        return Err(fail(format!(
            "TieringPolicy {} is disabled",
            policy.name_any()
        )));
    }
    if policy.spec.hot.kind != "MemoryEmptyDir" {
        return Err(fail(
            "this first tier reconciler requires hot.kind=MemoryEmptyDir",
        ));
    }
    if parse_bytes(&policy.spec.hot.maximum_volume_size)? == 0 {
        return Err(fail("hot.maximumVolumeSize must be greater than zero"));
    }
    if policy.spec.spill.kind != "HostPathFile" {
        return Err(fail(
            "this first tier reconciler requires spill.kind=HostPathFile",
        ));
    }
    let spill_root = Path::new(&policy.spec.spill.root_path);
    if !spill_root.is_absolute()
        || policy.spec.spill.root_path.contains(':')
        || policy.spec.spill.root_path.contains("..")
    {
        return Err(fail(
            "spill.rootPath must be an absolute path without ':' or '..' components",
        ));
    }
    if parse_bytes(&policy.spec.backpressure_bytes)? == 0 {
        return Err(fail("backpressureBytes must be greater than zero"));
    }
    if !policy.spec.rehydrate_on_cold_start {
        return Err(fail(
            "rehydrateOnColdStart=false is not implemented; refusing a tier that could return zeroes after restart",
        ));
    }
    if policy.spec.reclaim_policy != "Retain" {
        return Err(fail(
            "this first tier reconciler requires reclaimPolicy=Retain",
        ));
    }
    Ok(())
}

async fn resolve_tiering_policy(
    ctx: &Context,
    profile: &StorageProfile,
    capacity_bytes: Option<u64>,
) -> Result<Option<TieringPolicy>, OperatorError> {
    let Some(name) = profile.spec.tiering_policy_ref.as_deref() else {
        return Ok(None);
    };
    if name.trim().is_empty() {
        return Err(fail("tieringPolicyRef must not be empty"));
    }
    let policies: Api<TieringPolicy> = Api::all(ctx.client.clone());
    let policy = policies.get(name).await?;
    validate_tiering_policy(&policy)?;
    if let Some(capacity_bytes) = capacity_bytes
        && capacity_bytes > parse_bytes(&policy.spec.hot.maximum_volume_size)?
    {
        return Err(fail(format!(
            "volume capacity {capacity_bytes} exceeds TieringPolicy {} hot.maximumVolumeSize {}",
            policy.name_any(),
            policy.spec.hot.maximum_volume_size
        )));
    }
    Ok(Some(policy))
}

fn media_set_matches<'a>(grant: &'a MediaGrant, media_class: &str) -> Vec<&'a MediaSet> {
    grant
        .spec
        .media_sets
        .iter()
        .filter(|set| set.publish_as.media_class == media_class)
        .collect()
}

fn topology_value(node: &Node, key: &str) -> Option<String> {
    if key == "kubernetes.io/hostname" {
        return Some(
            node.metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get(key))
                .cloned()
                .unwrap_or_else(|| node.name_any()),
        );
    }
    node.metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get(key))
        .cloned()
}

fn candidates_are_distinct(
    selected: &[Candidate],
    candidate: &Candidate,
    topology_keys: &[String],
) -> bool {
    topology_keys.iter().all(|key| {
        let Some(value) = topology_value(&candidate.node, key) else {
            return false;
        };
        selected
            .iter()
            .all(|existing| topology_value(&existing.node, key).as_deref() != Some(&value))
    })
}

async fn used_partuuids(
    client: Client,
    current_namespace: &str,
    current_name: &str,
) -> Result<BTreeSet<String>, OperatorError> {
    let volumes: Api<ZcVolume> = Api::all(client);
    let mut used = BTreeSet::new();
    for volume in volumes.list(&ListParams::default()).await? {
        if volume.namespace().as_deref() == Some(current_namespace)
            && volume.name_any() == current_name
        {
            continue;
        }
        if let Some(runtime) = volume.status.and_then(|status| status.runtime) {
            used.extend(runtime.leaves.into_iter().filter_map(|leaf| leaf.part_uuid));
        }
    }
    Ok(used)
}

fn normalized_partuuid(value: &str) -> Result<String, OperatorError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || value.len() > 48
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'-'))
    {
        return Err(fail(format!("invalid PARTUUID syntax {value:?}")));
    }
    Ok(value)
}

fn reservation_name(part_uuid: &str) -> Result<String, OperatorError> {
    Ok(format!("zccusan-media-{}", normalized_partuuid(part_uuid)?))
}

async fn acquire_static_reservations(
    ctx: &Context,
    volume: &ZcVolume,
    candidates: &[Candidate],
) -> Result<(), OperatorError> {
    let namespace = volume
        .namespace()
        .ok_or_else(|| fail("ZcVolume is missing namespace"))?;
    let holder = volume
        .uid()
        .ok_or_else(|| fail("ZcVolume has no UID for media reservation"))?;
    let leases: Api<Lease> = Api::namespaced(ctx.client.clone(), &namespace);
    for candidate in candidates {
        let CandidateSource::Block(source) = &candidate.source else {
            continue;
        };
        let name = reservation_name(&source.part_uuid)?;
        let body = json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {
                "name": name,
                "namespace": namespace,
                "labels": runtime_labels(volume, "media-reservation"),
                "ownerReferences": [owner_reference(volume)?]
            },
            "spec": { "holderIdentity": holder }
        });
        let desired: Lease = serde_json::from_value(body)
            .map_err(|error| fail(format!("build media reservation: {error}")))?;
        match leases.create(&PostParams::default(), &desired).await {
            Ok(_) => {}
            Err(kube::Error::Api(response)) if response.code == 409 => {
                let existing = leases.get(&name).await?;
                let existing_holder = existing.spec.and_then(|spec| spec.holder_identity);
                if existing_holder.as_deref() != Some(holder.as_str()) {
                    return Err(fail(format!(
                        "PARTUUID={} is already reserved by another ZcVolume",
                        source.part_uuid
                    )));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

async fn select_candidates(
    ctx: &Context,
    volume: &ZcVolume,
    profile: &StorageProfile,
) -> Result<Vec<Candidate>, OperatorError> {
    let grants: Api<MediaGrant> = Api::all(ctx.client.clone());
    let nodes: Api<Node> = Api::all(ctx.client.clone());
    let grants = grants.list(&ListParams::default()).await?;
    let nodes = nodes.list(&ListParams::default()).await?;
    let node_map = nodes
        .into_iter()
        .map(|node| (node.name_any(), node))
        .collect::<BTreeMap<_, _>>();
    let namespace = volume
        .namespace()
        .ok_or_else(|| fail("ZcVolume is missing namespace"))?;
    let used_partuuids = used_partuuids(ctx.client.clone(), &namespace, &volume.name_any()).await?;
    let mut candidates = Vec::new();

    for grant in grants {
        if !grant.spec.enabled {
            continue;
        }
        for media_set in media_set_matches(&grant, &profile.spec.placement.media_class) {
            for source in &media_set.dynamic_sources {
                if source.kind != "MemoryArena" {
                    continue;
                }
                if source.huge_page_size.is_some() || source.pinned {
                    return Err(fail(format!(
                        "MediaGrant {} requests hugePageSize or pinned memory; this first reconciler does not yet translate those requests into Pod hugepage and memlock resources",
                        grant.name_any()
                    )));
                }
                if volume.spec.capacity_bytes > parse_bytes(&source.maximum_volume_size)? {
                    continue;
                }
                for node in node_map.values() {
                    if selector_matches(node, &grant.spec.node_selector.match_labels) {
                        candidates.push(Candidate {
                            node: node.clone(),
                            address: node_address(node, profile)?,
                            media_class: media_set.publish_as.media_class.clone(),
                            durability: media_set.publish_as.durability.clone(),
                            source: CandidateSource::Memory,
                        });
                    }
                }
            }
            for source in &media_set.static_sources {
                if source.kind != "BlockDevice" {
                    continue;
                }
                if !source.destructive_preparation.allow_raw_writes {
                    return Err(fail(format!(
                        "MediaGrant {} source PARTUUID={} selected for raw terminal writes without destructivePreparation.allowRawWrites=true",
                        grant.name_any(),
                        source.part_uuid
                    )));
                }
                if used_partuuids.contains(&source.part_uuid) {
                    continue;
                }
                let Some(node) = node_map.get(&source.node_name) else {
                    continue;
                };
                if selector_matches(node, &grant.spec.node_selector.match_labels) {
                    candidates.push(Candidate {
                        node: node.clone(),
                        address: node_address(node, profile)?,
                        media_class: media_set.publish_as.media_class.clone(),
                        durability: media_set.publish_as.durability.clone(),
                        source: CandidateSource::Block(source.clone()),
                    });
                }
            }
        }
    }

    candidates.sort_by_key(|candidate| {
        let source = match &candidate.source {
            CandidateSource::Memory => String::new(),
            CandidateSource::Block(source) => source.part_uuid.clone(),
        };
        (candidate.node_name(), source)
    });
    candidates.dedup_by(|left, right| left.node_name() == right.node_name());

    let mut selected = Vec::new();
    for candidate in candidates {
        if profile.spec.placement.exclude_client_node
            && candidate.node_name() == volume.spec.client_node
        {
            continue;
        }
        if candidates_are_distinct(
            &selected,
            &candidate,
            &profile.spec.placement.distinct_topology_keys,
        ) {
            selected.push(candidate);
        }
        if selected.len() == usize::from(profile.spec.placement.copies) {
            break;
        }
    }
    if selected.len() != usize::from(profile.spec.placement.copies) {
        return Err(fail(format!(
            "profile {} needs {} eligible leaves in distinct {:?} domains, found {}",
            profile.name_any(),
            profile.spec.placement.copies,
            profile.spec.placement.distinct_topology_keys,
            selected.len()
        )));
    }
    Ok(selected)
}

fn stable_port(name: &str, base: u16) -> u16 {
    let hash = name.as_bytes().iter().fold(2_166_136_261u32, |hash, byte| {
        hash.wrapping_mul(16_777_619) ^ u32::from(*byte)
    });
    base + u16::try_from(hash % 2000).expect("hash modulo fits u16")
}

fn owner_reference(volume: &ZcVolume) -> Result<Value, OperatorError> {
    Ok(json!({
        "apiVersion": format!("{API_GROUP}/{API_VERSION}"),
        "kind": "ZcVolume",
        "name": volume.name_any(),
        "uid": volume.meta().uid.clone().ok_or_else(|| fail("ZcVolume has no UID"))?,
        "controller": true,
        "blockOwnerDeletion": true
    }))
}

fn object_name(volume: &ZcVolume, suffix: &str) -> String {
    let prefix = volume.name_any();
    let available = 63usize.saturating_sub(suffix.len() + 1);
    format!("{}-{suffix}", &prefix[..prefix.len().min(available)])
}

fn runtime_labels(volume: &ZcVolume, stage: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (MANAGED_BY_LABEL.to_string(), FIELD_MANAGER.to_string()),
        (VOLUME_LABEL.to_string(), volume.name_any()),
        (STAGE_LABEL.to_string(), stage.to_string()),
    ])
}

async fn apply_raw_allowlist(
    ctx: &Context,
    volume: &ZcVolume,
    leaf_index: usize,
    part_uuid: &str,
) -> Result<String, OperatorError> {
    let namespace = volume
        .namespace()
        .ok_or_else(|| fail("ZcVolume is missing namespace"))?;
    let name = object_name(volume, &format!("leaf-{leaf_index}-raw"));
    let api: Api<k8s_openapi::api::core::v1::ConfigMap> =
        Api::namespaced(ctx.client.clone(), &namespace);
    let body = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": runtime_labels(volume, "terminal-leaf-config"),
            "ownerReferences": [owner_reference(volume)?]
        },
        "data": { "allowed-raw-partitions.txt": format!("PARTUUID={part_uuid}\n") }
    });
    api.patch(
        &name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&body),
    )
    .await?;
    Ok(name)
}

async fn apply_leaf_pod(
    ctx: &Context,
    volume: &ZcVolume,
    candidate: &Candidate,
    leaf_index: usize,
    port: u16,
    profile: &StorageProfile,
    tiering: Option<&TieringPolicy>,
) -> Result<String, OperatorError> {
    let namespace = volume
        .namespace()
        .ok_or_else(|| fail("ZcVolume is missing namespace"))?;
    let name = object_name(volume, &format!("leaf-{leaf_index}"));
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let (target, privileged, mut envs, volumes, mounts) = match &candidate.source {
        CandidateSource::Memory if tiering.is_some() => {
            let policy = tiering.expect("guarded by match");
            let hot_path = format!("/var/lib/zcutils/tier-hot/{}.hot", volume.name_any());
            let spill_path = format!("/var/lib/zcutils/tier-spill/{}.spill", volume.name_any());
            let backpressure = parse_bytes(&policy.spec.backpressure_bytes)?;
            (
                format!(
                    "zctier:{hot_path}:{spill_path}:{}:{}:{backpressure}",
                    volume.spec.capacity_bytes, profile.spec.transport.chunk_bytes
                ),
                false,
                vec![json!({
                    "name": "URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC",
                    "value": "1"
                })],
                vec![
                    json!({"name": "tier-hot", "emptyDir": {
                        "medium": "Memory",
                        "sizeLimit": policy.spec.hot.maximum_volume_size
                    }}),
                    json!({"name": "tier-spill", "hostPath": {
                        "path": policy.spec.spill.root_path,
                        "type": "DirectoryOrCreate"
                    }}),
                ],
                vec![
                    json!({"name": "tier-hot", "mountPath": "/var/lib/zcutils/tier-hot"}),
                    json!({"name": "tier-spill", "mountPath": "/var/lib/zcutils/tier-spill"}),
                ],
            )
        }
        CandidateSource::Memory => (
            format!("zcmem:{}", volume.spec.capacity_bytes),
            false,
            vec![json!({
                "name": "URING_PLAY_ZCNBLK_WAL_LEAF_ALLOW_VOLATILE_SYNC",
                "value": "1"
            })],
            Vec::<Value>::new(),
            Vec::<Value>::new(),
        ),
        CandidateSource::Block(source) => {
            if tiering.is_some() {
                return Err(fail(
                    "tieringPolicyRef currently wraps MemoryArena leaves only; a block device remains terminal media and is never used as a placement primitive",
                ));
            }
            let config_name =
                apply_raw_allowlist(ctx, volume, leaf_index, &source.part_uuid).await?;
            (
                format!("PARTUUID={}", source.part_uuid),
                true,
                vec![
                    json!({"name": "URING_PLAY_RAW_PARTITION_ALLOWLIST", "value": "/etc/zcutils/raw/allowed-raw-partitions.txt"}),
                    json!({"name": "URING_PLAY_ALLOW_RAW_BLOCK_WRITE", "value": "1"}),
                    json!({"name": "URING_PLAY_RAW_TARGET_PARTUUID", "value": source.part_uuid}),
                ],
                vec![
                    json!({"name": "dev", "hostPath": {"path": "/dev", "type": "Directory"}}),
                    json!({"name": "raw-allowlist", "configMap": {"name": config_name}}),
                ],
                vec![
                    json!({"name": "dev", "mountPath": "/dev"}),
                    json!({"name": "raw-allowlist", "mountPath": "/etc/zcutils/raw", "readOnly": true}),
                ],
            )
        }
    };
    envs.extend([
        json!({"name": "URING_PLAY_ZCNBLK_WAL_RESULT_RANGES", "value": "1"}),
        json!({"name": "URING_PLAY_TOPOLOGY_STRICT", "value": "0"}),
    ]);
    let workers = profile
        .spec
        .transport
        .lanes
        .saturating_mul(profile.spec.transport.connections_per_lane)
        .max(1);
    let body = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": runtime_labels(volume, "terminal-leaf"),
            "annotations": {
                "storage.zcutils.io/backplane-address": candidate.address,
                "storage.zcutils.io/media-class": candidate.media_class,
                "storage.zcutils.io/durability": candidate.durability,
                "storage.zcutils.io/tiering-policy": tiering.map(ResourceExt::name_any).unwrap_or_else(|| "none".to_string()),
                "storage.zcutils.io/tier-acknowledgement": if tiering.is_some() { "hot-only-spill-asynchronous" } else { "not-tiered" }
            },
            "ownerReferences": [owner_reference(volume)?]
        },
        "spec": {
            "nodeName": candidate.node_name(),
            "hostNetwork": true,
            "dnsPolicy": "ClusterFirstWithHostNet",
            "restartPolicy": "Always",
            "terminationGracePeriodSeconds": 5,
            "containers": [{
                "name": "leaf",
                "image": ctx.image,
                "imagePullPolicy": ctx.image_pull_policy,
                "command": ["/usr/local/bin/zcnblk-wal-leaf"],
                "args": [
                    target, candidate.address, port.to_string(),
                    profile.spec.transport.lanes.to_string(),
                    profile.spec.transport.connections_per_lane.to_string(),
                    profile.spec.transport.chunk_bytes.to_string(), workers.to_string(),
                    "false", "blocking"
                ],
                "env": envs,
                "ports": [{"name": "wal", "containerPort": port, "hostPort": port, "protocol": "TCP"}],
                "securityContext": {
                    "privileged": privileged,
                    "allowPrivilegeEscalation": privileged,
                    "readOnlyRootFilesystem": true,
                    "capabilities": if privileged { json!({}) } else { json!({"drop": ["ALL"]}) }
                },
                "volumeMounts": mounts
            }],
            "volumes": volumes
        }
    });
    pods.patch(
        &name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&body),
    )
    .await?;
    Ok(name)
}

async fn apply_fan_pod(
    ctx: &Context,
    volume: &ZcVolume,
    candidates: &[Candidate],
    fan_address: &str,
    fan_port: u16,
    leaf_port: u16,
    profile: &StorageProfile,
) -> Result<String, OperatorError> {
    let namespace = volume
        .namespace()
        .ok_or_else(|| fail("ZcVolume is missing namespace"))?;
    let name = object_name(volume, "fan");
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let [primary, secondary] = candidates else {
        return Err(fail(
            "the userspace WAL mirror runtime requires exactly two terminal leaves",
        ));
    };
    let control_port = stable_port(&volume.name_any(), 31_000);
    let body = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": runtime_labels(volume, "userspace-wal-mirror"),
            "annotations": {
                "storage.zcutils.io/data-path": "direct-backplane-no-clusterip",
                "storage.zcutils.io/placement-owner": "userspace-wal-failover",
                "storage.zcutils.io/write-policy": "synchronous-two-copy-mirror",
                "storage.zcutils.io/native-transport-security": "plaintext-preview-requires-external-encrypted-network"
            },
            "ownerReferences": [owner_reference(volume)?]
        },
        "spec": {
            "nodeName": volume.spec.client_node,
            "hostNetwork": true,
            "dnsPolicy": "ClusterFirstWithHostNet",
            "restartPolicy": "Always",
            "terminationGracePeriodSeconds": 5,
            "containers": [{
                "name": "mirror",
                "image": ctx.image,
                "imagePullPolicy": ctx.image_pull_policy,
                "command": ["/usr/local/bin/zcnblk-wal-failover"],
                "args": [
                    format!("{fan_address}:{fan_port}"),
                    format!("{}:{leaf_port}", primary.address),
                    format!("{}:{leaf_port}", secondary.address),
                    format!("{fan_address}:{control_port}"),
                    profile.spec.transport.lanes.to_string()
                ],
                "env": [{"name": "ZCNBLK_WAL_FAILOVER_MODE", "value": "sync"}],
                "ports": [
                    {"name": "wal", "containerPort": fan_port, "hostPort": fan_port, "protocol": "TCP"},
                    {"name": "control", "containerPort": control_port, "hostPort": control_port, "protocol": "TCP"}
                ],
                "securityContext": {
                    "privileged": false,
                    "allowPrivilegeEscalation": false,
                    "readOnlyRootFilesystem": true,
                    "capabilities": {"drop": ["ALL"]}
                }
            }]
        }
    });
    pods.patch(
        &name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&body),
    )
    .await?;
    Ok(name)
}

async fn apply_onramp_pod(
    ctx: &Context,
    volume: &ZcVolume,
    fan_address: &str,
    fan_port: u16,
    profile: &StorageProfile,
) -> Result<String, OperatorError> {
    let namespace = volume
        .namespace()
        .ok_or_else(|| fail("ZcVolume is missing namespace"))?;
    let name = object_name(volume, "onramp");
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let body = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": runtime_labels(volume, "client-userspace-onramp"),
            "annotations": {
                "storage.zcutils.io/kernel-role": "client-edge-only",
                "storage.zcutils.io/placement-owner": "userspace-wal-failover"
            },
            "ownerReferences": [owner_reference(volume)?]
        },
        "spec": {
            "nodeName": volume.spec.client_node,
            "hostNetwork": true,
            "dnsPolicy": "ClusterFirstWithHostNet",
            "restartPolicy": "Always",
            "terminationGracePeriodSeconds": 5,
            "containers": [{
                "name": "onramp",
                "image": ctx.image,
                "imagePullPolicy": ctx.image_pull_policy,
                "command": ["/usr/local/bin/zcnblk-shm-target"],
                "args": ["/dev/zcnblk-shmctl", "wal-tcp", "128"],
                "env": [
                    {"name": "URING_PLAY_ZCNBLK_SHM_REMOTE_TRANSPORT", "value": "tcp"},
                    {"name": "URING_PLAY_ZCNBLK_SHM_LEAF_ADDR", "value": format!("{fan_address}:{fan_port}")},
                    {"name": "URING_PLAY_ZCNBLK_SHM_WRITEBACK_BATCH", "value": "64"},
                    {"name": "URING_PLAY_ZCNBLK_SHM_TRANSFER_SLOTS", "value": profile.spec.transport.lanes.to_string()},
                    {"name": "URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_MODE", "value": "blocking"},
                    {"name": "URING_PLAY_ZCNBLK_SHM_REMOTE_SEND_ZC_REQUIRED", "value": "0"},
                    {"name": "URING_PLAY_ZCNBLK_SHM_REMOTE_CONNECT_RETRY_MS", "value": "60000"},
                    {"name": "URING_PLAY_TOPOLOGY_STRICT", "value": "0"}
                ],
                "securityContext": {
                    "privileged": true,
                    "allowPrivilegeEscalation": true,
                    "readOnlyRootFilesystem": true
                },
                "volumeMounts": [{"name": "dev", "mountPath": "/dev"}]
            }],
            "volumes": [{"name": "dev", "hostPath": {"path": "/dev", "type": "Directory"}}]
        }
    });
    pods.patch(
        &name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&body),
    )
    .await?;
    Ok(name)
}

async fn pods_running(
    client: Client,
    namespace: &str,
    names: &[String],
) -> Result<bool, OperatorError> {
    let pods: Api<Pod> = Api::namespaced(client, namespace);
    for name in names {
        let Some(pod) = pods.get_opt(name).await? else {
            return Ok(false);
        };
        let ready = pod
            .status
            .as_ref()
            .and_then(|status| status.container_statuses.as_ref())
            .is_some_and(|statuses| {
                !statuses.is_empty() && statuses.iter().all(|status| status.ready)
            });
        if !ready {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn patch_volume_status(
    api: &Api<ZcVolume>,
    volume: &ZcVolume,
    status: ZcVolumeStatus,
) -> Result<(), OperatorError> {
    api.patch_status(
        &volume.name_any(),
        &PatchParams::default(),
        &Patch::Merge(json!({"status": status})),
    )
    .await?;
    Ok(())
}

fn condition(volume: &ZcVolume, status: &str, reason: &str, message: &str) -> ZcCondition {
    ZcCondition {
        r#type: "Ready".to_string(),
        status: status.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        observed_generation: generation(volume),
    }
}

async fn ensure_finalizer(api: &Api<ZcVolume>, volume: &ZcVolume) -> Result<bool, OperatorError> {
    if volume
        .finalizers()
        .iter()
        .any(|value| value == VOLUME_FINALIZER)
    {
        return Ok(false);
    }
    let mut finalizers = volume.finalizers().to_vec();
    finalizers.push(VOLUME_FINALIZER.to_string());
    api.patch(
        &volume.name_any(),
        &PatchParams::default(),
        &Patch::Merge(json!({"metadata": {"finalizers": finalizers}})),
    )
    .await?;
    Ok(true)
}

async fn cleanup_volume(ctx: &Context, volume: &ZcVolume) -> Result<Action, OperatorError> {
    let namespace = volume
        .namespace()
        .ok_or_else(|| fail("ZcVolume is missing namespace"))?;
    let selector = format!("{VOLUME_LABEL}={}", volume.name_any());
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let configs: Api<k8s_openapi::api::core::v1::ConfigMap> =
        Api::namespaced(ctx.client.clone(), &namespace);
    let leases: Api<Lease> = Api::namespaced(ctx.client.clone(), &namespace);
    let list = ListParams::default().labels(&selector);
    pods.delete_collection(&DeleteParams::default(), &list)
        .await?;
    configs
        .delete_collection(&DeleteParams::default(), &list)
        .await?;
    leases
        .delete_collection(&DeleteParams::default(), &list)
        .await?;
    if !pods.list(&list).await?.items.is_empty() {
        return Ok(Action::requeue(Duration::from_millis(500)));
    }
    let volumes: Api<ZcVolume> = Api::namespaced(ctx.client.clone(), &namespace);
    let finalizers = volume
        .finalizers()
        .iter()
        .filter(|value| value.as_str() != VOLUME_FINALIZER)
        .cloned()
        .collect::<Vec<_>>();
    volumes
        .patch(
            &volume.name_any(),
            &PatchParams::default(),
            &Patch::Merge(json!({"metadata": {"finalizers": finalizers}})),
        )
        .await?;
    Ok(Action::await_change())
}

async fn ensure_client_edge_exclusive(
    ctx: &Context,
    volume: &ZcVolume,
) -> Result<(), OperatorError> {
    let volumes: Api<ZcVolume> = Api::all(ctx.client.clone());
    for other in volumes.list(&ListParams::default()).await? {
        if other.uid() == volume.uid() || other.meta().deletion_timestamp.is_some() {
            continue;
        }
        if other.spec.client_node == volume.spec.client_node
            && other
                .status
                .as_ref()
                .is_none_or(|status| status.phase != "Failed")
        {
            return Err(fail(format!(
                "client node {} already has volume {} attached to the single /dev/zcnblk0 edge",
                volume.spec.client_node,
                other.name_any()
            )));
        }
    }
    Ok(())
}

async fn reconcile_volume_inner(
    volume: Arc<ZcVolume>,
    ctx: Arc<Context>,
) -> Result<Action, OperatorError> {
    let namespace = volume
        .namespace()
        .ok_or_else(|| fail("ZcVolume is missing namespace"))?;
    let volumes: Api<ZcVolume> = Api::namespaced(ctx.client.clone(), &namespace);
    if volume.meta().deletion_timestamp.is_some() {
        return cleanup_volume(&ctx, &volume).await;
    }
    if ensure_finalizer(&volumes, &volume).await? {
        return Ok(Action::await_change());
    }
    ensure_client_edge_exclusive(&ctx, &volume).await?;
    let profiles: Api<StorageProfile> = Api::all(ctx.client.clone());
    let profile = profiles.get(&volume.spec.profile_ref).await?;
    if !profile.spec.enabled {
        return Err(fail(format!("profile {} is disabled", profile.name_any())));
    }
    validate_profile(&profile)?;
    let tiering = resolve_tiering_policy(&ctx, &profile, Some(volume.spec.capacity_bytes)).await?;
    let nodes: Api<Node> = Api::all(ctx.client.clone());
    let client_node = nodes.get(&volume.spec.client_node).await?;
    let fan_address = node_address(&client_node, &profile)?;
    let candidates = select_candidates(&ctx, &volume, &profile).await?;
    acquire_static_reservations(&ctx, &volume, &candidates).await?;
    let leaf_port = stable_port(&volume.name_any(), 26_000);
    let fan_port = stable_port(&volume.name_any(), 23_000);
    let mut leaf_names = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        leaf_names.push(
            apply_leaf_pod(
                &ctx,
                &volume,
                candidate,
                index,
                leaf_port,
                &profile,
                tiering.as_ref(),
            )
            .await?,
        );
    }
    if !pods_running(ctx.client.clone(), &namespace, &leaf_names).await? {
        patch_volume_status(
            &volumes,
            &volume,
            ZcVolumeStatus {
                observed_generation: Some(generation(&*volume)),
                phase: "ProvisioningLeaves".to_string(),
                message: Some("waiting for operator-owned terminal leaf Pods".to_string()),
                runtime: None,
                conditions: vec![condition(
                    &volume,
                    "False",
                    "LeavesStarting",
                    "terminal leaf Pods have not all started",
                )],
            },
        )
        .await?;
        return Ok(Action::requeue(Duration::from_secs(1)));
    }
    let fan_name = apply_fan_pod(
        &ctx,
        &volume,
        &candidates,
        &fan_address,
        fan_port,
        leaf_port,
        &profile,
    )
    .await?;
    if !pods_running(
        ctx.client.clone(),
        &namespace,
        std::slice::from_ref(&fan_name),
    )
    .await?
    {
        return Ok(Action::requeue(Duration::from_secs(1)));
    }
    let onramp_name = apply_onramp_pod(&ctx, &volume, &fan_address, fan_port, &profile).await?;
    if !pods_running(
        ctx.client.clone(),
        &namespace,
        std::slice::from_ref(&onramp_name),
    )
    .await?
    {
        return Ok(Action::requeue(Duration::from_secs(1)));
    }
    let runtime = VolumeRuntimeStatus {
        placement: "UserspaceMirror".to_string(),
        fan_node: volume.spec.client_node.clone(),
        fan_address: fan_address.clone(),
        fan_port,
        transport: "TcpMuxDirectBackplane".to_string(),
        leaves: candidates
            .iter()
            .map(|candidate| zcutils::kubernetes_storage::LeafRuntimeStatus {
                node_name: candidate.node_name(),
                address: candidate.address.clone(),
                port: leaf_port,
                media_class: candidate.media_class.clone(),
                source_kind: match candidate.source {
                    CandidateSource::Memory => "MemoryArena".to_string(),
                    CandidateSource::Block(_) => "BlockDevice".to_string(),
                },
                part_uuid: match &candidate.source {
                    CandidateSource::Memory => None,
                    CandidateSource::Block(source) => Some(source.part_uuid.clone()),
                },
                tier: tiering.as_ref().map(|policy| TierRuntimeStatus {
                    policy_name: policy.name_any(),
                    hot_kind: policy.spec.hot.kind.clone(),
                    spill_kind: policy.spec.spill.kind.clone(),
                    spill_path: format!(
                        "{}/{}.spill",
                        policy.spec.spill.root_path.trim_end_matches('/'),
                        volume.name_any()
                    ),
                    backpressure_bytes: parse_bytes(&policy.spec.backpressure_bytes)
                        .expect("validated tier backpressure"),
                    acknowledgement: "hot-only; spill asynchronous and excluded from durable HWM"
                        .to_string(),
                }),
            })
            .collect(),
    };
    patch_volume_status(
        &volumes,
        &volume,
        ZcVolumeStatus {
            observed_generation: Some(generation(&*volume)),
            phase: "Ready".to_string(),
            message: Some(
                "userspace mirror is reconciled over direct backplane endpoints".to_string(),
            ),
            runtime: Some(runtime),
            conditions: vec![condition(
                &volume,
                "True",
                "RuntimeReconciled",
                "leaf, userspace mirror, and client onramp Pods are running",
            )],
        },
    )
    .await?;
    Ok(Action::requeue(Duration::from_secs(10)))
}

async fn reconcile_volume(
    volume: Arc<ZcVolume>,
    ctx: Arc<Context>,
) -> Result<Action, OperatorError> {
    match reconcile_volume_inner(volume.clone(), ctx.clone()).await {
        Ok(action) => Ok(action),
        Err(error) => {
            if let Some(namespace) = volume.namespace() {
                let volumes: Api<ZcVolume> = Api::namespaced(ctx.client.clone(), &namespace);
                let message = error.to_string();
                let _ = patch_volume_status(
                    &volumes,
                    &volume,
                    ZcVolumeStatus {
                        observed_generation: Some(generation(&*volume)),
                        phase: "Failed".to_string(),
                        message: Some(message.clone()),
                        runtime: volume
                            .status
                            .as_ref()
                            .and_then(|status| status.runtime.clone()),
                        conditions: vec![condition(&volume, "False", "ReconcileFailed", &message)],
                    },
                )
                .await;
            }
            Err(error)
        }
    }
}

fn error_policy_volume(
    _volume: Arc<ZcVolume>,
    error: &OperatorError,
    _ctx: Arc<Context>,
) -> Action {
    eprintln!("zccusan-operator volume reconcile error: {error}");
    Action::requeue(Duration::from_secs(5))
}

async fn reconcile_profile_inner(
    profile: Arc<StorageProfile>,
    ctx: Arc<Context>,
) -> Result<Action, OperatorError> {
    let profiles: Api<StorageProfile> = Api::all(ctx.client.clone());
    let storage_classes: Api<StorageClass> = Api::all(ctx.client.clone());
    if profile.spec.enabled {
        validate_profile(&profile)?;
        resolve_tiering_policy(&ctx, &profile, None).await?;
    }
    let Some(published) = profile.spec.storage_class.as_ref() else {
        prune_owned_storage_classes(&storage_classes, &profile, None).await?;
        profiles
            .patch_status(
                &profile.name_any(),
                &PatchParams::default(),
                &Patch::Merge(json!({"status": StorageProfileStatus {
                    observed_generation: Some(generation(&*profile)),
                    phase: "Ready".to_string(),
                    storage_class_name: None,
                    message: Some("profile does not request a published StorageClass".to_string())
                }})),
            )
            .await?;
        return Ok(Action::await_change());
    };
    if !profile.spec.enabled {
        prune_owned_storage_classes(&storage_classes, &profile, None).await?;
        profiles
            .patch_status(
                &profile.name_any(),
                &PatchParams::default(),
                &Patch::Merge(json!({"status": StorageProfileStatus {
                    observed_generation: Some(generation(&*profile)),
                    phase: "Disabled".to_string(),
                    storage_class_name: None,
                    message: Some("profile is disabled and its published StorageClass is absent".to_string())
                }})),
            )
            .await?;
        return Ok(Action::requeue(Duration::from_secs(60)));
    }
    if let Some(existing) = storage_classes.get_opt(&published.name).await?
        && !owned_by_profile(&existing, &profile)
    {
        return Err(fail(format!(
            "StorageClass {} already exists and is not owned by StorageProfile {}",
            published.name,
            profile.name_any()
        )));
    }
    prune_owned_storage_classes(&storage_classes, &profile, Some(&published.name)).await?;
    let default_annotation = published
        .is_default
        .then(|| {
            BTreeMap::from([(
                "storageclass.kubernetes.io/is-default-class".to_string(),
                "true".to_string(),
            )])
        })
        .unwrap_or_default();
    let body = json!({
        "apiVersion": "storage.k8s.io/v1",
        "kind": "StorageClass",
        "metadata": {
            "name": published.name,
            "annotations": default_annotation,
            "ownerReferences": [{
                "apiVersion": format!("{API_GROUP}/{API_VERSION}"),
                "kind": "StorageProfile",
                "name": profile.name_any(),
                "uid": profile.meta().uid.clone().ok_or_else(|| fail("StorageProfile has no UID"))?,
                "controller": true,
                "blockOwnerDeletion": true
            }]
        },
        "provisioner": ctx.csi_provisioner,
        "parameters": {
            "backend": "fabric",
            "rawDevice": "/dev/zcnblk0",
            "storageProfile": profile.name_any()
        },
        "reclaimPolicy": "Delete",
        "volumeBindingMode": "WaitForFirstConsumer",
        "allowVolumeExpansion": false
    });
    storage_classes
        .patch(
            &published.name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&body),
        )
        .await?;
    profiles
        .patch_status(
            &profile.name_any(),
            &PatchParams::default(),
            &Patch::Merge(json!({"status": StorageProfileStatus {
                observed_generation: Some(generation(&*profile)),
                phase: "Ready".to_string(),
                storage_class_name: Some(published.name.clone()),
                message: Some("StorageClass reconciled".to_string())
            }})),
        )
        .await?;
    Ok(Action::requeue(Duration::from_secs(60)))
}

async fn reconcile_profile(
    profile: Arc<StorageProfile>,
    ctx: Arc<Context>,
) -> Result<Action, OperatorError> {
    match reconcile_profile_inner(profile.clone(), ctx.clone()).await {
        Ok(action) => Ok(action),
        Err(error) => {
            let message = error.to_string();
            let profiles: Api<StorageProfile> = Api::all(ctx.client.clone());
            let _ = profiles
                .patch_status(
                    &profile.name_any(),
                    &PatchParams::default(),
                    &Patch::Merge(json!({"status": StorageProfileStatus {
                        observed_generation: Some(generation(&*profile)),
                        phase: "Failed".to_string(),
                        storage_class_name: None,
                        message: Some(message)
                    }})),
                )
                .await;
            Err(error)
        }
    }
}

async fn reconcile_media_grant(
    grant: Arc<MediaGrant>,
    ctx: Arc<Context>,
) -> Result<Action, OperatorError> {
    let grants: Api<MediaGrant> = Api::all(ctx.client.clone());
    let nodes: Api<Node> = Api::all(ctx.client.clone());
    let mut eligible_nodes = BTreeSet::new();
    let mut media_set_names = BTreeSet::new();
    if grant.spec.enabled {
        if grant.spec.media_sets.is_empty() {
            return Err(fail("MediaGrant must contain at least one mediaSet"));
        }
        let node_items = nodes.list(&ListParams::default()).await?.items;
        for media_set in &grant.spec.media_sets {
            if !media_set_names.insert(media_set.name.clone()) {
                return Err(fail(format!(
                    "MediaGrant contains duplicate mediaSet name {:?}",
                    media_set.name
                )));
            }
            if media_set.name.trim().is_empty()
                || media_set.publish_as.media_class.trim().is_empty()
                || media_set.publish_as.durability.trim().is_empty()
            {
                return Err(fail(
                    "mediaSet name, publishAs.mediaClass, and publishAs.durability must not be empty",
                ));
            }
            for source in &media_set.dynamic_sources {
                if source.kind != "MemoryArena" {
                    return Err(fail(format!(
                        "unsupported dynamic media source kind {:?}",
                        source.kind
                    )));
                }
                if parse_bytes(&source.maximum_volume_size)? == 0 {
                    return Err(fail("dynamic maximumVolumeSize must be greater than zero"));
                }
                if source.huge_page_size.is_some() || source.pinned {
                    return Err(fail(
                        "hugePageSize and pinned memory are declared but not implemented by this first runtime reconciler",
                    ));
                }
                eligible_nodes.extend(
                    node_items
                        .iter()
                        .filter(|node| {
                            selector_matches(node, &grant.spec.node_selector.match_labels)
                        })
                        .map(ResourceExt::name_any),
                );
            }
            for source in &media_set.static_sources {
                if source.kind != "BlockDevice" {
                    return Err(fail(format!(
                        "unsupported static media source kind {:?}",
                        source.kind
                    )));
                }
                normalized_partuuid(&source.part_uuid)?;
                if source.node_name.trim().is_empty() {
                    return Err(fail("static block source nodeName must not be empty"));
                }
                if node_items.iter().any(|node| {
                    node.name_any() == source.node_name
                        && selector_matches(node, &grant.spec.node_selector.match_labels)
                }) {
                    eligible_nodes.insert(source.node_name.clone());
                }
            }
        }
    }
    let status = MediaGrantStatus {
        observed_generation: Some(generation(&*grant)),
        phase: if grant.spec.enabled {
            "Ready".to_string()
        } else {
            "Disabled".to_string()
        },
        eligible_nodes: eligible_nodes.into_iter().collect(),
        message: Some(if grant.spec.enabled {
            "media grant validated; eligible node inventory refreshed".to_string()
        } else {
            "media grant is disabled".to_string()
        }),
    };
    grants
        .patch_status(
            &grant.name_any(),
            &PatchParams::default(),
            &Patch::Merge(json!({"status": status})),
        )
        .await?;
    Ok(Action::requeue(Duration::from_secs(30)))
}

fn error_policy_media_grant(
    grant: Arc<MediaGrant>,
    error: &OperatorError,
    ctx: Arc<Context>,
) -> Action {
    eprintln!(
        "zccusan-operator MediaGrant {} reconcile error: {error}",
        grant.name_any()
    );
    let grant = grant.clone();
    let ctx = ctx.clone();
    let message = error.to_string();
    tokio::spawn(async move {
        let grants: Api<MediaGrant> = Api::all(ctx.client.clone());
        let _ = grants
            .patch_status(
                &grant.name_any(),
                &PatchParams::default(),
                &Patch::Merge(json!({"status": MediaGrantStatus {
                    observed_generation: Some(generation(&*grant)),
                    phase: "Failed".to_string(),
                    eligible_nodes: Vec::new(),
                    message: Some(message)
                }})),
            )
            .await;
    });
    Action::requeue(Duration::from_secs(5))
}

fn error_policy_profile(
    _profile: Arc<StorageProfile>,
    error: &OperatorError,
    _ctx: Arc<Context>,
) -> Action {
    eprintln!("zccusan-operator profile reconcile error: {error}");
    Action::requeue(Duration::from_secs(5))
}

async fn reconcile_tiering_policy(
    policy: Arc<TieringPolicy>,
    ctx: Arc<Context>,
) -> Result<Action, OperatorError> {
    let policies: Api<TieringPolicy> = Api::all(ctx.client.clone());
    let result = if policy.spec.enabled {
        validate_tiering_policy(&policy).map(|_| ("Ready", "tier policy validated"))
    } else {
        Ok(("Disabled", "tier policy is disabled"))
    };
    match result {
        Ok((phase, message)) => {
            policies
                .patch_status(
                    &policy.name_any(),
                    &PatchParams::default(),
                    &Patch::Merge(json!({"status": TieringPolicyStatus {
                        observed_generation: Some(generation(&*policy)),
                        phase: phase.to_string(),
                        message: Some(message.to_string()),
                    }})),
                )
                .await?;
            Ok(Action::requeue(Duration::from_secs(60)))
        }
        Err(error) => {
            policies
                .patch_status(
                    &policy.name_any(),
                    &PatchParams::default(),
                    &Patch::Merge(json!({"status": TieringPolicyStatus {
                        observed_generation: Some(generation(&*policy)),
                        phase: "Failed".to_string(),
                        message: Some(error.to_string()),
                    }})),
                )
                .await?;
            Err(error)
        }
    }
}

fn error_policy_tiering(
    policy: Arc<TieringPolicy>,
    error: &OperatorError,
    _ctx: Arc<Context>,
) -> Action {
    eprintln!(
        "zccusan-operator TieringPolicy {} reconcile error: {error}",
        policy.name_any()
    );
    Action::requeue(Duration::from_secs(5))
}

fn validate_cross_region(replication: &CrossRegionReplication) -> Result<(), OperatorError> {
    let spec = &replication.spec;
    if !spec.enabled {
        return Ok(());
    }
    if spec.mode != "AsynchronousCheckpoint" {
        return Err(fail(
            "only mode=AsynchronousCheckpoint is implemented; the live WAL transport is not yet the authenticated encrypted cross-region path",
        ));
    }
    if spec.automatic_failover {
        return Err(fail(
            "automaticFailover=true is rejected for checkpoint replication; promotion requires a separately verified failover policy and remote durable HWM",
        ));
    }
    if spec.transport.kind != "Aes256AuthenticatedTcp" {
        return Err(fail(
            "cross-region replication requires transport.kind=Aes256AuthenticatedTcp",
        ));
    }
    if spec.bytes == 0 {
        return Err(fail("cross-region bytes must be greater than zero"));
    }
    for (role, endpoint) in [("source", &spec.source), ("target", &spec.target)] {
        if endpoint.region.trim().is_empty() || endpoint.node_name.trim().is_empty() {
            return Err(fail(format!(
                "{role}.region and {role}.nodeName must not be empty"
            )));
        }
        let path = Path::new(&endpoint.path);
        if !path.is_absolute()
            || path.parent().is_none()
            || path.file_name().and_then(|name| name.to_str()).is_none()
            || endpoint.path.contains("..")
        {
            return Err(fail(format!(
                "{role}.path must be an absolute UTF-8 file path without '..' components"
            )));
        }
        if let Some(address) = endpoint.address.as_deref() {
            validate_backplane_address(address)?;
        }
    }
    if spec.source.region == spec.target.region {
        return Err(fail(
            "source.region and target.region must differ for CrossRegionReplication",
        ));
    }
    if spec.source.node_name == spec.target.node_name && spec.source.path == spec.target.path {
        return Err(fail("source and target must not name the same host file"));
    }
    if spec.credential_secret_ref.name.trim().is_empty()
        || spec.credential_secret_ref.key.trim().is_empty()
    {
        return Err(fail(
            "credentialSecretRef.name and credentialSecretRef.key must not be empty",
        ));
    }
    if !spec.allow_target_overwrite {
        return Err(fail(
            "allowTargetOverwrite=true is required because the receiver applies the checkpoint to the declared target file",
        ));
    }
    Ok(())
}

fn endpoint_mount(endpoint_path: &str) -> Result<(String, String), OperatorError> {
    let path = Path::new(endpoint_path);
    let parent = path
        .parent()
        .and_then(Path::to_str)
        .ok_or_else(|| fail("replication endpoint path has no UTF-8 parent"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| fail("replication endpoint path has no UTF-8 file name"))?;
    Ok((parent.to_string(), name.to_string()))
}

fn cross_owner_reference(replication: &CrossRegionReplication) -> Result<Value, OperatorError> {
    Ok(json!({
        "apiVersion": format!("{API_GROUP}/{API_VERSION}"),
        "kind": "CrossRegionReplication",
        "name": replication.name_any(),
        "uid": replication.meta().uid.clone().ok_or_else(|| fail("CrossRegionReplication has no UID"))?,
        "controller": true,
        "blockOwnerDeletion": true
    }))
}

fn cross_object_name(replication: &CrossRegionReplication, role: &str) -> String {
    let suffix = format!("g{}-{role}", generation(replication));
    let prefix = replication.name_any();
    let available = 63usize.saturating_sub(suffix.len() + 1);
    format!("{}-{suffix}", &prefix[..prefix.len().min(available)])
}

fn cross_labels(replication: &CrossRegionReplication, role: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (MANAGED_BY_LABEL.to_string(), FIELD_MANAGER.to_string()),
        (CROSS_REGION_LABEL.to_string(), replication.name_any()),
        (STAGE_LABEL.to_string(), format!("cross-region-{role}")),
    ])
}

fn cross_node_address(
    node: &Node,
    explicit: Option<&str>,
    annotation: &str,
) -> Result<String, OperatorError> {
    if let Some(explicit) = explicit {
        return validate_backplane_address(explicit);
    }
    if let Some(value) = node
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(annotation))
        .filter(|value| !value.trim().is_empty())
    {
        return validate_backplane_address(value.trim());
    }
    let address = node
        .status
        .as_ref()
        .and_then(|status| status.addresses.as_ref())
        .and_then(|addresses| {
            addresses
                .iter()
                .find(|address| address.type_ == "InternalIP")
        })
        .map(|address| address.address.as_str())
        .ok_or_else(|| fail(format!("node {} has no InternalIP", node.name_any())))?;
    validate_backplane_address(address)
}

async fn ensure_cross_finalizer(
    api: &Api<CrossRegionReplication>,
    replication: &CrossRegionReplication,
) -> Result<bool, OperatorError> {
    if replication
        .finalizers()
        .iter()
        .any(|value| value == CROSS_REGION_FINALIZER)
    {
        return Ok(false);
    }
    let mut finalizers = replication.finalizers().to_vec();
    finalizers.push(CROSS_REGION_FINALIZER.to_string());
    api.patch(
        &replication.name_any(),
        &PatchParams::default(),
        &Patch::Merge(json!({"metadata": {"finalizers": finalizers}})),
    )
    .await?;
    Ok(true)
}

async fn cleanup_cross_region(
    ctx: &Context,
    replication: &CrossRegionReplication,
) -> Result<Action, OperatorError> {
    let namespace = replication
        .namespace()
        .ok_or_else(|| fail("CrossRegionReplication is missing namespace"))?;
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let selector = format!("{CROSS_REGION_LABEL}={}", replication.name_any());
    pods.delete_collection(
        &DeleteParams::default(),
        &ListParams::default().labels(&selector),
    )
    .await?;
    let api: Api<CrossRegionReplication> = Api::namespaced(ctx.client.clone(), &namespace);
    let finalizers = replication
        .finalizers()
        .iter()
        .filter(|value| value.as_str() != CROSS_REGION_FINALIZER)
        .cloned()
        .collect::<Vec<_>>();
    api.patch(
        &replication.name_any(),
        &PatchParams::default(),
        &Patch::Merge(json!({"metadata": {"finalizers": finalizers}})),
    )
    .await?;
    Ok(Action::await_change())
}

async fn prune_cross_region_pods(
    ctx: &Context,
    replication: &CrossRegionReplication,
    retain: &[String],
) -> Result<(), OperatorError> {
    let namespace = replication
        .namespace()
        .ok_or_else(|| fail("CrossRegionReplication is missing namespace"))?;
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let selector = format!("{CROSS_REGION_LABEL}={}", replication.name_any());
    for pod in pods.list(&ListParams::default().labels(&selector)).await? {
        if !retain.iter().any(|name| name == &pod.name_any()) {
            match pods.delete(&pod.name_any(), &DeleteParams::default()).await {
                Ok(_) => {}
                Err(kube::Error::Api(response)) if response.code == 404 => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

async fn patch_cross_status(
    api: &Api<CrossRegionReplication>,
    replication: &CrossRegionReplication,
    phase: &str,
    message: impl Into<String>,
    sender: Option<String>,
    receiver: Option<String>,
    watermarks: (u64, u64, u64),
) -> Result<(), OperatorError> {
    api.patch_status(
        &replication.name_any(),
        &PatchParams::default(),
        &Patch::Merge(json!({"status": CrossRegionReplicationStatus {
            observed_generation: Some(generation(replication)),
            phase: phase.to_string(),
            message: Some(message.into()),
            sender_pod: sender,
            receiver_pod: receiver,
            accepted_hwm: watermarks.0,
            remote_durable_hwm: watermarks.1,
            remote_applied_hwm: watermarks.2,
            transport: Some("AES-256 authenticated TCP payload stream".to_string()),
        }})),
    )
    .await?;
    Ok(())
}

fn pod_phase(pod: &Pod) -> &str {
    pod.status
        .as_ref()
        .and_then(|status| status.phase.as_deref())
        .unwrap_or("Pending")
}

fn pod_ready(pod: &Pod) -> bool {
    pod_phase(pod) == "Succeeded"
        || pod
            .status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|condition| condition.type_ == "Ready" && condition.status == "True")
            })
}

async fn apply_cross_receiver(
    ctx: &Context,
    replication: &CrossRegionReplication,
    target_address: &str,
    port: u16,
) -> Result<String, OperatorError> {
    let namespace = replication
        .namespace()
        .ok_or_else(|| fail("CrossRegionReplication is missing namespace"))?;
    let (parent, file_name) = endpoint_mount(&replication.spec.target.path)?;
    let name = cross_object_name(replication, "recv");
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let body = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": cross_labels(replication, "receiver"),
            "annotations": {
                "storage.zcutils.io/region": replication.spec.target.region,
                "storage.zcutils.io/encryption": "aes-256-authenticated",
                "storage.zcutils.io/hwm-contract": "receiver-sync-data-before-success"
            },
            "ownerReferences": [cross_owner_reference(replication)?]
        },
        "spec": {
            "nodeName": replication.spec.target.node_name,
            "hostNetwork": true,
            "dnsPolicy": "ClusterFirstWithHostNet",
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": 2,
            "containers": [{
                "name": "receiver",
                "image": ctx.image,
                "imagePullPolicy": ctx.image_pull_policy,
                "command": ["/usr/local/bin/zcrepl"],
                "args": [
                    "recv", "--output", format!("/zc-repl-target/{file_name}"),
                    "--listen", target_address, "--port", port.to_string(),
                    "--bytes", replication.spec.bytes.to_string()
                ],
                "env": [{
                    "name": "ZCREPL_TOKEN",
                    "valueFrom": {"secretKeyRef": {
                        "name": replication.spec.credential_secret_ref.name,
                        "key": replication.spec.credential_secret_ref.key
                    }}
                }],
                "ports": [{"name": "repl", "containerPort": port, "hostPort": port, "protocol": "TCP"}],
                "securityContext": {
                    "runAsUser": 0,
                    "runAsGroup": 0,
                    "allowPrivilegeEscalation": false,
                    "readOnlyRootFilesystem": true,
                    "capabilities": {"drop": ["ALL"]}
                },
                "volumeMounts": [{"name": "target", "mountPath": "/zc-repl-target"}]
            }],
            "volumes": [{"name": "target", "hostPath": {"path": parent, "type": "DirectoryOrCreate"}}]
        }
    });
    pods.patch(
        &name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&body),
    )
    .await?;
    Ok(name)
}

async fn apply_cross_sender(
    ctx: &Context,
    replication: &CrossRegionReplication,
    target_address: &str,
    port: u16,
) -> Result<String, OperatorError> {
    let namespace = replication
        .namespace()
        .ok_or_else(|| fail("CrossRegionReplication is missing namespace"))?;
    let (parent, file_name) = endpoint_mount(&replication.spec.source.path)?;
    let name = cross_object_name(replication, "send");
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let body = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": cross_labels(replication, "sender"),
            "annotations": {
                "storage.zcutils.io/region": replication.spec.source.region,
                "storage.zcutils.io/encryption": "aes-256-authenticated"
            },
            "ownerReferences": [cross_owner_reference(replication)?]
        },
        "spec": {
            "nodeName": replication.spec.source.node_name,
            "hostNetwork": true,
            "dnsPolicy": "ClusterFirstWithHostNet",
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": 2,
            "containers": [{
                "name": "sender",
                "image": ctx.image,
                "imagePullPolicy": ctx.image_pull_policy,
                "command": ["/usr/local/bin/zcrepl"],
                "args": [
                    "send", "--input", format!("/zc-repl-source/{file_name}"),
                    "--peer", target_address, "--port", port.to_string(),
                    "--bytes", replication.spec.bytes.to_string()
                ],
                "env": [{
                    "name": "ZCREPL_TOKEN",
                    "valueFrom": {"secretKeyRef": {
                        "name": replication.spec.credential_secret_ref.name,
                        "key": replication.spec.credential_secret_ref.key
                    }}
                }],
                "securityContext": {
                    "runAsUser": 0,
                    "runAsGroup": 0,
                    "allowPrivilegeEscalation": false,
                    "readOnlyRootFilesystem": true,
                    "capabilities": {"drop": ["ALL"]}
                },
                "volumeMounts": [{"name": "source", "mountPath": "/zc-repl-source", "readOnly": true}]
            }],
            "volumes": [{"name": "source", "hostPath": {"path": parent, "type": "Directory"}}]
        }
    });
    pods.patch(
        &name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&body),
    )
    .await?;
    Ok(name)
}

async fn reconcile_cross_region_inner(
    replication: Arc<CrossRegionReplication>,
    ctx: Arc<Context>,
) -> Result<Action, OperatorError> {
    let namespace = replication
        .namespace()
        .ok_or_else(|| fail("CrossRegionReplication is missing namespace"))?;
    let replications: Api<CrossRegionReplication> = Api::namespaced(ctx.client.clone(), &namespace);
    if replication.meta().deletion_timestamp.is_some() {
        return cleanup_cross_region(&ctx, &replication).await;
    }
    if ensure_cross_finalizer(&replications, &replication).await? {
        return Ok(Action::await_change());
    }
    if !replication.spec.enabled {
        prune_cross_region_pods(&ctx, &replication, &[]).await?;
        patch_cross_status(
            &replications,
            &replication,
            "Disabled",
            "cross-region replication is disabled",
            None,
            None,
            (0, 0, 0),
        )
        .await?;
        return Ok(Action::requeue(Duration::from_secs(60)));
    }
    validate_cross_region(&replication)?;
    let receiver_name = cross_object_name(&replication, "recv");
    let sender_name = cross_object_name(&replication, "send");
    prune_cross_region_pods(
        &ctx,
        &replication,
        &[receiver_name.clone(), sender_name.clone()],
    )
    .await?;
    let nodes: Api<Node> = Api::all(ctx.client.clone());
    let source_node = nodes.get(&replication.spec.source.node_name).await?;
    let target_node = nodes.get(&replication.spec.target.node_name).await?;
    let _source_address = cross_node_address(
        &source_node,
        replication.spec.source.address.as_deref(),
        &replication.spec.transport.node_address_annotation,
    )?;
    let target_address = cross_node_address(
        &target_node,
        replication.spec.target.address.as_deref(),
        &replication.spec.transport.node_address_annotation,
    )?;
    let identity = format!(
        "{}/{}-g{}",
        namespace,
        replication.name_any(),
        generation(&*replication)
    );
    let port = stable_port(&identity, 28_000);
    let receiver = apply_cross_receiver(&ctx, &replication, &target_address, port).await?;
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let receiver_pod = pods.get(&receiver).await?;
    if pod_phase(&receiver_pod) == "Failed" {
        return Err(fail(format!("receiver Pod {receiver} failed")));
    }
    if !pod_ready(&receiver_pod) {
        patch_cross_status(
            &replications,
            &replication,
            "StartingReceiver",
            "waiting for encrypted receiver readiness",
            None,
            Some(receiver),
            (0, 0, 0),
        )
        .await?;
        return Ok(Action::requeue(Duration::from_secs(1)));
    }
    let sender = apply_cross_sender(&ctx, &replication, &target_address, port).await?;
    let sender_pod = pods.get(&sender).await?;
    if pod_phase(&sender_pod) == "Failed" {
        return Err(fail(format!("sender Pod {sender} failed")));
    }
    let receiver_pod = pods.get(&receiver).await?;
    if pod_phase(&receiver_pod) == "Failed" {
        return Err(fail(format!("receiver Pod {receiver} failed")));
    }
    if pod_phase(&sender_pod) == "Succeeded" && pod_phase(&receiver_pod) == "Succeeded" {
        let bytes = replication.spec.bytes;
        patch_cross_status(
            &replications,
            &replication,
            "Ready",
            "authenticated encrypted checkpoint received and sync_data completed",
            Some(sender),
            Some(receiver),
            (bytes, bytes, bytes),
        )
        .await?;
        return Ok(Action::requeue(Duration::from_secs(60)));
    }
    patch_cross_status(
        &replications,
        &replication,
        "Replicating",
        "encrypted checkpoint transfer is in progress",
        Some(sender),
        Some(receiver),
        (0, 0, 0),
    )
    .await?;
    Ok(Action::requeue(Duration::from_secs(1)))
}

async fn reconcile_cross_region(
    replication: Arc<CrossRegionReplication>,
    ctx: Arc<Context>,
) -> Result<Action, OperatorError> {
    match reconcile_cross_region_inner(replication.clone(), ctx.clone()).await {
        Ok(action) => Ok(action),
        Err(error) => {
            if let Some(namespace) = replication.namespace() {
                let api: Api<CrossRegionReplication> =
                    Api::namespaced(ctx.client.clone(), &namespace);
                let _ = patch_cross_status(
                    &api,
                    &replication,
                    "Failed",
                    error.to_string(),
                    replication
                        .status
                        .as_ref()
                        .and_then(|status| status.sender_pod.clone()),
                    replication
                        .status
                        .as_ref()
                        .and_then(|status| status.receiver_pod.clone()),
                    (0, 0, 0),
                )
                .await;
            }
            Err(error)
        }
    }
}

fn error_policy_cross_region(
    replication: Arc<CrossRegionReplication>,
    error: &OperatorError,
    _ctx: Arc<Context>,
) -> Action {
    eprintln!(
        "zccusan-operator CrossRegionReplication {} reconcile error: {error}",
        replication.name_any()
    );
    Action::requeue(Duration::from_secs(5))
}

async fn volume_controller(ctx: Arc<Context>) {
    let volumes: Api<ZcVolume> = Api::all(ctx.client.clone());
    let pods: Api<Pod> = Api::all(ctx.client.clone());
    Controller::new(volumes, watcher::Config::default())
        .owns(pods, watcher::Config::default())
        .run(reconcile_volume, error_policy_volume, ctx)
        .for_each(|result| async move {
            if let Err(error) = result {
                eprintln!("zccusan-operator volume controller stream error: {error}");
            }
        })
        .await;
}

async fn profile_controller(ctx: Arc<Context>) {
    let profiles: Api<StorageProfile> = Api::all(ctx.client.clone());
    let storage_classes: Api<StorageClass> = Api::all(ctx.client.clone());
    Controller::new(profiles, watcher::Config::default())
        .owns(storage_classes, watcher::Config::default())
        .run(reconcile_profile, error_policy_profile, ctx)
        .for_each(|result| async move {
            if let Err(error) = result {
                eprintln!("zccusan-operator profile controller stream error: {error}");
            }
        })
        .await;
}

async fn media_grant_controller(ctx: Arc<Context>) {
    let grants: Api<MediaGrant> = Api::all(ctx.client.clone());
    Controller::new(grants, watcher::Config::default())
        .run(reconcile_media_grant, error_policy_media_grant, ctx)
        .for_each(|result| async move {
            if let Err(error) = result {
                eprintln!("zccusan-operator media grant controller stream error: {error}");
            }
        })
        .await;
}

async fn tiering_policy_controller(ctx: Arc<Context>) {
    let policies: Api<TieringPolicy> = Api::all(ctx.client.clone());
    Controller::new(policies, watcher::Config::default())
        .run(reconcile_tiering_policy, error_policy_tiering, ctx)
        .for_each(|result| async move {
            if let Err(error) = result {
                eprintln!("zccusan-operator tiering policy controller stream error: {error}");
            }
        })
        .await;
}

async fn cross_region_controller(ctx: Arc<Context>) {
    let replications: Api<CrossRegionReplication> = Api::all(ctx.client.clone());
    let pods: Api<Pod> = Api::all(ctx.client.clone());
    Controller::new(replications, watcher::Config::default())
        .owns(pods, watcher::Config::default())
        .run(reconcile_cross_region, error_policy_cross_region, ctx)
        .for_each(|result| async move {
            if let Err(error) = result {
                eprintln!("zccusan-operator cross-region controller stream error: {error}");
            }
        })
        .await;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let client = Client::try_default().await?;
    let repository = env::var("ZCCUSAN_RUNTIME_IMAGE")
        .unwrap_or_else(|_| "docker.io/robjcaskey/zcblock-csi:nightly".to_string());
    let pull_policy =
        env::var("ZCCUSAN_RUNTIME_IMAGE_PULL_POLICY").unwrap_or_else(|_| "IfNotPresent".into());
    let csi_provisioner =
        env::var("ZCCUSAN_CSI_PROVISIONER").unwrap_or_else(|_| "io.zcutils.zcblock".to_string());
    if csi_provisioner.trim().is_empty() {
        return Err("ZCCUSAN_CSI_PROVISIONER must not be empty".into());
    }
    let ctx = Arc::new(Context {
        client,
        image: repository,
        image_pull_policy: pull_policy,
        csi_provisioner,
    });
    eprintln!(
        "zccusan-operator starting image={} csi_provisioner={} data_path=direct-backplane services=none placement=userspace",
        ctx.image, ctx.csi_provisioner
    );
    let volumes = tokio::spawn(volume_controller(ctx.clone()));
    let profiles = tokio::spawn(profile_controller(ctx.clone()));
    let media_grants = tokio::spawn(media_grant_controller(ctx.clone()));
    let tiering_policies = tokio::spawn(tiering_policy_controller(ctx.clone()));
    let cross_region = tokio::spawn(cross_region_controller(ctx));
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = volumes => {},
        _ = profiles => {},
        _ = media_grants => {},
        _ = tiering_policies => {},
        _ = cross_region => {},
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use serde::Deserialize;

    fn node(name: &str, zone: &str) -> Node {
        Node {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                labels: Some(BTreeMap::from([
                    ("kubernetes.io/hostname".to_string(), name.to_string()),
                    ("topology.kubernetes.io/zone".to_string(), zone.to_string()),
                ])),
                ..ObjectMeta::default()
            },
            ..Node::default()
        }
    }

    #[test]
    fn parses_binary_and_decimal_byte_quantities() {
        assert_eq!(parse_bytes("128Mi").unwrap(), 128 * 1024 * 1024);
        assert_eq!(parse_bytes("2GB").unwrap(), 2_000_000_000);
        assert!(parse_bytes("1Ei").is_err());
    }

    #[test]
    fn stable_ports_remain_in_disjoint_runtime_ranges() {
        let leaf = stable_port("zcblk-csi-test", 26_000);
        let fan = stable_port("zcblk-csi-test", 23_000);
        let cross_region = stable_port("zcblk-csi-test", 28_000);
        let mirror_control = stable_port("zcblk-csi-test", 31_000);
        assert!((26_000..28_000).contains(&leaf));
        assert!((23_000..25_000).contains(&fan));
        assert!((28_000..30_000).contains(&cross_region));
        assert!((31_000..33_000).contains(&mirror_control));
    }

    #[test]
    fn topology_distinctness_rejects_same_zone() {
        let selected = vec![Candidate {
            node: node("leaf-a", "zone-a"),
            address: "10.0.0.1".to_string(),
            media_class: "test".to_string(),
            durability: "Volatile".to_string(),
            source: CandidateSource::Memory,
        }];
        let same_zone = Candidate {
            node: node("leaf-b", "zone-a"),
            address: "10.0.0.2".to_string(),
            media_class: "test".to_string(),
            durability: "Volatile".to_string(),
            source: CandidateSource::Memory,
        };
        let other_zone = Candidate {
            node: node("leaf-c", "zone-b"),
            address: "10.0.0.3".to_string(),
            media_class: "test".to_string(),
            durability: "Volatile".to_string(),
            source: CandidateSource::Memory,
        };
        let keys = vec![
            "kubernetes.io/hostname".to_string(),
            "topology.kubernetes.io/zone".to_string(),
        ];
        assert!(!candidates_are_distinct(&selected, &same_zone, &keys));
        assert!(candidates_are_distinct(&selected, &other_zone, &keys));
    }

    #[test]
    fn partuuid_reservation_names_are_case_insensitive_and_bounded() {
        assert_eq!(
            reservation_name("ABCD-1234").unwrap(),
            "zccusan-media-abcd-1234"
        );
        assert!(reservation_name(&"a".repeat(49)).is_err());
    }

    #[test]
    fn first_wave_tier_and_cross_region_contracts_validate_fail_closed() {
        let tier_source =
            include_str!("../../zccusan/deploy/zcblock-csi/getting-started/tiered-mirror-ram.yaml");
        let tier_value = serde_yaml::Deserializer::from_str(tier_source)
            .next()
            .map(|document| serde_yaml::Value::deserialize(document).unwrap())
            .unwrap();
        let tier: TieringPolicy = serde_yaml::from_value(tier_value).unwrap();
        validate_tiering_policy(&tier).unwrap();

        let cross_source = include_str!(
            "../../zccusan/deploy/zcblock-csi/getting-started/cross-region-checkpoint.template.yaml"
        );
        let cross_value = serde_yaml::Deserializer::from_str(cross_source)
            .nth(1)
            .map(|document| serde_yaml::Value::deserialize(document).unwrap())
            .unwrap();
        let mut cross: CrossRegionReplication = serde_yaml::from_value(cross_value).unwrap();
        validate_cross_region(&cross).unwrap();
        cross.spec.automatic_failover = true;
        assert!(validate_cross_region(&cross).is_err());
        cross.spec.automatic_failover = false;
        cross.spec.transport.kind = "PlaintextTcp".to_string();
        assert!(validate_cross_region(&cross).is_err());
    }
}
