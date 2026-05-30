use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use super::logstream::ZccusanLogEntry;

pub const DEFAULT_CONTROL_URL: &str = "http://127.0.0.1:9788";
pub const GLOBAL_REPLICATION_SCOPE: &str = "global";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotSpec {
    pub snapshot_id: String,
    pub name_hex: String,
    pub source_volume_id: String,
    pub source_backend: String,
    pub size_bytes: i64,
    pub snapshot_path: String,
    pub snapshot_mode: String,
    pub creation_time_secs: i64,
    pub ready_to_use: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateSnapshotRequest {
    pub source_volume_id: String,
    pub snapshot_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateSnapshotResponse {
    pub snapshot: SnapshotSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeleteSnapshotResponse {
    pub deleted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotDeviceSpec {
    pub device_id: String,
    pub snapshot_id: String,
    pub source_volume_id: String,
    pub mode: String,
    pub device_name: String,
    pub device_path: String,
    pub configfs_path: String,
    pub size_bytes: i64,
    pub readonly: bool,
    pub state: String,
    #[serde(default)]
    pub compaction_job_id: Option<String>,
    pub created_at_millis: u64,
    pub updated_at_millis: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateSnapshotDeviceRequest {
    pub snapshot_id: String,
    #[serde(default)]
    pub device_id: Option<String>,
    pub mode: String,
    #[serde(default)]
    pub readonly: Option<bool>,
    #[serde(default)]
    pub start_compaction: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotDeviceResponse {
    pub device: SnapshotDeviceSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotDeviceStatusResponse {
    pub devices: Vec<SnapshotDeviceSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeleteSnapshotDeviceResponse {
    pub deleted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotCompactionJob {
    pub job_id: String,
    pub device_id: String,
    pub snapshot_id: String,
    pub mode: String,
    pub strategy: String,
    pub phase: String,
    pub state: String,
    pub bytes_compacted: u64,
    pub bytes_streamed_out: u64,
    pub bytes_streamed_in: u64,
    #[serde(default)]
    pub bytes_total: Option<u64>,
    #[serde(default)]
    pub outbound_stream_id: Option<String>,
    #[serde(default)]
    pub inbound_stream_id: Option<String>,
    #[serde(default)]
    pub target_location: Option<String>,
    #[serde(default)]
    pub worker_id: Option<String>,
    #[serde(default)]
    pub checkpoint: Option<String>,
    pub started_at_millis: u64,
    pub updated_at_millis: u64,
    #[serde(default)]
    pub finished_at_millis: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StartSnapshotCompactionRequest {
    #[serde(default)]
    pub strategy: Option<String>,
    #[serde(default)]
    pub target_location: Option<String>,
    #[serde(default)]
    pub worker_id: Option<String>,
    #[serde(default)]
    pub checkpoint: Option<String>,
    #[serde(default)]
    pub outbound_stream_id: Option<String>,
    #[serde(default)]
    pub inbound_stream_id: Option<String>,
    #[serde(default)]
    pub bytes_total: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpdateSnapshotCompactionRequest {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub bytes_compacted: Option<u64>,
    #[serde(default)]
    pub bytes_streamed_out: Option<u64>,
    #[serde(default)]
    pub bytes_streamed_in: Option<u64>,
    #[serde(default)]
    pub bytes_total: Option<u64>,
    #[serde(default)]
    pub outbound_stream_id: Option<String>,
    #[serde(default)]
    pub inbound_stream_id: Option<String>,
    #[serde(default)]
    pub target_location: Option<String>,
    #[serde(default)]
    pub worker_id: Option<String>,
    #[serde(default)]
    pub checkpoint: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub finished: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotCompactionResponse {
    pub job: SnapshotCompactionJob,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotCompactionStatusResponse {
    pub jobs: Vec<SnapshotCompactionJob>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FreezeRequest {
    pub barrier_id: String,
    pub ttl_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FreezeResponse {
    pub barrier_id: String,
    pub frozen_mounts: Vec<String>,
    pub remaining_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FreezeStatusResponse {
    pub active: bool,
    pub barrier_id: Option<String>,
    pub frozen_mounts: Vec<String>,
    pub remaining_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartReceiveRequest {
    pub volume_id: String,
    pub listen: String,
    pub port: u16,
    pub token: Option<String>,
    pub bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartReceiveResponse {
    pub repl_id: String,
    pub role: String,
    pub volume_id: String,
    pub target: String,
    pub listen: String,
    pub port: u16,
    pub token: String,
    #[serde(default = "default_replication_mode")]
    pub replication_mode: String,
    #[serde(default)]
    pub target_cluster: Option<String>,
    #[serde(default)]
    pub gateway_endpoint: Option<String>,
    #[serde(default)]
    pub spillover_tier: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartSendRequest {
    pub volume_id: Option<String>,
    pub snapshot_id: Option<String>,
    pub peer: String,
    pub port: u16,
    pub token: String,
    pub bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartSendResponse {
    pub repl_id: String,
    pub role: String,
    pub source: String,
    pub peer: String,
    pub port: u16,
    pub bytes_limit: u64,
    #[serde(default = "default_replication_mode")]
    pub replication_mode: String,
    #[serde(default)]
    pub target_cluster: Option<String>,
    #[serde(default)]
    pub gateway_endpoint: Option<String>,
    #[serde(default)]
    pub spillover_tier: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplicationJob {
    pub repl_id: String,
    pub role: String,
    pub state: String,
    pub subject: String,
    pub peer: String,
    pub port: Option<u16>,
    pub bytes: u64,
    #[serde(default)]
    pub bytes_limit: Option<u64>,
    pub error: Option<String>,
    pub started_at_secs: u64,
    #[serde(default)]
    pub started_at_millis: u64,
    #[serde(default)]
    pub updated_at_millis: u64,
    pub finished_at_secs: Option<u64>,
    #[serde(default)]
    pub finished_at_millis: Option<u64>,
    #[serde(default = "default_replication_mode")]
    pub replication_mode: String,
    #[serde(default)]
    pub target_cluster: Option<String>,
    #[serde(default)]
    pub gateway_endpoint: Option<String>,
    #[serde(default)]
    pub spillover_tier: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplicationStatusResponse {
    pub jobs: Vec<ReplicationJob>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplicationDelaySample {
    pub repl_id: String,
    pub role: String,
    pub state: String,
    pub subject: String,
    pub stream_kind: String,
    pub volume_id: Option<String>,
    pub snapshot_id: Option<String>,
    pub peer: String,
    pub port: Option<u16>,
    pub replication_mode: String,
    pub target_cluster: Option<String>,
    pub gateway_endpoint: Option<String>,
    pub spillover_tier: Option<String>,
    pub bytes: u64,
    pub bytes_limit: Option<u64>,
    pub bytes_remaining: Option<u64>,
    pub started_at_millis: u64,
    pub updated_at_millis: u64,
    pub finished_at_millis: Option<u64>,
    pub elapsed_millis: u64,
    pub idle_millis: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplicationDelayResponse {
    pub samples: Vec<ReplicationDelaySample>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StatsTotals {
    #[serde(default)]
    pub attention_required: bool,
    #[serde(default)]
    pub attention_failed_jobs: u64,
    #[serde(default)]
    pub attention_idle_jobs: u64,
    #[serde(default)]
    pub attention_idle_threshold_millis: u64,
    pub jobs_total: u64,
    pub jobs_active: u64,
    pub jobs_succeeded: u64,
    pub jobs_failed: u64,
    pub bytes: u64,
    pub bytes_limit_known: u64,
    pub bytes_limit: u64,
    pub bytes_remaining: u64,
    pub max_elapsed_millis: u64,
    pub max_idle_millis: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatsNode {
    pub kind: String,
    pub name: String,
    pub totals: StatsTotals,
    pub children: Vec<StatsNode>,
    pub samples: Vec<ReplicationDelaySample>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatsHierarchy {
    pub placement: StatsNode,
    pub logical: StatsNode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatsResponse {
    pub generated_at_millis: u64,
    pub summary: StatsTotals,
    pub hierarchy: StatsHierarchy,
    pub compactions: SnapshotCompactionStatusResponse,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationModeSpec {
    pub scope: String,
    pub mode: String,
    pub generation: u64,
    pub updated_at_secs: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetReplicationModeRequest {
    pub scope: Option<String>,
    pub mode: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplicationModeResponse {
    pub policy: ReplicationModeSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplicationModeStatusResponse {
    pub policies: Vec<ReplicationModeSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationRouteSpec {
    pub scope: String,
    pub target_cluster: String,
    pub gateway_endpoint: String,
    pub spillover_tier: String,
    pub generation: u64,
    pub updated_at_secs: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetReplicationRouteRequest {
    pub scope: Option<String>,
    pub target_cluster: String,
    pub gateway_endpoint: String,
    pub spillover_tier: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplicationRouteResponse {
    pub route: ReplicationRouteSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplicationRouteStatusResponse {
    pub routes: Vec<ReplicationRouteSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogStreamResponse {
    pub entries: Vec<ZccusanLogEntry>,
}

#[derive(Clone, Debug)]
pub struct HttpControlClient {
    endpoint: HttpEndpoint,
    timeout: Duration,
}

impl HttpControlClient {
    pub fn new(base_url: impl AsRef<str>) -> Result<Self, String> {
        Ok(Self {
            endpoint: parse_http_endpoint(base_url.as_ref())?,
            timeout: Duration::from_secs(30),
        })
    }

    pub fn health(&self) -> Result<HealthResponse, String> {
        self.request_json::<(), HealthResponse>("GET", "/v1/healthz", None)
    }

    pub fn create_snapshot(
        &self,
        request: &CreateSnapshotRequest,
    ) -> Result<CreateSnapshotResponse, String> {
        self.request_json("POST", "/v1/snapshots", Some(request))
    }

    pub fn delete_snapshot(&self, snapshot_id: &str) -> Result<DeleteSnapshotResponse, String> {
        let path = format!("/v1/snapshots/{}", path_segment(snapshot_id));
        self.request_json::<(), DeleteSnapshotResponse>("DELETE", &path, None)
    }

    pub fn create_snapshot_device(
        &self,
        request: &CreateSnapshotDeviceRequest,
    ) -> Result<SnapshotDeviceResponse, String> {
        self.request_json("POST", "/v1/snapshot-devices", Some(request))
    }

    pub fn snapshot_devices(&self) -> Result<SnapshotDeviceStatusResponse, String> {
        self.request_json::<(), SnapshotDeviceStatusResponse>("GET", "/v1/snapshot-devices", None)
    }

    pub fn snapshot_device(&self, device_id: &str) -> Result<SnapshotDeviceStatusResponse, String> {
        let path = format!("/v1/snapshot-devices/{}", path_segment(device_id));
        self.request_json::<(), SnapshotDeviceStatusResponse>("GET", &path, None)
    }

    pub fn delete_snapshot_device(
        &self,
        device_id: &str,
    ) -> Result<DeleteSnapshotDeviceResponse, String> {
        let path = format!("/v1/snapshot-devices/{}", path_segment(device_id));
        self.request_json::<(), DeleteSnapshotDeviceResponse>("DELETE", &path, None)
    }

    pub fn start_snapshot_compaction(
        &self,
        device_id: &str,
        request: &StartSnapshotCompactionRequest,
    ) -> Result<SnapshotCompactionResponse, String> {
        let path = format!("/v1/snapshot-devices/{}/compact", path_segment(device_id));
        self.request_json("POST", &path, Some(request))
    }

    pub fn snapshot_compactions(&self) -> Result<SnapshotCompactionStatusResponse, String> {
        self.request_json::<(), SnapshotCompactionStatusResponse>("GET", "/v1/compactions", None)
    }

    pub fn snapshot_compaction(
        &self,
        job_id: &str,
    ) -> Result<SnapshotCompactionStatusResponse, String> {
        let path = format!("/v1/compactions/{}", path_segment(job_id));
        self.request_json::<(), SnapshotCompactionStatusResponse>("GET", &path, None)
    }

    pub fn update_snapshot_compaction(
        &self,
        job_id: &str,
        request: &UpdateSnapshotCompactionRequest,
    ) -> Result<SnapshotCompactionResponse, String> {
        let path = format!("/v1/compactions/{}", path_segment(job_id));
        self.request_json("PUT", &path, Some(request))
    }

    pub fn freeze(&self, request: &FreezeRequest) -> Result<FreezeResponse, String> {
        self.request_json("POST", "/v1/freeze", Some(request))
    }

    pub fn release(&self, barrier_id: &str) -> Result<FreezeResponse, String> {
        let path = format!("/v1/freeze/{}", path_segment(barrier_id));
        self.request_json::<(), FreezeResponse>("DELETE", &path, None)
    }

    pub fn freeze_status(&self) -> Result<FreezeStatusResponse, String> {
        self.request_json::<(), FreezeStatusResponse>("GET", "/v1/freeze", None)
    }

    pub fn start_receive(
        &self,
        request: &StartReceiveRequest,
    ) -> Result<StartReceiveResponse, String> {
        self.request_json("POST", "/v1/streams/receive", Some(request))
    }

    pub fn start_send(&self, request: &StartSendRequest) -> Result<StartSendResponse, String> {
        self.request_json("POST", "/v1/streams/send", Some(request))
    }

    pub fn replication_status(
        &self,
        repl_id: Option<&str>,
    ) -> Result<ReplicationStatusResponse, String> {
        let path = match repl_id {
            Some(repl_id) => format!("/v1/streams/{}", path_segment(repl_id)),
            None => "/v1/streams".to_string(),
        };
        self.request_json::<(), ReplicationStatusResponse>("GET", &path, None)
    }

    pub fn replication_delay(&self) -> Result<ReplicationDelayResponse, String> {
        self.request_json::<(), ReplicationDelayResponse>("GET", "/v1/replication/delay", None)
    }

    pub fn stats(&self) -> Result<StatsResponse, String> {
        self.request_json::<(), StatsResponse>("GET", "/v1/stats", None)
    }

    pub fn replication_modes(&self) -> Result<ReplicationModeStatusResponse, String> {
        self.request_json::<(), ReplicationModeStatusResponse>("GET", "/v1/replication/modes", None)
    }

    pub fn set_replication_mode(
        &self,
        request: &SetReplicationModeRequest,
    ) -> Result<ReplicationModeResponse, String> {
        self.request_json("PUT", "/v1/replication/modes", Some(request))
    }

    pub fn replication_routes(&self) -> Result<ReplicationRouteStatusResponse, String> {
        self.request_json::<(), ReplicationRouteStatusResponse>(
            "GET",
            "/v1/replication/routes",
            None,
        )
    }

    pub fn set_replication_route(
        &self,
        request: &SetReplicationRouteRequest,
    ) -> Result<ReplicationRouteResponse, String> {
        self.request_json("PUT", "/v1/replication/routes", Some(request))
    }

    pub fn logstream(&self) -> Result<LogStreamResponse, String> {
        self.request_json::<(), LogStreamResponse>("GET", "/v1/logstream", None)
    }

    fn request_json<T, R>(&self, method: &str, path: &str, body: Option<&T>) -> Result<R, String>
    where
        T: Serialize,
        R: for<'de> Deserialize<'de>,
    {
        let body = match body {
            Some(body) => serde_json::to_vec(body).map_err(|e| format!("encode request: {e}"))?,
            None => Vec::new(),
        };
        let full_path = self.endpoint.join_path(path);
        let mut stream = TcpStream::connect(&self.endpoint.authority)
            .map_err(|e| format!("connect {}: {e}", self.endpoint.authority))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|e| format!("set read timeout: {e}"))?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(|e| format!("set write timeout: {e}"))?;
        write!(
            stream,
            "{method} {full_path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: zcblock-control-client/{}\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.endpoint.host_header,
            env!("CARGO_PKG_VERSION"),
            body.len()
        )
        .map_err(|e| format!("write request headers: {e}"))?;
        if !body.is_empty() {
            stream
                .write_all(&body)
                .map_err(|e| format!("write request body: {e}"))?;
        }
        stream.flush().map_err(|e| format!("flush request: {e}"))?;

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .map_err(|e| format!("read response: {e}"))?;
        parse_json_response(&response)
    }
}

#[derive(Clone, Debug)]
struct HttpEndpoint {
    authority: String,
    host_header: String,
    base_path: String,
}

impl HttpEndpoint {
    fn join_path(&self, path: &str) -> String {
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        if self.base_path.is_empty() || self.base_path == "/" {
            path
        } else if path == "/" {
            self.base_path.clone()
        } else {
            format!("{}{}", self.base_path, path)
        }
    }
}

fn parse_http_endpoint(value: &str) -> Result<HttpEndpoint, String> {
    let rest = value
        .strip_prefix("http://")
        .ok_or_else(|| "control URL must start with http://".to_string())?;
    let (authority, raw_path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty() {
        return Err("control URL host must not be empty".to_string());
    }
    let authority = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    let base_path = if raw_path.is_empty() {
        String::new()
    } else {
        format!("/{}", raw_path.trim_end_matches('/'))
    };
    Ok(HttpEndpoint {
        host_header: authority.clone(),
        authority,
        base_path,
    })
}

fn parse_json_response<R>(response: &[u8]) -> Result<R, String>
where
    R: for<'de> Deserialize<'de>,
{
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "invalid HTTP response: missing header terminator".to_string())?;
    let header = std::str::from_utf8(&response[..split])
        .map_err(|e| format!("invalid HTTP response header: {e}"))?;
    let body = &response[split + 4..];
    let status_line = header
        .lines()
        .next()
        .ok_or_else(|| "invalid HTTP response: missing status line".to_string())?;
    let mut status_parts = status_line.split_whitespace();
    let _version = status_parts.next();
    let status = status_parts
        .next()
        .ok_or_else(|| "invalid HTTP response: missing status code".to_string())?
        .parse::<u16>()
        .map_err(|_| "invalid HTTP response: bad status code".to_string())?;
    if (200..300).contains(&status) {
        serde_json::from_slice(body).map_err(|e| format!("decode response: {e}"))
    } else if let Ok(error) = serde_json::from_slice::<ErrorResponse>(body) {
        Err(format!(
            "control API returned HTTP {status}: {}",
            error.error
        ))
    } else {
        Err(format!(
            "control API returned HTTP {status}: {}",
            String::from_utf8_lossy(body)
        ))
    }
}

fn path_segment(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn default_replication_mode() -> String {
    "async".to_string()
}
