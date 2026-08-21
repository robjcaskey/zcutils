use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub const CURRENT_TELEMETRY_SCHEMA_VERSION: u64 = 1;
pub const CURRENT_ANONYMIZATION_SCHEMA_VERSION: u64 = 1;

const SAFE_STRING_FIELDS: &[&str] = &[
    "event_type",
    "cloud_provider",
    "cloud_region",
    "version",
    "phase",
    "component",
    "backend",
    "status",
    "latency_scope",
];
const SAFE_INTEGER_FIELDS: &[&str] = &[
    "event_at_ms",
    "started_at_millis",
    "interval_secs",
    "active_volume_count",
    "sampled_volume_count",
    "missing_device_volume_count",
    "total_iops",
    "avg_iops_per_volume",
    "cluster_node_count",
    "size_bytes",
    "io_size_bytes",
    "requested_bytes",
    "evicted_first_message_index",
    "evicted_last_message_index",
    "evicted_count",
    "hop_count",
    "path_count",
    "latency_sample_count",
    "latency_p50_ns",
    "latency_p95_ns",
    "latency_p99_ns",
    "latency_p995_ns",
    "latency_p999_ns",
    "latency_jitter_p995_ns",
    "lane_count",
    "worker_count",
    "numa_node_count",
    "nic_count",
];
const SAFE_BOOLEAN_FIELDS: &[&str] = &[
    "ok",
    "evicted_events_were_logged_to_stdout",
    "upstream_acknowledged_before_eviction",
    "cpu_pinned",
    "numa_local",
];
const SAFE_INTEGER_MAP_FIELDS: &[&str] = &["iops_distribution", "backend_iops"];
const SOURCE_ID_FIELDS: &[&str] = &[
    "installation_id",
    "environment_id",
    "anonymous_installation_id",
];

/// A telemetry record received from a local process or a mixed-version client.
///
/// Its fields are intentionally opaque: possession of this type does not imply
/// that the record is safe to send to the community survey.
#[derive(Clone, Debug)]
pub struct TelemetryRecord {
    value: Value,
}

/// A telemetry record that has crossed the explicit non-identifying boundary.
///
/// The inner value is private, so community-survey senders cannot accidentally
/// accept arbitrary `serde_json::Value` payloads.
#[derive(Clone, Debug)]
pub struct NonIdentifyingTelemetry {
    value: Value,
}

impl TelemetryRecord {
    pub fn current(event_type: &str, mut fields: Map<String, Value>) -> Self {
        fields.insert("event_type".to_string(), json!(event_type));
        fields.insert(
            "telemetry_schema_version".to_string(),
            json!(CURRENT_TELEMETRY_SCHEMA_VERSION),
        );
        Self {
            value: Value::Object(fields),
        }
    }

    /// Accept legacy records without a schema version and future records with
    /// unknown fields. Anonymization remains allowlist-based, so unknown fields
    /// never pass through merely because a newer producer supplied them.
    pub fn from_value(value: Value) -> Option<Self> {
        value.is_object().then_some(Self { value })
    }

    pub fn from_json(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes)
            .ok()
            .and_then(Self::from_value)
    }

    pub fn as_value(&self) -> &Value {
        &self.value
    }

    pub fn to_json_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.value).unwrap_or_default()
    }

    pub fn anonymize(&self) -> NonIdentifyingTelemetry {
        NonIdentifyingTelemetry::from_record(self)
    }
}

impl NonIdentifyingTelemetry {
    fn from_record(record: &TelemetryRecord) -> Self {
        let source = record
            .value
            .as_object()
            .expect("TelemetryRecord is always an object");
        let mut output = Map::new();

        output.insert(
            "telemetry_schema_version".to_string(),
            json!(schema_version(source)),
        );
        output.insert(
            "anonymization_schema_version".to_string(),
            json!(CURRENT_ANONYMIZATION_SCHEMA_VERSION),
        );

        for name in SAFE_STRING_FIELDS {
            if let Some(value) = bounded_string(source.get(*name), 128) {
                output.insert((*name).to_string(), Value::String(value));
            }
        }
        for name in SAFE_INTEGER_FIELDS {
            if let Some(value) = nonnegative_integer(source.get(*name)) {
                output.insert((*name).to_string(), Value::from(value));
            }
        }
        for name in SAFE_BOOLEAN_FIELDS {
            if let Some(value) = source.get(*name).and_then(Value::as_bool) {
                output.insert((*name).to_string(), Value::Bool(value));
            }
        }
        for name in SAFE_INTEGER_MAP_FIELDS {
            if let Some(value) = safe_integer_map(source.get(*name)) {
                output.insert((*name).to_string(), Value::Object(value));
            }
        }

        if let Some(release) = bounded_string(source.get("kernel_release"), 256)
            .or_else(|| bounded_string(source.get("kernel_family"), 256))
        {
            output.insert(
                "kernel_family".to_string(),
                Value::String(approved_kernel_family(&release)),
            );
        }
        if let Some(value) = approved_literal(
            source.get("topology_class"),
            &[
                "direct",
                "client-leaf",
                "client-hop-leaf",
                "multi-hop",
                "unknown",
            ],
        ) {
            output.insert("topology_class".to_string(), Value::String(value));
        }
        if let Some(value) = approved_literal(
            source.get("placement_scope"),
            &[
                "same-placement-group",
                "same-az",
                "same-region",
                "cross-region",
                "unknown",
            ],
        ) {
            output.insert("placement_scope".to_string(), Value::String(value));
        }
        if let Some(value) = approved_literal(
            source.get("virtualization_family"),
            &[
                "bare-metal",
                "nitro-vm",
                "qemu",
                "firecracker",
                "xen",
                "hyper-v",
                "vmware",
                "container",
                "unknown",
            ],
        ) {
            output.insert("virtualization_family".to_string(), Value::String(value));
        }
        if let Some(value) = approved_literal(
            source.get("lane_mapping"),
            &[
                "one-lane-per-worker",
                "shared-workers",
                "dedicated-with-spares",
                "unknown",
            ],
        ) {
            output.insert("lane_mapping".to_string(), Value::String(value));
        }
        if let Some(value) = approved_literal(
            source.get("frontend"),
            &[
                "userspace-client",
                "linux-block",
                "kubernetes-csi",
                "libvirt-disk",
                "qemu-block",
                "qemu-virtio-blk",
                "qemu-virtio-scsi",
                "qemu-nvme",
                "vhost-user-blk",
                "spdk-bdev",
                "nvme-of",
                "unknown",
            ],
        ) {
            output.insert("frontend".to_string(), Value::String(value));
        }
        if let Some(value) = safe_transport_map(source.get("transport_paths")) {
            output.insert("transport_paths".to_string(), Value::Object(value));
        }
        if let Some(value) = safe_topology_hops(source.get("topology_hops")) {
            output.insert("topology_hops".to_string(), Value::Array(value));
        }

        if let Some(source_id) = SOURCE_ID_FIELDS
            .iter()
            .find_map(|name| bounded_string(source.get(*name), 256))
        {
            output.insert(
                "anonymous_installation_id".to_string(),
                Value::String(anonymous_installation_id(&source_id)),
            );
        }

        Self {
            value: Value::Object(output),
        }
    }

    pub fn as_value(&self) -> &Value {
        &self.value
    }

    pub fn to_json_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.value).unwrap_or_default()
    }
}

fn schema_version(source: &Map<String, Value>) -> u64 {
    source
        .get("telemetry_schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn bounded_string(value: Option<&Value>, maximum_bytes: usize) -> Option<String> {
    let value = value?.as_str()?.trim();
    if value.is_empty() {
        return None;
    }
    Some(value.chars().take(maximum_bytes).collect())
}

fn nonnegative_integer(value: Option<&Value>) -> Option<u64> {
    value?.as_u64()
}

fn safe_integer_map(value: Option<&Value>) -> Option<Map<String, Value>> {
    let source = value?.as_object()?;
    let mut output = Map::new();
    for (key, value) in source.iter().take(64) {
        let Some(number) = value.as_u64() else {
            continue;
        };
        let safe_key: String = key
            .chars()
            .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
            .take(64)
            .collect();
        if !safe_key.is_empty() {
            output.insert(safe_key, Value::from(number));
        }
    }
    Some(output)
}

fn approved_literal(value: Option<&Value>, allowed: &[&str]) -> Option<String> {
    let value = value?.as_str()?.trim().to_ascii_lowercase();
    allowed.contains(&value.as_str()).then_some(value)
}

fn approved_transport(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "tcp" | "tcp-mux" => Some("tcp"),
        "efa-direct" => Some("efa-direct"),
        "efa" | "efa-rdm" => Some("efa"),
        "rdma" | "verbs" => Some("rdma"),
        "libfabric-sockets" | "sockets" => Some("libfabric-sockets"),
        "unix" | "unix-socket" => Some("unix"),
        "shared-memory" | "shm" => Some("shared-memory"),
        "in-process" => Some("in-process"),
        "unknown" | "other" => Some("unknown"),
        _ => None,
    }
}

fn safe_transport_map(value: Option<&Value>) -> Option<Map<String, Value>> {
    let source = value?.as_object()?;
    let mut output = Map::new();
    for (key, value) in source.iter().take(16) {
        let transport = approved_transport(key)?;
        let count = value.as_u64()?.clamp(1, 256);
        output.insert(transport.to_string(), Value::from(count));
    }
    (!output.is_empty()).then_some(output)
}

fn safe_topology_hops(value: Option<&Value>) -> Option<Vec<Value>> {
    let source = value?.as_array()?;
    let mut output = Vec::new();
    for (ordinal, hop) in source.iter().take(8).enumerate() {
        let hop = hop.as_object()?;
        let role = approved_literal(
            hop.get("role"),
            &[
                "client-edge",
                "userspace-hop",
                "storage-service",
                "terminal-leaf",
            ],
        )?;
        let transport = approved_transport(hop.get("transport")?.as_str()?)?;
        let mut safe = Map::new();
        safe.insert("ordinal".to_string(), Value::from(ordinal as u64));
        safe.insert("role".to_string(), Value::String(role));
        safe.insert(
            "transport".to_string(),
            Value::String(transport.to_string()),
        );
        if let Some(paths) = hop.get("path_count").and_then(Value::as_u64) {
            safe.insert("path_count".to_string(), Value::from(paths.clamp(1, 256)));
        }
        if let Some(release) = hop
            .get("kernel_release")
            .or_else(|| hop.get("kernel_family"))
            .and_then(Value::as_str)
        {
            safe.insert(
                "kernel_family".to_string(),
                Value::String(approved_kernel_family(release)),
            );
        }
        for name in [
            "latency_sample_count",
            "latency_p50_ns",
            "latency_p95_ns",
            "latency_p99_ns",
            "latency_p995_ns",
            "latency_p999_ns",
            "latency_jitter_p995_ns",
        ] {
            if let Some(value) = hop.get(name).and_then(Value::as_u64) {
                safe.insert(name.to_string(), Value::from(value));
            }
        }
        output.push(Value::Object(safe));
    }
    (!output.is_empty()).then_some(output)
}

/// Approve complete releases from a deliberately small set of literal vendor
/// patterns. Unknown/custom suffixes are never hashed or forwarded: they
/// collapse to `linux-custom` so build hostnames and organization names
/// embedded in locally compiled releases cannot escape.
pub fn approved_kernel_family(release: &str) -> String {
    let release = release.trim().to_ascii_lowercase();
    let core = release.split('-').next().unwrap_or_default();
    let components: Vec<&str> = core.split('.').collect();
    if components.len() < 2
        || !components.iter().all(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return "linux-custom".to_string();
    }
    let suffix = release.strip_prefix(core).unwrap_or_default();
    if suffix.is_empty() {
        return format!("linux-{release}");
    }
    let suffix_parts: Vec<&str> = suffix.trim_start_matches('-').split('-').collect();
    let literal_vendor = suffix_parts.last().copied();
    let approved_simple_vendor =
        matches!(literal_vendor, Some("aws" | "generic" | "azure" | "gcp"))
            && suffix_parts.len() >= 2
            && suffix_parts[..suffix_parts.len() - 1]
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if approved_simple_vendor && release.len() <= 96 {
        format!("linux-{release}")
    } else {
        "linux-custom".to_string()
    }
}

/// Return only a coarse, approved execution environment. DMI product strings,
/// guest names, container IDs, and hypervisor build strings are never returned.
pub fn detected_virtualization_family() -> &'static str {
    if std::path::Path::new("/.dockerenv").exists()
        || std::fs::read_to_string("/proc/1/cgroup")
            .ok()
            .is_some_and(|value| value.contains("docker") || value.contains("kubepods"))
    {
        return "container";
    }
    let product = std::fs::read_to_string("/sys/class/dmi/id/product_name")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let vendor = std::fs::read_to_string("/sys/class/dmi/id/sys_vendor")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let combined = format!("{vendor} {product}");
    if combined.contains("amazon ec2") {
        "nitro-vm"
    } else if combined.contains("qemu") || combined.contains("kvm") {
        "qemu"
    } else if combined.contains("firecracker") {
        "firecracker"
    } else if combined.contains("xen") {
        "xen"
    } else if combined.contains("microsoft") || combined.contains("hyper-v") {
        "hyper-v"
    } else if combined.contains("vmware") {
        "vmware"
    } else if combined.trim().is_empty() {
        "unknown"
    } else {
        "bare-metal"
    }
}

fn anonymous_installation_id(source_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"zccusan-community-installation-v1\0");
    digest.update(source_id.as_bytes());
    format!("anon-{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_records_are_anonymized_by_allowlist() {
        let record = TelemetryRecord::from_value(json!({
            "event_type": "csi_hourly_stats",
            "environment_id": "private-installation-id",
            "node_id": "private-node-id",
            "volume_id": "private-volume-id",
            "error_detail": "/private/path",
            "total_iops": 12_345,
            "future_unknown_field": "must not pass",
        }))
        .expect("legacy object");

        let anonymized = record.anonymize();
        assert_eq!(anonymized.as_value()["telemetry_schema_version"], 0);
        assert_eq!(anonymized.as_value()["total_iops"], 12_345);
        assert!(
            anonymized.as_value()["anonymous_installation_id"]
                .as_str()
                .is_some_and(|value| value.starts_with("anon-"))
        );
        let serialized = anonymized.to_json_bytes();
        let serialized = String::from_utf8(serialized).expect("JSON UTF-8");
        assert!(!serialized.contains("private-installation-id"));
        assert!(!serialized.contains("private-node-id"));
        assert!(!serialized.contains("private-volume-id"));
        assert!(!serialized.contains("future_unknown_field"));
        assert!(!serialized.contains("/private/path"));
    }

    #[test]
    fn future_versions_keep_only_known_non_identifying_fields() {
        let record = TelemetryRecord::from_value(json!({
            "telemetry_schema_version": 99,
            "event_type": "future_event",
            "cloud_region": "us-east-1",
            "new_identifier": "secret",
            "active_volume_count": 7,
            "io_size_bytes": 4096,
        }))
        .expect("future object");
        let anonymized = record.anonymize();

        assert_eq!(anonymized.as_value()["telemetry_schema_version"], 99);
        assert_eq!(anonymized.as_value()["active_volume_count"], 7);
        assert_eq!(anonymized.as_value()["io_size_bytes"], 4096);
        assert!(anonymized.as_value().get("new_identifier").is_none());
    }

    #[test]
    fn current_records_carry_an_explicit_schema_version() {
        let record = TelemetryRecord::current("startup", Map::new());
        assert_eq!(
            record.as_value()["telemetry_schema_version"],
            CURRENT_TELEMETRY_SCHEMA_VERSION
        );
    }

    #[test]
    fn known_vendor_kernel_is_preserved_but_custom_release_is_not() {
        assert_eq!(
            approved_kernel_family("6.17.0-1017-aws"),
            "linux-6.17.0-1017-aws"
        );
        assert_eq!(
            approved_kernel_family("6.8.0-79-generic"),
            "linux-6.8.0-79-generic"
        );
        assert_eq!(
            approved_kernel_family("7.2.0-company-buildhost"),
            "linux-custom"
        );
        assert_eq!(approved_kernel_family("host-secret"), "linux-custom");
    }

    #[test]
    fn topology_is_reduced_to_ordinal_allowlisted_hops() {
        let record = TelemetryRecord::from_value(json!({
            "telemetry_schema_version": 1,
            "installation_id": "private-installation",
            "kernel_release": "6.17.0-1017-aws",
            "topology_class": "client-leaf",
            "placement_scope": "same-placement-group",
            "transport_paths": {"efa-direct": 2, "secret-transport": 9},
            "topology_hops": [{
                "node_id": "i-private",
                "hostname": "private.example",
                "role": "storage-service",
                "transport": "efa-direct",
                "path_count": 2,
                "kernel_release": "7.2.0-company-host",
                "latency_p995_ns": 1200000,
                "latency_jitter_p995_ns": 300000
            }]
        }))
        .expect("record");
        let output = record.anonymize();
        let output = output.as_value();
        assert_eq!(output["kernel_family"], "linux-6.17.0-1017-aws");
        assert_eq!(output["placement_scope"], "same-placement-group");
        assert_eq!(output["topology_hops"][0]["kernel_family"], "linux-custom");
        assert_eq!(output["topology_hops"][0]["ordinal"], 0);
        assert_eq!(output["topology_hops"][0]["latency_p995_ns"], 1200000);
        assert!(output["topology_hops"][0].get("node_id").is_none());
        assert!(output["topology_hops"][0].get("hostname").is_none());
        // A single unknown key rejects the transport map instead of leaking a
        // caller-defined label that might contain an identifier.
        assert!(output.get("transport_paths").is_none());
    }
}
