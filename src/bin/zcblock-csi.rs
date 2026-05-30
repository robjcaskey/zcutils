use k8s_csi::v1_3_0::controller_server::{Controller, ControllerServer};
use k8s_csi::v1_3_0::identity_server::{Identity, IdentityServer};
use k8s_csi::v1_3_0::node_server::{Node, NodeServer};
use k8s_csi::v1_3_0::{
    CapacityRange, ControllerExpandVolumeRequest, ControllerExpandVolumeResponse,
    ControllerGetCapabilitiesRequest, ControllerGetCapabilitiesResponse,
    ControllerGetVolumeRequest, ControllerGetVolumeResponse, ControllerPublishVolumeRequest,
    ControllerPublishVolumeResponse, ControllerServiceCapability, ControllerUnpublishVolumeRequest,
    ControllerUnpublishVolumeResponse, CreateSnapshotRequest, CreateSnapshotResponse,
    CreateVolumeRequest, CreateVolumeResponse, DeleteSnapshotRequest, DeleteSnapshotResponse,
    DeleteVolumeRequest, DeleteVolumeResponse, GetCapacityRequest, GetCapacityResponse,
    GetPluginCapabilitiesRequest, GetPluginCapabilitiesResponse, GetPluginInfoRequest,
    GetPluginInfoResponse, ListSnapshotsRequest, ListSnapshotsResponse, ListVolumesRequest,
    ListVolumesResponse, NodeExpandVolumeRequest, NodeExpandVolumeResponse,
    NodeGetCapabilitiesRequest, NodeGetCapabilitiesResponse, NodeGetInfoRequest,
    NodeGetInfoResponse, NodeGetVolumeStatsRequest, NodeGetVolumeStatsResponse,
    NodePublishVolumeRequest, NodePublishVolumeResponse, NodeServiceCapability,
    NodeStageVolumeRequest, NodeStageVolumeResponse, NodeUnpublishVolumeRequest,
    NodeUnpublishVolumeResponse, NodeUnstageVolumeRequest, NodeUnstageVolumeResponse,
    PluginCapability, ProbeRequest, ProbeResponse, Snapshot, Topology,
    ValidateVolumeCapabilitiesRequest, ValidateVolumeCapabilitiesResponse, Volume,
    VolumeCapability, VolumeContentSource, VolumeUsage, controller_service_capability,
    list_snapshots_response, list_volumes_response, node_service_capability, plugin_capability,
    validate_volume_capabilities_response, volume_capability, volume_content_source, volume_usage,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::UnixListener;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tonic::transport::server::Connected;
use tonic::{Request, Response, Status};
use zcutils::block::control as control_api;
use zcutils::{
    ZcStreamEncryption, zc_pit_is_reflink_unsupported, zc_pit_reflink_file,
    zc_stream_bind_listener, zc_stream_generate_token, zc_stream_receive_listener_to_writer,
    zc_stream_send_reader_to_tcp,
};

const DRIVER_NAME: &str = "io.zcutils.zcblock";
const TOPOLOGY_KEY: &str = "topology.zcutils.io/node";
const DEFAULT_ENDPOINT: &str = "unix:///csi/csi.sock";
const DEFAULT_STATE_DIR: &str = "/var/lib/zcblock-csi";
const DEFAULT_CONTROL_SOCKET_NAME: &str = "control.sock";
const DEFAULT_FREEZE_MAX_TTL_MS: u64 = 5_000;
const FREEZE_COMMAND_TIMEOUT_MS: u64 = 250;
const DEFAULT_CONFIGFS_ROOT: &str = "/sys/kernel/config/zcbrd";
const DEFAULT_DEV_ROOT: &str = "/dev";
const DEFAULT_RAW_ALLOWLIST: &str = "/etc/zcblock-csi/allowed-raw-partitions.txt";
const DEFAULT_SIZE_MIB: u64 = 256;
const DEFAULT_BLOCKSIZE: u64 = 4096;
const DEFAULT_QUEUES: u64 = 8;
const DEFAULT_QUEUE_DEPTH: u64 = 512;
const DEFAULT_DESCRIPTOR_MODE: &str = "advertise";
const DEFAULT_REPLICATION_BUFFER_BYTES: usize = 1024 * 1024;
const DEFAULT_SNAPSHOT_MODE: &str = "auto";
const MIB: u64 = 1024 * 1024;
const BLKGETSIZE64: libc::c_ulong = 0x80081272;

#[derive(Clone, Debug)]
struct Config {
    driver_name: String,
    socket_path: PathBuf,
    node_id: String,
    state_dir: PathBuf,
    control_socket_path: PathBuf,
    control_url: Option<String>,
    freeze_max_ttl_ms: u64,
    configfs_root: PathBuf,
    dev_root: PathBuf,
    raw_allowlist: PathBuf,
    snapshot_mode: String,
}

impl Config {
    fn from_args() -> Result<Self, String> {
        let mut driver_name =
            env::var("CSI_DRIVER_NAME").unwrap_or_else(|_| DRIVER_NAME.to_string());
        let mut endpoint =
            env::var("CSI_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string());
        let mut node_id = env::var("NODE_ID")
            .or_else(|_| env::var("KUBE_NODE_NAME"))
            .unwrap_or_else(|_| local_hostname());
        let mut state_dir = PathBuf::from(
            env::var("ZCBLOCK_CSI_STATE_DIR")
                .or_else(|_| env::var("ZCBRD_CSI_STATE_DIR"))
                .unwrap_or_else(|_| DEFAULT_STATE_DIR.into()),
        );
        let mut control_socket_path = env::var("ZCBLOCK_CSI_CONTROL_SOCKET")
            .ok()
            .map(PathBuf::from);
        let mut control_url = env::var("ZCBLOCK_CONTROL_URL").ok();
        let mut freeze_max_ttl_ms = env::var("ZCBLOCK_CSI_FREEZE_MAX_TTL_MS")
            .ok()
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| "ZCBLOCK_CSI_FREEZE_MAX_TTL_MS must be an integer".to_string())
            })
            .transpose()?
            .unwrap_or(DEFAULT_FREEZE_MAX_TTL_MS);
        let mut configfs_root = PathBuf::from(
            env::var("ZCBLOCK_CONFIGFS_ROOT")
                .or_else(|_| env::var("ZCBRD_CONFIGFS_ROOT"))
                .unwrap_or_else(|_| DEFAULT_CONFIGFS_ROOT.into()),
        );
        let mut dev_root = PathBuf::from(
            env::var("ZCBLOCK_DEV_ROOT")
                .or_else(|_| env::var("ZCBRD_DEV_ROOT"))
                .unwrap_or_else(|_| DEFAULT_DEV_ROOT.into()),
        );
        let mut raw_allowlist = PathBuf::from(
            env::var("ZCBLOCK_RAW_ALLOWLIST")
                .or_else(|_| env::var("ZCBRD_RAW_ALLOWLIST"))
                .unwrap_or_else(|_| DEFAULT_RAW_ALLOWLIST.into()),
        );
        let mut snapshot_mode = env::var("ZCBLOCK_CSI_SNAPSHOT_MODE")
            .unwrap_or_else(|_| DEFAULT_SNAPSHOT_MODE.to_string());
        snapshot_mode = normalize_snapshot_mode(&snapshot_mode)?;

        let args: Vec<String> = env::args().skip(1).collect();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if let Some(value) = arg.strip_prefix("--driver-name=") {
                driver_name = value.to_string();
            } else if let Some(value) = arg.strip_prefix("--endpoint=") {
                endpoint = value.to_string();
            } else if let Some(value) = arg.strip_prefix("--node-id=") {
                node_id = value.to_string();
            } else if let Some(value) = arg.strip_prefix("--state-dir=") {
                state_dir = PathBuf::from(value);
            } else if let Some(value) = arg.strip_prefix("--control-socket=") {
                control_socket_path = Some(PathBuf::from(value));
            } else if let Some(value) = arg.strip_prefix("--control-url=") {
                control_url = Some(value.to_string());
            } else if let Some(value) = arg.strip_prefix("--freeze-max-ttl-ms=") {
                freeze_max_ttl_ms = value
                    .parse::<u64>()
                    .map_err(|_| "--freeze-max-ttl-ms must be an integer".to_string())?;
            } else if let Some(value) = arg.strip_prefix("--configfs-root=") {
                configfs_root = PathBuf::from(value);
            } else if let Some(value) = arg.strip_prefix("--dev-root=") {
                dev_root = PathBuf::from(value);
            } else if let Some(value) = arg.strip_prefix("--raw-allowlist=") {
                raw_allowlist = PathBuf::from(value);
            } else if let Some(value) = arg.strip_prefix("--snapshot-mode=") {
                snapshot_mode = normalize_snapshot_mode(value)?;
            } else if arg == "--driver-name" {
                i += 1;
                driver_name = args
                    .get(i)
                    .ok_or("--driver-name requires a value")?
                    .to_string();
            } else if arg == "--endpoint" {
                i += 1;
                endpoint = args
                    .get(i)
                    .ok_or("--endpoint requires a value")?
                    .to_string();
            } else if arg == "--node-id" {
                i += 1;
                node_id = args.get(i).ok_or("--node-id requires a value")?.to_string();
            } else if arg == "--state-dir" {
                i += 1;
                state_dir = PathBuf::from(args.get(i).ok_or("--state-dir requires a value")?);
            } else if arg == "--control-socket" {
                i += 1;
                control_socket_path = Some(PathBuf::from(
                    args.get(i).ok_or("--control-socket requires a value")?,
                ));
            } else if arg == "--control-url" {
                i += 1;
                control_url = Some(
                    args.get(i)
                        .ok_or("--control-url requires a value")?
                        .to_string(),
                );
            } else if arg == "--freeze-max-ttl-ms" {
                i += 1;
                freeze_max_ttl_ms = args
                    .get(i)
                    .ok_or("--freeze-max-ttl-ms requires a value")?
                    .parse::<u64>()
                    .map_err(|_| "--freeze-max-ttl-ms must be an integer".to_string())?;
            } else if arg == "--configfs-root" {
                i += 1;
                configfs_root =
                    PathBuf::from(args.get(i).ok_or("--configfs-root requires a value")?);
            } else if arg == "--dev-root" {
                i += 1;
                dev_root = PathBuf::from(args.get(i).ok_or("--dev-root requires a value")?);
            } else if arg == "--raw-allowlist" {
                i += 1;
                raw_allowlist =
                    PathBuf::from(args.get(i).ok_or("--raw-allowlist requires a value")?);
            } else if arg == "--snapshot-mode" {
                i += 1;
                snapshot_mode = normalize_snapshot_mode(
                    args.get(i).ok_or("--snapshot-mode requires a value")?,
                )?;
            } else {
                return Err(format!("unknown argument: {arg}"));
            }
            i += 1;
        }

        if node_id.is_empty() {
            return Err("node id must not be empty".to_string());
        }
        if driver_name.is_empty() {
            return Err("driver name must not be empty".to_string());
        }
        if freeze_max_ttl_ms == 0 {
            return Err("freeze max ttl must be greater than zero".to_string());
        }

        let control_socket_path =
            control_socket_path.unwrap_or_else(|| state_dir.join(DEFAULT_CONTROL_SOCKET_NAME));
        if !control_socket_path.is_absolute() {
            return Err(format!(
                "control socket must be an absolute path: {}",
                control_socket_path.display()
            ));
        }
        if control_socket_path == parse_unix_endpoint(&endpoint)? {
            return Err("control socket must be different from CSI endpoint".to_string());
        }
        if let Some(url) = control_url.as_ref() {
            control_api::HttpControlClient::new(url)
                .map_err(|e| format!("invalid control URL {url}: {e}"))?;
        }

        Ok(Self {
            driver_name,
            socket_path: parse_unix_endpoint(&endpoint)?,
            node_id,
            state_dir,
            control_socket_path,
            control_url,
            freeze_max_ttl_ms,
            configfs_root,
            dev_root,
            raw_allowlist,
            snapshot_mode,
        })
    }

    fn volumes_dir(&self) -> PathBuf {
        self.state_dir.join("volumes")
    }

    fn state_path(&self, volume_id: &str) -> PathBuf {
        self.volumes_dir().join(format!("{volume_id}.conf"))
    }

    fn files_dir(&self) -> PathBuf {
        self.state_dir.join("files")
    }

    fn snapshots_dir(&self) -> PathBuf {
        self.state_dir.join("snapshots")
    }

    fn snapshot_state_path(&self, snapshot_id: &str) -> PathBuf {
        self.snapshots_dir().join(format!("{snapshot_id}.conf"))
    }

    fn snapshot_images_dir(&self) -> PathBuf {
        self.snapshots_dir().join("images")
    }

    fn topology(&self) -> Topology {
        let mut segments = BTreeMap::new();
        segments.insert(TOPOLOGY_KEY.to_string(), self.node_id.clone());
        Topology { segments }
    }
}

#[derive(Clone, Debug)]
struct ZcblockCsi {
    cfg: Arc<Config>,
    repl: Arc<ReplicationManager>,
}

#[derive(Debug)]
struct UnixIo(tokio::net::UnixStream);

#[derive(Clone, Debug)]
struct UnixConnectInfo;

#[derive(Clone, Debug, PartialEq, Eq)]
struct VolumeSpec {
    backend: String,
    volume_id: String,
    name_hex: String,
    device_name: String,
    capacity_bytes: i64,
    size_mib: u64,
    blocksize: u64,
    queues: u64,
    queue_depth: u64,
    descriptor_mode: String,
    file_path: Option<String>,
    raw_device: Option<String>,
    staging_path: Option<String>,
    restore_path: Option<String>,
    restore_snapshot_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SnapshotSpec {
    snapshot_id: String,
    name_hex: String,
    source_volume_id: String,
    source_backend: String,
    size_bytes: i64,
    snapshot_path: String,
    snapshot_mode: String,
    creation_time_secs: i64,
    ready_to_use: bool,
}

#[derive(Clone, Debug)]
enum AccessKind {
    Block,
    Mount { fs_type: String, flags: Vec<String> },
}

#[derive(Debug)]
struct FreezeManager {
    cfg: Arc<Config>,
    state: Mutex<FreezeState>,
    op: Mutex<()>,
}

#[derive(Clone, Debug, Default)]
struct FreezeState {
    active: Option<ActiveFreeze>,
}

#[derive(Clone, Debug)]
struct ActiveFreeze {
    barrier_id: String,
    deadline: Instant,
    frozen_mounts: Vec<PathBuf>,
}

#[derive(Debug)]
struct FreezeReport {
    barrier_id: String,
    frozen_mounts: Vec<PathBuf>,
    remaining_ms: u64,
}

#[derive(Debug, Default)]
struct ReplicationManager {
    jobs: StdMutex<BTreeMap<String, ReplicationJob>>,
}

#[derive(Clone, Debug)]
struct ReplicationJob {
    repl_id: String,
    role: String,
    state: String,
    subject: String,
    peer: String,
    port: Option<u16>,
    bytes: u64,
    error: Option<String>,
    started_at_secs: u64,
    finished_at_secs: Option<u64>,
}

#[derive(Debug)]
enum ControlCommand {
    Freeze {
        barrier_id: String,
        ttl_ms: u64,
    },
    Release {
        barrier_id: String,
    },
    Status,
    ReplRecv {
        volume_id: String,
        listen: String,
        port: u16,
        token: Option<String>,
        bytes: Option<u64>,
    },
    ReplSend {
        volume_id: Option<String>,
        snapshot_id: Option<String>,
        peer: String,
        port: u16,
        token: String,
        bytes: Option<u64>,
    },
    ReplStatus {
        repl_id: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::from_args().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let cfg = Arc::new(cfg);
    fs::create_dir_all(cfg.volumes_dir())?;
    if let Some(parent) = cfg.socket_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if socket_exists(&cfg.socket_path)? {
        fs::remove_file(&cfg.socket_path)?;
    }
    if let Some(parent) = cfg.control_socket_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if socket_exists(&cfg.control_socket_path)? {
        fs::remove_file(&cfg.control_socket_path)?;
    }

    let listener = UnixListener::bind(&cfg.socket_path)?;
    let incoming = UnixListenerStream::new(listener).map(|stream| stream.map(UnixIo));
    let control_listener = UnixListener::bind(&cfg.control_socket_path)?;
    let freeze = Arc::new(FreezeManager::new(cfg.clone()));
    freeze.thaw_stale_mounts_on_startup().await;
    let driver = ZcblockCsi {
        cfg,
        repl: Arc::new(ReplicationManager::default()),
    };
    tokio::spawn(run_control_server(control_listener, driver.clone(), freeze));

    eprintln!(
        "{} {} listening on {} control {} control_url={} max_freeze_ttl_ms={} snapshot_mode={}",
        driver.cfg.driver_name,
        env!("CARGO_PKG_VERSION"),
        driver.cfg.socket_path.display(),
        driver.cfg.control_socket_path.display(),
        driver.cfg.control_url.as_deref().unwrap_or("-"),
        driver.cfg.freeze_max_ttl_ms,
        driver.cfg.snapshot_mode
    );

    Server::builder()
        .add_service(IdentityServer::new(driver.clone()))
        .add_service(ControllerServer::new(driver.clone()))
        .add_service(NodeServer::new(driver))
        .serve_with_incoming_shutdown(incoming, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    Ok(())
}

async fn run_control_server(
    listener: UnixListener,
    driver: ZcblockCsi,
    freeze: Arc<FreezeManager>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let driver = driver.clone();
                let freeze = freeze.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_control_connection(stream, driver, freeze).await {
                        eprintln!("control connection failed: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("control accept failed: {e}");
                break;
            }
        }
    }
}

async fn handle_control_connection(
    stream: tokio::net::UnixStream,
    driver: ZcblockCsi,
    freeze: Arc<FreezeManager>,
) -> io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let response =
        match tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await {
            Ok(Ok(0)) => "ERR empty command\n".to_string(),
            Ok(Ok(_)) => handle_control_line(&driver, &freeze, &line).await,
            Ok(Err(e)) => format!("ERR read failed: {e}\n"),
            Err(_) => "ERR command read timed out\n".to_string(),
        };
    write_half.write_all(response.as_bytes()).await?;
    write_half.shutdown().await
}

async fn handle_control_line(
    driver: &ZcblockCsi,
    freeze: &Arc<FreezeManager>,
    line: &str,
) -> String {
    match parse_control_command(line) {
        Ok(ControlCommand::Freeze { barrier_id, ttl_ms }) => {
            match freeze.freeze(barrier_id, ttl_ms).await {
                Ok(report) => format!(
                    "OK barrier={} frozen={} remaining_ms={} mounts={}\n",
                    report.barrier_id,
                    report.frozen_mounts.len(),
                    report.remaining_ms,
                    join_paths(&report.frozen_mounts)
                ),
                Err(e) => format!("ERR {e}\n"),
            }
        }
        Ok(ControlCommand::Release { barrier_id }) => match freeze.release(&barrier_id).await {
            Ok(report) => format!(
                "OK barrier={} thawed={} remaining_ms={} mounts={}\n",
                report.barrier_id,
                report.frozen_mounts.len(),
                report.remaining_ms,
                join_paths(&report.frozen_mounts)
            ),
            Err(e) => format!("ERR {e}\n"),
        },
        Ok(ControlCommand::Status) => freeze.status().await,
        Ok(ControlCommand::ReplRecv {
            volume_id,
            listen,
            port,
            token,
            bytes,
        }) => match driver
            .start_replication_receive(volume_id, listen, port, token, bytes)
            .await
        {
            Ok(response) => response,
            Err(e) => format!("ERR {}\n", e.message()),
        },
        Ok(ControlCommand::ReplSend {
            volume_id,
            snapshot_id,
            peer,
            port,
            token,
            bytes,
        }) => match driver
            .start_replication_send(volume_id, snapshot_id, peer, port, token, bytes)
            .await
        {
            Ok(response) => response,
            Err(e) => format!("ERR {}\n", e.message()),
        },
        Ok(ControlCommand::ReplStatus { repl_id }) => driver.repl.status_response(repl_id),
        Err(e) => format!("ERR {e}\n"),
    }
}

impl FreezeManager {
    fn new(cfg: Arc<Config>) -> Self {
        Self {
            cfg,
            state: Mutex::new(FreezeState::default()),
            op: Mutex::new(()),
        }
    }

    async fn freeze(
        self: &Arc<Self>,
        barrier_id: String,
        ttl_ms: u64,
    ) -> Result<FreezeReport, String> {
        validate_barrier_id(&barrier_id)?;
        if ttl_ms == 0 {
            return Err("ttl_ms must be greater than zero".to_string());
        }
        if ttl_ms > self.cfg.freeze_max_ttl_ms {
            return Err(format!(
                "ttl_ms {ttl_ms} exceeds configured max {}",
                self.cfg.freeze_max_ttl_ms
            ));
        }

        let _guard = self.op.lock().await;
        self.expire_active_under_op().await;

        {
            let state = self.state.lock().await;
            if let Some(active) = state.active.as_ref() {
                if active.barrier_id == barrier_id {
                    return Ok(FreezeReport {
                        barrier_id,
                        frozen_mounts: active.frozen_mounts.clone(),
                        remaining_ms: remaining_ms(active.deadline),
                    });
                }
                return Err(format!(
                    "busy active_barrier={} remaining_ms={}",
                    active.barrier_id,
                    remaining_ms(active.deadline)
                ));
            }
        }

        let deadline = Instant::now() + Duration::from_millis(ttl_ms);
        let mounts = self.discover_freezable_mounts().await?;
        let mut frozen = Vec::new();
        for mount in mounts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.thaw_mounts(&frozen).await;
                return Err("deadline elapsed before freeze completed".to_string());
            }
            let command_timeout = remaining.min(Duration::from_millis(FREEZE_COMMAND_TIMEOUT_MS));
            if let Err(e) = fsfreeze_path(&mount, true, command_timeout).await {
                let _ = self.thaw_mounts(&frozen).await;
                return Err(format!("freeze {} failed: {e}", mount.display()));
            }
            frozen.push(mount);
        }

        let active = ActiveFreeze {
            barrier_id: barrier_id.clone(),
            deadline,
            frozen_mounts: frozen.clone(),
        };
        {
            let mut state = self.state.lock().await;
            state.active = Some(active);
        }

        let manager = self.clone();
        let auto_barrier_id = barrier_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            manager.release_if_expired(&auto_barrier_id).await;
        });

        Ok(FreezeReport {
            barrier_id,
            frozen_mounts: frozen,
            remaining_ms: remaining_ms(deadline),
        })
    }

    async fn release(&self, barrier_id: &str) -> Result<FreezeReport, String> {
        validate_barrier_id(barrier_id)?;
        let _guard = self.op.lock().await;
        self.release_under_op(barrier_id, false).await
    }

    async fn release_if_expired(&self, barrier_id: &str) {
        let _guard = self.op.lock().await;
        if let Err(e) = self.release_under_op(barrier_id, true).await {
            eprintln!("auto-thaw for barrier {barrier_id} failed: {e}");
        }
    }

    async fn release_under_op(
        &self,
        barrier_id: &str,
        only_if_expired: bool,
    ) -> Result<FreezeReport, String> {
        let active = {
            let mut state = self.state.lock().await;
            let Some(active) = state.active.as_ref() else {
                return Ok(FreezeReport {
                    barrier_id: barrier_id.to_string(),
                    frozen_mounts: Vec::new(),
                    remaining_ms: 0,
                });
            };
            if active.barrier_id != barrier_id {
                return Err(format!(
                    "active barrier is {} remaining_ms={}",
                    active.barrier_id,
                    remaining_ms(active.deadline)
                ));
            }
            if only_if_expired && Instant::now() < active.deadline {
                return Ok(FreezeReport {
                    barrier_id: barrier_id.to_string(),
                    frozen_mounts: Vec::new(),
                    remaining_ms: remaining_ms(active.deadline),
                });
            }
            state.active.take().expect("active freeze disappeared")
        };

        let thaw_errors = self.thaw_mounts(&active.frozen_mounts).await;
        if !thaw_errors.is_empty() {
            return Err(format!("thaw errors: {}", thaw_errors.join("; ")));
        }
        Ok(FreezeReport {
            barrier_id: active.barrier_id,
            frozen_mounts: active.frozen_mounts,
            remaining_ms: 0,
        })
    }

    async fn expire_active_under_op(&self) {
        let expired = {
            let mut state = self.state.lock().await;
            match state.active.as_ref() {
                Some(active) if Instant::now() >= active.deadline => state.active.take(),
                _ => None,
            }
        };
        if let Some(active) = expired {
            let errors = self.thaw_mounts(&active.frozen_mounts).await;
            if !errors.is_empty() {
                eprintln!(
                    "expired barrier {} thaw errors: {}",
                    active.barrier_id,
                    errors.join("; ")
                );
            }
        }
    }

    async fn status(&self) -> String {
        let state = self.state.lock().await;
        match state.active.as_ref() {
            Some(active) => format!(
                "OK active=true barrier={} frozen={} remaining_ms={} mounts={}\n",
                active.barrier_id,
                active.frozen_mounts.len(),
                remaining_ms(active.deadline),
                join_paths(&active.frozen_mounts)
            ),
            None => "OK active=false\n".to_string(),
        }
    }

    async fn thaw_stale_mounts_on_startup(&self) {
        let mounts = match self.discover_freezable_mounts().await {
            Ok(mounts) => mounts,
            Err(e) => {
                eprintln!("startup thaw discovery failed: {e}");
                return;
            }
        };
        if mounts.is_empty() {
            return;
        }
        let errors = self.thaw_mounts(&mounts).await;
        if !errors.is_empty() {
            eprintln!("startup thaw errors: {}", errors.join("; "));
        }
    }

    async fn discover_freezable_mounts(&self) -> Result<Vec<PathBuf>, String> {
        let mut mounts = BTreeSet::new();
        match fs::read_dir(self.cfg.volumes_dir()) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| format!("read volume state entry: {e}"))?;
                    if entry.path().extension().and_then(|s| s.to_str()) != Some("conf") {
                        continue;
                    }
                    let body = fs::read_to_string(entry.path())
                        .map_err(|e| format!("read volume state: {e}"))?;
                    let spec = VolumeSpec::from_state(&body)
                        .map_err(|e| format!("parse volume state: {}", e.message()))?;
                    if spec.backend == "raw-block" {
                        continue;
                    }
                    let Some(staging_path) = spec.staging_path.as_ref() else {
                        continue;
                    };
                    let path = PathBuf::from(staging_path);
                    if is_mountpoint(&path).await {
                        mounts.insert(path);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("read volume state dir: {e}")),
        }
        Ok(mounts.into_iter().collect())
    }

    async fn thaw_mounts(&self, mounts: &[PathBuf]) -> Vec<String> {
        let mut errors = Vec::new();
        for mount in mounts.iter().rev() {
            match fsfreeze_path(
                mount,
                false,
                Duration::from_millis(FREEZE_COMMAND_TIMEOUT_MS),
            )
            .await
            {
                Ok(()) => {}
                Err(e) if is_already_thawed_error(&e) => {}
                Err(e) => errors.push(format!("{}: {e}", mount.display())),
            }
        }
        errors
    }
}

impl ReplicationManager {
    fn insert(&self, job: ReplicationJob) {
        let mut jobs = self.jobs.lock().expect("replication job mutex poisoned");
        jobs.insert(job.repl_id.clone(), job);
    }

    fn mark_state(&self, repl_id: &str, state: &str) {
        let mut jobs = self.jobs.lock().expect("replication job mutex poisoned");
        if let Some(job) = jobs.get_mut(repl_id) {
            job.state = state.to_string();
        }
    }

    fn mark_done(&self, repl_id: &str, bytes: u64) {
        let mut jobs = self.jobs.lock().expect("replication job mutex poisoned");
        if let Some(job) = jobs.get_mut(repl_id) {
            job.state = "succeeded".to_string();
            job.bytes = bytes;
            job.error = None;
            job.finished_at_secs = Some(unix_now_secs());
        }
    }

    fn mark_error(&self, repl_id: &str, error: String) {
        let mut jobs = self.jobs.lock().expect("replication job mutex poisoned");
        if let Some(job) = jobs.get_mut(repl_id) {
            job.state = "failed".to_string();
            job.error = Some(error);
            job.finished_at_secs = Some(unix_now_secs());
        }
    }

    fn status_response(&self, repl_id: Option<String>) -> String {
        let jobs = self.jobs.lock().expect("replication job mutex poisoned");
        if let Some(repl_id) = repl_id {
            return match jobs.get(&repl_id) {
                Some(job) => format!("OK {}\n", job.status_fields()),
                None => format!("ERR repl_id {repl_id} not found\n"),
            };
        }

        let mut response = format!("OK jobs={}\n", jobs.len());
        for job in jobs.values() {
            response.push_str("JOB ");
            response.push_str(&job.status_fields());
            response.push('\n');
        }
        response
    }
}

impl ReplicationJob {
    fn status_fields(&self) -> String {
        let mut fields = format!(
            "repl_id={} role={} state={} subject={} peer={} bytes={} started_at_secs={}",
            self.repl_id,
            self.role,
            self.state,
            control_field(&self.subject),
            control_field(&self.peer),
            self.bytes,
            self.started_at_secs
        );
        if let Some(port) = self.port {
            fields.push_str(&format!(" port={port}"));
        }
        if let Some(finished_at_secs) = self.finished_at_secs {
            fields.push_str(&format!(" finished_at_secs={finished_at_secs}"));
        }
        if let Some(error) = self.error.as_ref() {
            fields.push_str(&format!(" error={}", control_field(error)));
        }
        fields
    }
}

struct BoundedWriter<W> {
    inner: W,
    remaining: u64,
}

impl<W> BoundedWriter<W> {
    fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() as u64 > self.remaining {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "replication target byte limit exceeded",
            ));
        }
        let written = self.inner.write(buf)?;
        self.remaining = self.remaining.saturating_sub(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Connected for UnixIo {
    type ConnectInfo = UnixConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        UnixConnectInfo
    }
}

impl AsyncRead for UnixIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for UnixIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[tonic::async_trait]
impl Identity for ZcblockCsi {
    async fn get_plugin_info(
        &self,
        _request: Request<GetPluginInfoRequest>,
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        let mut manifest = BTreeMap::new();
        manifest.insert("backend".to_string(), "zcbrd".to_string());
        Ok(Response::new(GetPluginInfoResponse {
            name: self.cfg.driver_name.clone(),
            vendor_version: env!("CARGO_PKG_VERSION").to_string(),
            manifest,
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _request: Request<GetPluginCapabilitiesRequest>,
    ) -> Result<Response<GetPluginCapabilitiesResponse>, Status> {
        Ok(Response::new(GetPluginCapabilitiesResponse {
            capabilities: vec![
                plugin_service_cap(plugin_capability::service::Type::ControllerService),
                plugin_service_cap(
                    plugin_capability::service::Type::VolumeAccessibilityConstraints,
                ),
            ],
        }))
    }

    async fn probe(
        &self,
        _request: Request<ProbeRequest>,
    ) -> Result<Response<ProbeResponse>, Status> {
        Ok(Response::new(ProbeResponse { ready: Some(true) }))
    }
}

#[tonic::async_trait]
impl Controller for ZcblockCsi {
    async fn create_volume(
        &self,
        request: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        let req = request.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("CreateVolume.name is required"));
        }
        validate_volume_capabilities(&req.volume_capabilities)?;

        let spec = self.spec_from_create_request(&req).await?;
        if let Some(existing) = self.load_volume(&spec.volume_id)? {
            let mut compatible = existing.clone();
            compatible.staging_path = None;
            if compatible.restore_snapshot_id.is_none()
                && compatible.restore_path == spec.restore_path
                && spec.restore_snapshot_id.is_some()
            {
                compatible.restore_snapshot_id = spec.restore_snapshot_id.clone();
            }
            if compatible != spec {
                return Err(Status::already_exists(format!(
                    "volume {} already exists with different parameters",
                    existing.volume_id
                )));
            }
            if existing.restore_snapshot_id != spec.restore_snapshot_id {
                let mut updated = existing.clone();
                updated.restore_path = spec.restore_path.clone();
                updated.restore_snapshot_id = spec.restore_snapshot_id.clone();
                self.save_volume(&updated)?;
            }
            return Ok(Response::new(CreateVolumeResponse {
                volume: Some(self.volume_from_spec(&spec)),
            }));
        }

        if spec.backend == "raw-block" {
            self.ensure_raw_device_unclaimed(&spec)?;
        }
        self.create_backend_storage(&spec).await?;
        self.save_volume(&spec)?;
        Ok(Response::new(CreateVolumeResponse {
            volume: Some(self.volume_from_spec(&spec)),
        }))
    }

    async fn delete_volume(
        &self,
        request: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        let req = request.into_inner();
        if req.volume_id.is_empty() {
            return Err(Status::invalid_argument(
                "DeleteVolume.volume_id is required",
            ));
        }

        if let Some(spec) = self.load_volume(&req.volume_id)? {
            self.delete_backend_storage(&spec).await?;
            let path = self.cfg.state_path(&req.volume_id);
            if let Err(e) = fs::remove_file(&path) {
                if e.kind() != io::ErrorKind::NotFound {
                    return Err(io_status("remove volume state", e));
                }
            }
        }

        Ok(Response::new(DeleteVolumeResponse {}))
    }

    async fn controller_publish_volume(
        &self,
        _request: Request<ControllerPublishVolumeRequest>,
    ) -> Result<Response<ControllerPublishVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "zcblock-csi does not use controller publish/attach",
        ))
    }

    async fn controller_unpublish_volume(
        &self,
        _request: Request<ControllerUnpublishVolumeRequest>,
    ) -> Result<Response<ControllerUnpublishVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "zcblock-csi does not use controller unpublish/detach",
        ))
    }

    async fn validate_volume_capabilities(
        &self,
        request: Request<ValidateVolumeCapabilitiesRequest>,
    ) -> Result<Response<ValidateVolumeCapabilitiesResponse>, Status> {
        let req = request.into_inner();
        if req.volume_id.is_empty() {
            return Err(Status::invalid_argument(
                "ValidateVolumeCapabilities.volume_id is required",
            ));
        }
        if self.load_volume(&req.volume_id)?.is_none() {
            return Err(Status::not_found(format!(
                "volume {} not found",
                req.volume_id
            )));
        }

        match validate_volume_capabilities(&req.volume_capabilities) {
            Ok(()) => Ok(Response::new(ValidateVolumeCapabilitiesResponse {
                confirmed: Some(validate_volume_capabilities_response::Confirmed {
                    volume_context: req.volume_context,
                    volume_capabilities: req.volume_capabilities,
                    parameters: req.parameters,
                }),
                message: String::new(),
            })),
            Err(status) => Ok(Response::new(ValidateVolumeCapabilitiesResponse {
                confirmed: None,
                message: status.message().to_string(),
            })),
        }
    }

    async fn list_volumes(
        &self,
        request: Request<ListVolumesRequest>,
    ) -> Result<Response<ListVolumesResponse>, Status> {
        let req = request.into_inner();
        if req.max_entries < 0 {
            return Err(Status::invalid_argument(
                "ListVolumes.max_entries must not be negative",
            ));
        }

        let mut specs = self.list_volume_specs()?;
        specs.sort_by(|a, b| a.volume_id.cmp(&b.volume_id));
        let start = if req.starting_token.is_empty() {
            0
        } else {
            req.starting_token
                .parse::<usize>()
                .map_err(|_| Status::aborted("invalid ListVolumes.starting_token"))?
        };
        let max = if req.max_entries == 0 {
            specs.len()
        } else {
            req.max_entries as usize
        };
        let end = specs.len().min(start.saturating_add(max));
        let entries = specs[start..end]
            .iter()
            .map(|spec| list_volumes_response::Entry {
                volume: Some(self.volume_from_spec(spec)),
                status: None,
            })
            .collect();
        let next_token = if end < specs.len() {
            end.to_string()
        } else {
            String::new()
        };

        Ok(Response::new(ListVolumesResponse {
            entries,
            next_token,
        }))
    }

    async fn get_capacity(
        &self,
        _request: Request<GetCapacityRequest>,
    ) -> Result<Response<GetCapacityResponse>, Status> {
        Ok(Response::new(GetCapacityResponse {
            available_capacity: i64::MAX,
        }))
    }

    async fn controller_get_capabilities(
        &self,
        _request: Request<ControllerGetCapabilitiesRequest>,
    ) -> Result<Response<ControllerGetCapabilitiesResponse>, Status> {
        Ok(Response::new(ControllerGetCapabilitiesResponse {
            capabilities: vec![
                controller_rpc_cap(controller_service_capability::rpc::Type::CreateDeleteVolume),
                controller_rpc_cap(controller_service_capability::rpc::Type::CreateDeleteSnapshot),
                controller_rpc_cap(controller_service_capability::rpc::Type::ListSnapshots),
            ],
        }))
    }

    async fn create_snapshot(
        &self,
        request: Request<CreateSnapshotRequest>,
    ) -> Result<Response<CreateSnapshotResponse>, Status> {
        let req = request.into_inner();
        if req.source_volume_id.is_empty() {
            return Err(Status::invalid_argument(
                "CreateSnapshot.source_volume_id is required",
            ));
        }
        if req.name.is_empty() {
            return Err(Status::invalid_argument("CreateSnapshot.name is required"));
        }
        let snapshot_id = format!("zcblock-csi-snap-{}", short_hash(&req.name, 24));
        if let Some(existing) = self.load_snapshot(&snapshot_id)? {
            if existing.source_volume_id != req.source_volume_id {
                return Err(Status::already_exists(format!(
                    "snapshot {} already exists for a different source volume",
                    snapshot_id
                )));
            }
            return Ok(Response::new(CreateSnapshotResponse {
                snapshot: Some(existing.to_csi_snapshot()),
            }));
        }

        let spec = if self.cfg.control_url.is_some() {
            self.control_create_snapshot(&req.source_volume_id, &snapshot_id, &req.name)
                .await?
        } else {
            let source = self.load_volume(&req.source_volume_id)?.ok_or_else(|| {
                Status::not_found(format!("volume {} not found", req.source_volume_id))
            })?;
            let spec = self
                .snapshot_volume(&source, &snapshot_id, &req.name)
                .await?;
            self.save_snapshot(&spec)?;
            spec
        };
        Ok(Response::new(CreateSnapshotResponse {
            snapshot: Some(spec.to_csi_snapshot()),
        }))
    }

    async fn delete_snapshot(
        &self,
        request: Request<DeleteSnapshotRequest>,
    ) -> Result<Response<DeleteSnapshotResponse>, Status> {
        let req = request.into_inner();
        if req.snapshot_id.is_empty() {
            return Err(Status::invalid_argument(
                "DeleteSnapshot.snapshot_id is required",
            ));
        }
        if self.cfg.control_url.is_some() {
            self.control_delete_snapshot(&req.snapshot_id).await?;
            return Ok(Response::new(DeleteSnapshotResponse {}));
        }
        if let Some(spec) = self.load_snapshot(&req.snapshot_id)? {
            match fs::remove_file(&spec.snapshot_path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_status("remove snapshot image", e)),
            }
            let state = self.cfg.snapshot_state_path(&req.snapshot_id);
            match fs::remove_file(state) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_status("remove snapshot state", e)),
            }
        }
        Ok(Response::new(DeleteSnapshotResponse {}))
    }

    async fn list_snapshots(
        &self,
        request: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        let req = request.into_inner();
        if req.max_entries < 0 {
            return Err(Status::invalid_argument(
                "ListSnapshots.max_entries must not be negative",
            ));
        }
        let mut snapshots = self.list_snapshot_specs()?;
        if !req.snapshot_id.is_empty() {
            snapshots.retain(|snap| snap.snapshot_id == req.snapshot_id);
        }
        if !req.source_volume_id.is_empty() {
            snapshots.retain(|snap| snap.source_volume_id == req.source_volume_id);
        }
        snapshots.sort_by(|a, b| a.snapshot_id.cmp(&b.snapshot_id));
        let start = if req.starting_token.is_empty() {
            0
        } else {
            req.starting_token
                .parse::<usize>()
                .map_err(|_| Status::aborted("invalid ListSnapshots.starting_token"))?
        };
        let max = if req.max_entries == 0 {
            snapshots.len()
        } else {
            req.max_entries as usize
        };
        let end = snapshots.len().min(start.saturating_add(max));
        let entries = snapshots[start..end]
            .iter()
            .map(|snapshot| list_snapshots_response::Entry {
                snapshot: Some(snapshot.to_csi_snapshot()),
            })
            .collect();
        let next_token = if end < snapshots.len() {
            end.to_string()
        } else {
            String::new()
        };
        Ok(Response::new(ListSnapshotsResponse {
            entries,
            next_token,
        }))
    }

    async fn controller_expand_volume(
        &self,
        _request: Request<ControllerExpandVolumeRequest>,
    ) -> Result<Response<ControllerExpandVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "zcblock-csi does not support expansion yet",
        ))
    }

    async fn controller_get_volume(
        &self,
        request: Request<ControllerGetVolumeRequest>,
    ) -> Result<Response<ControllerGetVolumeResponse>, Status> {
        let req = request.into_inner();
        let spec = self
            .load_volume(&req.volume_id)?
            .ok_or_else(|| Status::not_found(format!("volume {} not found", req.volume_id)))?;

        Ok(Response::new(ControllerGetVolumeResponse {
            volume: Some(self.volume_from_spec(&spec)),
            status: None,
        }))
    }
}

#[tonic::async_trait]
impl Node for ZcblockCsi {
    async fn node_stage_volume(
        &self,
        request: Request<NodeStageVolumeRequest>,
    ) -> Result<Response<NodeStageVolumeResponse>, Status> {
        let req = request.into_inner();
        if req.volume_id.is_empty() {
            return Err(Status::invalid_argument(
                "NodeStageVolume.volume_id is required",
            ));
        }
        if req.staging_target_path.is_empty() {
            return Err(Status::invalid_argument(
                "NodeStageVolume.staging_target_path is required",
            ));
        }
        let access = access_kind(req.volume_capability.as_ref())?;
        let spec = self.spec_for_node(&req.volume_id, &req.volume_context)?;
        if spec.backend == "raw-block" && !matches!(access, AccessKind::Block) {
            return Err(Status::invalid_argument(
                "backend=raw-block supports only volumeMode: Block",
            ));
        }
        let device = self.ensure_backend_device(&spec).await?;
        let mut staged_spec = spec.clone();

        match access {
            AccessKind::Block => {
                fs::create_dir_all(&req.staging_target_path)
                    .map_err(|e| io_status("create staging path", e))?;
                fs::write(
                    Path::new(&req.staging_target_path).join("device"),
                    format!("{}\n", device.display()),
                )
                .map_err(|e| io_status("write staged device marker", e))?;
                staged_spec.staging_path = None;
            }
            AccessKind::Mount { fs_type, flags } => {
                fs::create_dir_all(&req.staging_target_path)
                    .map_err(|e| io_status("create staging path", e))?;
                if !is_mountpoint(&req.staging_target_path).await {
                    let actual_fs = ensure_filesystem(&device, &fs_type).await?;
                    mount_device(
                        &device,
                        Path::new(&req.staging_target_path),
                        &actual_fs,
                        &flags,
                    )
                    .await?;
                }
                staged_spec.staging_path = Some(req.staging_target_path.clone());
            }
        }
        self.save_volume(&staged_spec)?;

        Ok(Response::new(NodeStageVolumeResponse {}))
    }

    async fn node_unstage_volume(
        &self,
        request: Request<NodeUnstageVolumeRequest>,
    ) -> Result<Response<NodeUnstageVolumeResponse>, Status> {
        let req = request.into_inner();
        if req.volume_id.is_empty() {
            return Err(Status::invalid_argument(
                "NodeUnstageVolume.volume_id is required",
            ));
        }
        if req.staging_target_path.is_empty() {
            return Err(Status::invalid_argument(
                "NodeUnstageVolume.staging_target_path is required",
            ));
        }

        if is_mountpoint(&req.staging_target_path).await {
            umount_path(Path::new(&req.staging_target_path)).await?;
        }
        let marker = Path::new(&req.staging_target_path).join("device");
        if let Err(e) = fs::remove_file(marker) {
            if e.kind() != io::ErrorKind::NotFound {
                return Err(io_status("remove staged device marker", e));
            }
        }
        if let Some(spec) = self.load_volume(&req.volume_id)? {
            self.unstage_backend_device(&spec).await?;
            let mut spec = spec;
            spec.staging_path = None;
            self.save_volume(&spec)?;
        }

        Ok(Response::new(NodeUnstageVolumeResponse {}))
    }

    async fn node_publish_volume(
        &self,
        request: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, Status> {
        let req = request.into_inner();
        if req.volume_id.is_empty() {
            return Err(Status::invalid_argument(
                "NodePublishVolume.volume_id is required",
            ));
        }
        if req.target_path.is_empty() {
            return Err(Status::invalid_argument(
                "NodePublishVolume.target_path is required",
            ));
        }
        let access = access_kind(req.volume_capability.as_ref())?;
        let spec = self.spec_for_node(&req.volume_id, &req.volume_context)?;
        if spec.backend == "raw-block" && !matches!(access, AccessKind::Block) {
            return Err(Status::invalid_argument(
                "backend=raw-block supports only volumeMode: Block",
            ));
        }
        let device = self.ensure_backend_device(&spec).await?;

        if is_mountpoint(&req.target_path).await {
            return Ok(Response::new(NodePublishVolumeResponse {}));
        }

        match access {
            AccessKind::Block => {
                let target = Path::new(&req.target_path);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| io_status("create block target parent", e))?;
                }
                OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(target)
                    .map_err(|e| io_status("create block target", e))?;
                bind_mount(&device, target, req.readonly).await?;
            }
            AccessKind::Mount { .. } => {
                if req.staging_target_path.is_empty() {
                    return Err(Status::invalid_argument(
                        "NodePublishVolume.staging_target_path is required for mounted volumes",
                    ));
                }
                let target = Path::new(&req.target_path);
                fs::create_dir_all(target).map_err(|e| io_status("create mount target", e))?;
                bind_mount(Path::new(&req.staging_target_path), target, req.readonly).await?;
            }
        }

        Ok(Response::new(NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        request: Request<NodeUnpublishVolumeRequest>,
    ) -> Result<Response<NodeUnpublishVolumeResponse>, Status> {
        let req = request.into_inner();
        if req.target_path.is_empty() {
            return Err(Status::invalid_argument(
                "NodeUnpublishVolume.target_path is required",
            ));
        }
        let target = Path::new(&req.target_path);
        if is_mountpoint(target).await {
            umount_path(target).await?;
        }
        remove_path_if_exists(target).map_err(|e| io_status("remove publish target", e))?;
        Ok(Response::new(NodeUnpublishVolumeResponse {}))
    }

    async fn node_get_volume_stats(
        &self,
        request: Request<NodeGetVolumeStatsRequest>,
    ) -> Result<Response<NodeGetVolumeStatsResponse>, Status> {
        let req = request.into_inner();
        let stats = stat_volume_path(Path::new(&req.volume_path))?;
        Ok(Response::new(NodeGetVolumeStatsResponse {
            usage: vec![stats],
            volume_condition: None,
        }))
    }

    async fn node_expand_volume(
        &self,
        _request: Request<NodeExpandVolumeRequest>,
    ) -> Result<Response<NodeExpandVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "zcblock-csi does not support expansion yet",
        ))
    }

    async fn node_get_capabilities(
        &self,
        _request: Request<NodeGetCapabilitiesRequest>,
    ) -> Result<Response<NodeGetCapabilitiesResponse>, Status> {
        Ok(Response::new(NodeGetCapabilitiesResponse {
            capabilities: vec![node_rpc_cap(
                node_service_capability::rpc::Type::StageUnstageVolume,
            )],
        }))
    }

    async fn node_get_info(
        &self,
        _request: Request<NodeGetInfoRequest>,
    ) -> Result<Response<NodeGetInfoResponse>, Status> {
        Ok(Response::new(NodeGetInfoResponse {
            node_id: self.cfg.node_id.clone(),
            max_volumes_per_node: 0,
            accessible_topology: Some(self.cfg.topology()),
        }))
    }
}

impl ZcblockCsi {
    fn volume_from_spec(&self, spec: &VolumeSpec) -> Volume {
        Volume {
            capacity_bytes: spec.capacity_bytes,
            volume_id: spec.volume_id.clone(),
            volume_context: spec.to_context(),
            content_source: spec.restore_snapshot_id.as_ref().map(|snapshot_id| {
                VolumeContentSource {
                    r#type: Some(volume_content_source::Type::Snapshot(
                        volume_content_source::SnapshotSource {
                            snapshot_id: snapshot_id.clone(),
                        },
                    )),
                }
            }),
            accessible_topology: vec![self.cfg.topology()],
        }
    }

    fn save_volume(&self, spec: &VolumeSpec) -> Result<(), Status> {
        fs::create_dir_all(self.cfg.volumes_dir())
            .map_err(|e| io_status("create volume state dir", e))?;
        fs::write(self.cfg.state_path(&spec.volume_id), spec.to_state())
            .map_err(|e| io_status("write volume state", e))
    }

    fn load_volume(&self, volume_id: &str) -> Result<Option<VolumeSpec>, Status> {
        let path = self.cfg.state_path(volume_id);
        match fs::read_to_string(&path) {
            Ok(body) => Ok(Some(VolumeSpec::from_state(&body)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_status("read volume state", e)),
        }
    }

    fn list_volume_specs(&self) -> Result<Vec<VolumeSpec>, Status> {
        let mut specs = Vec::new();
        match fs::read_dir(self.cfg.volumes_dir()) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| io_status("read volume state entry", e))?;
                    if entry.path().extension().and_then(|s| s.to_str()) != Some("conf") {
                        continue;
                    }
                    let body = fs::read_to_string(entry.path())
                        .map_err(|e| io_status("read volume state", e))?;
                    specs.push(VolumeSpec::from_state(&body)?);
                }
                Ok(specs)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(specs),
            Err(e) => Err(io_status("read volume state dir", e)),
        }
    }

    fn save_snapshot(&self, spec: &SnapshotSpec) -> Result<(), Status> {
        fs::create_dir_all(self.cfg.snapshots_dir())
            .map_err(|e| io_status("create snapshot state dir", e))?;
        fs::write(
            self.cfg.snapshot_state_path(&spec.snapshot_id),
            spec.to_state(),
        )
        .map_err(|e| io_status("write snapshot state", e))
    }

    fn load_snapshot(&self, snapshot_id: &str) -> Result<Option<SnapshotSpec>, Status> {
        let path = self.cfg.snapshot_state_path(snapshot_id);
        match fs::read_to_string(&path) {
            Ok(body) => Ok(Some(SnapshotSpec::from_state(&body)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_status("read snapshot state", e)),
        }
    }

    async fn control_create_snapshot(
        &self,
        source_volume_id: &str,
        snapshot_id: &str,
        name: &str,
    ) -> Result<SnapshotSpec, Status> {
        let url = self
            .cfg
            .control_url
            .clone()
            .ok_or_else(|| Status::internal("control URL is not configured"))?;
        let request = control_api::CreateSnapshotRequest {
            source_volume_id: source_volume_id.to_string(),
            snapshot_id: snapshot_id.to_string(),
            name: name.to_string(),
        };
        let response = tokio::task::spawn_blocking(move || {
            let client = control_api::HttpControlClient::new(&url)?;
            client.create_snapshot(&request)
        })
        .await
        .map_err(|e| Status::internal(format!("control snapshot task failed: {e}")))?
        .map_err(|e| Status::unavailable(format!("control snapshot create failed: {e}")))?;
        Ok(SnapshotSpec::from_control(response.snapshot))
    }

    async fn control_delete_snapshot(&self, snapshot_id: &str) -> Result<(), Status> {
        let url = self
            .cfg
            .control_url
            .clone()
            .ok_or_else(|| Status::internal("control URL is not configured"))?;
        let snapshot_id = snapshot_id.to_string();
        tokio::task::spawn_blocking(move || {
            let client = control_api::HttpControlClient::new(&url)?;
            client.delete_snapshot(&snapshot_id).map(|_| ())
        })
        .await
        .map_err(|e| Status::internal(format!("control snapshot delete task failed: {e}")))?
        .map_err(|e| Status::unavailable(format!("control snapshot delete failed: {e}")))
    }

    fn list_snapshot_specs(&self) -> Result<Vec<SnapshotSpec>, Status> {
        let mut specs = Vec::new();
        match fs::read_dir(self.cfg.snapshots_dir()) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| io_status("read snapshot state entry", e))?;
                    if entry.path().extension().and_then(|s| s.to_str()) != Some("conf") {
                        continue;
                    }
                    let body = fs::read_to_string(entry.path())
                        .map_err(|e| io_status("read snapshot state", e))?;
                    specs.push(SnapshotSpec::from_state(&body)?);
                }
                Ok(specs)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(specs),
            Err(e) => Err(io_status("read snapshot state dir", e)),
        }
    }

    fn spec_for_node(
        &self,
        volume_id: &str,
        volume_context: &BTreeMap<String, String>,
    ) -> Result<VolumeSpec, Status> {
        if let Some(spec) = self.load_volume(volume_id)? {
            return Ok(spec);
        }
        VolumeSpec::from_context(volume_id, volume_context)
    }

    async fn spec_from_create_request(
        &self,
        req: &CreateVolumeRequest,
    ) -> Result<VolumeSpec, Status> {
        let restore_snapshot = self.restore_source_for_create_request(req)?;
        let backend = normalize_backend(
            req.parameters
                .get("backend")
                .map(String::as_str)
                .unwrap_or("zcbrd"),
        )?;
        if backend == "mux" {
            return Err(Status::unimplemented(
                "backend=mux needs a separate mux gateway/control plane; use backend=zcbrd, backend=file-loop, or backend=raw-block for this driver",
            ));
        }

        let mut requested_bytes = requested_capacity(req.capacity_range.as_ref())?;
        if let Some(snapshot) = restore_snapshot.as_ref() {
            if !snapshot.ready_to_use {
                return Err(Status::failed_precondition(format!(
                    "snapshot {} is not ready",
                    snapshot.snapshot_id
                )));
            }
            if let Some(range) = req.capacity_range.as_ref() {
                if range.limit_bytes > 0 && snapshot.size_bytes > range.limit_bytes {
                    return Err(Status::out_of_range(format!(
                        "snapshot size {} exceeds requested limit {}",
                        snapshot.size_bytes, range.limit_bytes
                    )));
                }
            }
            requested_bytes = requested_bytes.max(snapshot.size_bytes);
        }
        let hash = short_hash(&req.name, 20);
        let volume_id = format!("zcblk-csi-{hash}");

        let mut spec = match backend.as_str() {
            "zcbrd" => spec_from_zcbrd_create_request(req, volume_id, requested_bytes),
            "file-loop" => self.spec_from_file_loop_create_request(req, volume_id, requested_bytes),
            "raw-block" => self.spec_from_raw_block_create_request(req, volume_id, requested_bytes),
            _ => Err(Status::invalid_argument(format!(
                "unsupported backend: {backend}"
            ))),
        }?;
        if let Some(snapshot) = restore_snapshot {
            spec.restore_path = Some(snapshot.snapshot_path);
            spec.restore_snapshot_id = Some(snapshot.snapshot_id);
        }
        Ok(spec)
    }

    fn restore_source_for_create_request(
        &self,
        req: &CreateVolumeRequest,
    ) -> Result<Option<SnapshotSpec>, Status> {
        let Some(source) = req.volume_content_source.as_ref() else {
            return Ok(None);
        };
        match source.r#type.as_ref() {
            Some(volume_content_source::Type::Snapshot(snapshot)) => {
                if snapshot.snapshot_id.is_empty() {
                    return Err(Status::invalid_argument(
                        "snapshot content source requires snapshot_id",
                    ));
                }
                self.load_snapshot(&snapshot.snapshot_id)?.map_or_else(
                    || {
                        Err(Status::not_found(format!(
                            "snapshot {} not found",
                            snapshot.snapshot_id
                        )))
                    },
                    |snapshot| Ok(Some(snapshot)),
                )
            }
            Some(volume_content_source::Type::Volume(_)) => Err(Status::unimplemented(
                "volume cloning is not supported yet; create from snapshot instead",
            )),
            None => Err(Status::invalid_argument(
                "volume_content_source requires a source type",
            )),
        }
    }

    fn spec_from_file_loop_create_request(
        &self,
        req: &CreateVolumeRequest,
        volume_id: String,
        requested_bytes: i64,
    ) -> Result<VolumeSpec, Status> {
        let size_mib = bytes_to_mib(requested_bytes)?;
        let capacity_bytes = checked_mib_to_bytes(size_mib)?;
        if let Some(range) = req.capacity_range.as_ref() {
            if range.limit_bytes > 0 && capacity_bytes > range.limit_bytes {
                return Err(Status::out_of_range(format!(
                    "requested capacity rounds to {capacity_bytes} bytes, above limit {}",
                    range.limit_bytes
                )));
            }
        }
        let file_root = req
            .parameters
            .get("fileRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cfg.files_dir());
        if !file_root.is_absolute() {
            return Err(Status::invalid_argument(
                "fileRoot must be an absolute path",
            ));
        }
        let file_path = file_root.join(format!("{volume_id}.img"));
        Ok(VolumeSpec {
            backend: "file-loop".to_string(),
            volume_id,
            name_hex: hex_encode(req.name.as_bytes()),
            device_name: String::new(),
            capacity_bytes,
            size_mib,
            blocksize: DEFAULT_BLOCKSIZE,
            queues: 0,
            queue_depth: 0,
            descriptor_mode: "disabled".to_string(),
            file_path: Some(file_path.display().to_string()),
            raw_device: None,
            staging_path: None,
            restore_path: None,
            restore_snapshot_id: None,
        })
    }

    fn spec_from_raw_block_create_request(
        &self,
        req: &CreateVolumeRequest,
        volume_id: String,
        requested_bytes: i64,
    ) -> Result<VolumeSpec, Status> {
        for cap in &req.volume_capabilities {
            if !matches!(
                cap.access_type.as_ref(),
                Some(volume_capability::AccessType::Block(_))
            ) {
                return Err(Status::invalid_argument(
                    "backend=raw-block supports only volumeMode: Block",
                ));
            }
        }
        let raw_device = self.raw_device_from_parameters(&req.parameters)?;
        let capacity_bytes = block_device_size(&raw_device)?;
        if requested_bytes > 0 && requested_bytes as u64 > capacity_bytes {
            return Err(Status::out_of_range(format!(
                "requested capacity {} exceeds raw device capacity {}",
                requested_bytes, capacity_bytes
            )));
        }
        if let Some(range) = req.capacity_range.as_ref() {
            if range.limit_bytes > 0 && capacity_bytes > range.limit_bytes as u64 {
                return Err(Status::out_of_range(format!(
                    "raw device capacity {} exceeds requested limit {}",
                    capacity_bytes, range.limit_bytes
                )));
            }
        }
        Ok(VolumeSpec {
            backend: "raw-block".to_string(),
            volume_id,
            name_hex: hex_encode(req.name.as_bytes()),
            device_name: String::new(),
            capacity_bytes: i64::try_from(capacity_bytes)
                .map_err(|_| Status::out_of_range("raw device is too large"))?,
            size_mib: capacity_bytes / MIB,
            blocksize: 0,
            queues: 0,
            queue_depth: 0,
            descriptor_mode: "disabled".to_string(),
            file_path: None,
            raw_device: Some(raw_device.display().to_string()),
            staging_path: None,
            restore_path: None,
            restore_snapshot_id: None,
        })
    }

    fn raw_device_from_parameters(
        &self,
        parameters: &BTreeMap<String, String>,
    ) -> Result<PathBuf, Status> {
        if let Some(partuuid) = parameters
            .get("rawPartUUID")
            .or_else(|| parameters.get("rawPartUuid"))
            .or_else(|| parameters.get("partuuid"))
        {
            let partuuid = normalize_partuuid(partuuid)?;
            self.ensure_partuuid_allowlisted(&partuuid)?;
            let path = PathBuf::from("/dev/disk/by-partuuid").join(&partuuid);
            return canonical_block_device(&path);
        }
        let raw_device = parameters.get("rawDevice").ok_or_else(|| {
            Status::invalid_argument("backend=raw-block requires rawPartUUID or rawDevice")
        })?;
        let path = canonical_block_device(Path::new(raw_device))?;
        let partuuid = partuuid_for_device(&path).ok_or_else(|| {
            Status::permission_denied(format!(
                "{} has no PARTUUID allowlist identity",
                path.display()
            ))
        })?;
        self.ensure_partuuid_allowlisted(&partuuid)?;
        Ok(path)
    }

    fn ensure_partuuid_allowlisted(&self, target: &str) -> Result<(), Status> {
        let target = normalize_partuuid(target)?;
        let allowed = read_raw_allowlist(&self.cfg.raw_allowlist)?;
        if allowed.iter().any(|uuid| uuid == &target) {
            Ok(())
        } else {
            Err(Status::permission_denied(format!(
                "PARTUUID={} is not listed in {}",
                target,
                self.cfg.raw_allowlist.display()
            )))
        }
    }

    fn ensure_raw_device_unclaimed(&self, spec: &VolumeSpec) -> Result<(), Status> {
        let Some(raw_device) = spec.raw_device.as_ref() else {
            return Ok(());
        };
        for existing in self.list_volume_specs()? {
            if existing.volume_id != spec.volume_id
                && existing.raw_device.as_deref() == Some(raw_device.as_str())
            {
                return Err(Status::already_exists(format!(
                    "{} is already claimed by {}",
                    raw_device, existing.volume_id
                )));
            }
        }
        Ok(())
    }

    fn create_backing_file(&self, spec: &VolumeSpec) -> Result<(), Status> {
        let path = spec
            .file_path
            .as_ref()
            .ok_or_else(|| Status::internal("file-loop volume missing file_path"))?;
        let path = Path::new(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_status("create file-loop root", e))?;
        }
        let existed = path.exists();
        if !existed {
            if let Some(restore_path) = spec.restore_path.as_ref() {
                restore_image_to_file(
                    Path::new(restore_path),
                    path,
                    spec.capacity_bytes.try_into().map_err(|_| {
                        Status::out_of_range("file-loop volume capacity must be non-negative")
                    })?,
                )?;
                return Ok(());
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| io_status("create file-loop backing file", e))?;
        file.set_len(spec.capacity_bytes as u64)
            .map_err(|e| io_status("size file-loop backing file", e))
    }

    async fn ensure_backend_device(&self, spec: &VolumeSpec) -> Result<PathBuf, Status> {
        match spec.backend.as_str() {
            "zcbrd" => self.ensure_zcbrd_device(spec).await,
            "file-loop" => self.ensure_loop_device(spec).await,
            "raw-block" => self.ensure_raw_block_device(spec),
            other => Err(Status::invalid_argument(format!(
                "unsupported backend: {other}"
            ))),
        }
    }

    async fn unstage_backend_device(&self, spec: &VolumeSpec) -> Result<(), Status> {
        match spec.backend.as_str() {
            "zcbrd" => self.destroy_zcbrd_device(spec).map_err(|e| {
                Status::failed_precondition(format!("could not remove zcbrd device: {e}"))
            }),
            "file-loop" => detach_loop_for_spec(spec).await,
            "raw-block" => Ok(()),
            other => Err(Status::invalid_argument(format!(
                "unsupported backend: {other}"
            ))),
        }
    }

    async fn delete_backend_storage(&self, spec: &VolumeSpec) -> Result<(), Status> {
        match spec.backend.as_str() {
            "zcbrd" => self.destroy_zcbrd_device(spec).map_err(|e| {
                Status::failed_precondition(format!("could not remove zcbrd device: {e}"))
            }),
            "file-loop" => {
                detach_loop_for_spec(spec).await?;
                if let Some(path) = spec.file_path.as_ref() {
                    match fs::remove_file(path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(io_status("remove file-loop backing file", e)),
                    }
                }
                Ok(())
            }
            "raw-block" => Ok(()),
            other => Err(Status::invalid_argument(format!(
                "unsupported backend: {other}"
            ))),
        }
    }

    async fn create_backend_storage(&self, spec: &VolumeSpec) -> Result<(), Status> {
        match spec.backend.as_str() {
            "zcbrd" => {
                let _ = self.ensure_zcbrd_device(spec).await?;
                Ok(())
            }
            "file-loop" => self.create_backing_file(spec),
            "raw-block" => {
                let dev = self.ensure_raw_block_device(spec)?;
                if let Some(restore_path) = spec.restore_path.as_ref() {
                    restore_image_to_device(
                        Path::new(restore_path),
                        &dev,
                        spec.capacity_bytes.try_into().map_err(|_| {
                            Status::out_of_range("raw-block volume capacity must be non-negative")
                        })?,
                    )?;
                }
                Ok(())
            }
            other => Err(Status::invalid_argument(format!(
                "unsupported backend: {other}"
            ))),
        }
    }

    async fn start_replication_receive(
        &self,
        volume_id: String,
        listen: String,
        port: u16,
        token: Option<String>,
        bytes: Option<u64>,
    ) -> Result<String, Status> {
        let token = match token {
            Some(token) => token,
            None => zc_stream_generate_token()
                .map_err(|e| io_status("generate replication token", e))?,
        };
        let listener = zc_stream_bind_listener(&listen, (port, port))
            .map_err(|e| io_status("bind replication receiver", e))?;
        let bound_port = listener
            .local_addr()
            .map_err(|e| io_status("read replication listener address", e))?
            .port();
        let (writer, limit, target) = self.open_replication_target(&volume_id, bytes).await?;
        let repl_id = new_repl_id("recv", &volume_id);
        self.repl.insert(ReplicationJob {
            repl_id: repl_id.clone(),
            role: "receive".to_string(),
            state: "listening".to_string(),
            subject: volume_id.clone(),
            peer: listen.clone(),
            port: Some(bound_port),
            bytes: 0,
            error: None,
            started_at_secs: unix_now_secs(),
            finished_at_secs: None,
        });

        let jobs = self.repl.clone();
        let thread_repl_id = repl_id.clone();
        let thread_token = token.clone();
        std::thread::spawn(move || {
            jobs.mark_state(&thread_repl_id, "running");
            let writer = BoundedWriter::new(writer, limit);
            match zc_stream_receive_listener_to_writer(
                listener,
                writer,
                Some(&thread_token),
                ZcStreamEncryption::Aes256,
                false,
                DEFAULT_REPLICATION_BUFFER_BYTES,
            ) {
                Ok((_peer, bytes)) => jobs.mark_done(&thread_repl_id, bytes),
                Err(e) => jobs.mark_error(&thread_repl_id, e.to_string()),
            }
        });

        Ok(format!(
            "OK repl_id={repl_id} role=receive volume={volume_id} target={} listen={} port={bound_port} token={token}\n",
            control_field(&target),
            control_field(&listen)
        ))
    }

    async fn start_replication_send(
        &self,
        volume_id: Option<String>,
        snapshot_id: Option<String>,
        peer: String,
        port: u16,
        token: String,
        bytes: Option<u64>,
    ) -> Result<String, Status> {
        let (reader, limit, subject) = self
            .open_replication_source(volume_id.as_deref(), snapshot_id.as_deref(), bytes)
            .await?;
        let repl_id = new_repl_id("send", &subject);
        self.repl.insert(ReplicationJob {
            repl_id: repl_id.clone(),
            role: "send".to_string(),
            state: "queued".to_string(),
            subject: subject.clone(),
            peer: format!("{peer}:{port}"),
            port: Some(port),
            bytes: 0,
            error: None,
            started_at_secs: unix_now_secs(),
            finished_at_secs: None,
        });

        let jobs = self.repl.clone();
        let thread_repl_id = repl_id.clone();
        let thread_peer = peer.clone();
        let thread_token = token.clone();
        std::thread::spawn(move || {
            jobs.mark_state(&thread_repl_id, "running");
            let reader = reader.take(limit);
            match zc_stream_send_reader_to_tcp(
                reader,
                &thread_peer,
                port,
                None,
                Some(&thread_token),
                ZcStreamEncryption::Aes256,
                false,
                DEFAULT_REPLICATION_BUFFER_BYTES,
            ) {
                Ok(bytes) => jobs.mark_done(&thread_repl_id, bytes),
                Err(e) => jobs.mark_error(&thread_repl_id, e.to_string()),
            }
        });

        Ok(format!(
            "OK repl_id={repl_id} role=send source={} peer={} port={port} bytes_limit={limit}\n",
            control_field(&subject),
            control_field(&peer)
        ))
    }

    async fn open_replication_source(
        &self,
        volume_id: Option<&str>,
        snapshot_id: Option<&str>,
        bytes: Option<u64>,
    ) -> Result<(Box<dyn Read + Send>, u64, String), Status> {
        match (volume_id, snapshot_id) {
            (Some(_), Some(_)) => Err(Status::invalid_argument(
                "REPL_SEND requires either volume=<id> or snapshot=<id>, not both",
            )),
            (None, None) => Err(Status::invalid_argument(
                "REPL_SEND requires volume=<id> or snapshot=<id>",
            )),
            (Some(volume_id), None) => {
                let spec = self
                    .load_volume(volume_id)?
                    .ok_or_else(|| Status::not_found(format!("volume {volume_id} not found")))?;
                let path = self.replication_volume_path(&spec).await?;
                let capacity = volume_capacity_u64(&spec)?;
                let limit = replication_limit(bytes, capacity, "source volume")?;
                let file = OpenOptions::new()
                    .read(true)
                    .open(&path)
                    .map_err(|e| io_status("open replication source volume", e))?;
                Ok((Box::new(file), limit, format!("volume:{volume_id}")))
            }
            (None, Some(snapshot_id)) => {
                let spec = self.load_snapshot(snapshot_id)?.ok_or_else(|| {
                    Status::not_found(format!("snapshot {snapshot_id} not found"))
                })?;
                if !spec.ready_to_use {
                    return Err(Status::failed_precondition(format!(
                        "snapshot {snapshot_id} is not ready"
                    )));
                }
                let capacity = spec
                    .size_bytes
                    .try_into()
                    .map_err(|_| Status::out_of_range("snapshot size must be non-negative"))?;
                let limit = replication_limit(bytes, capacity, "source snapshot")?;
                let file = OpenOptions::new()
                    .read(true)
                    .open(&spec.snapshot_path)
                    .map_err(|e| io_status("open replication source snapshot", e))?;
                Ok((Box::new(file), limit, format!("snapshot:{snapshot_id}")))
            }
        }
    }

    async fn open_replication_target(
        &self,
        volume_id: &str,
        bytes: Option<u64>,
    ) -> Result<(Box<dyn Write + Send>, u64, String), Status> {
        let spec = self
            .load_volume(volume_id)?
            .ok_or_else(|| Status::not_found(format!("volume {volume_id} not found")))?;
        let path = self.replication_volume_path(&spec).await?;
        let capacity = volume_capacity_u64(&spec)?;
        let limit = replication_limit(bytes, capacity, "target volume")?;
        let file = OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(|e| io_status("open replication target volume", e))?;
        Ok((Box::new(file), limit, path.display().to_string()))
    }

    async fn replication_volume_path(&self, spec: &VolumeSpec) -> Result<PathBuf, Status> {
        match spec.backend.as_str() {
            "zcbrd" => self.ensure_zcbrd_device(spec).await,
            "file-loop" => {
                self.create_backing_file(spec)?;
                Ok(PathBuf::from(spec.file_path.as_ref().ok_or_else(|| {
                    Status::internal("file-loop volume missing file_path")
                })?))
            }
            "raw-block" => self.ensure_raw_block_device(spec),
            other => Err(Status::invalid_argument(format!(
                "unsupported backend: {other}"
            ))),
        }
    }

    async fn snapshot_volume(
        &self,
        source: &VolumeSpec,
        snapshot_id: &str,
        name: &str,
    ) -> Result<SnapshotSpec, Status> {
        fs::create_dir_all(self.cfg.snapshot_images_dir())
            .map_err(|e| io_status("create snapshot image dir", e))?;
        let snapshot_path = self
            .cfg
            .snapshot_images_dir()
            .join(format!("{snapshot_id}.img"));
        let size_bytes = source
            .capacity_bytes
            .try_into()
            .map_err(|_| Status::out_of_range("source volume capacity must be non-negative"))?;

        let mut snapshot_mode = "existing".to_string();
        if !snapshot_path.exists() {
            let source_path =
                match source.backend.as_str() {
                    "zcbrd" => self.ensure_zcbrd_device(source).await?,
                    "file-loop" => {
                        self.create_backing_file(source)?;
                        PathBuf::from(source.file_path.as_ref().ok_or_else(|| {
                            Status::internal("file-loop volume missing file_path")
                        })?)
                    }
                    "raw-block" => self.ensure_raw_block_device(source)?,
                    other => {
                        return Err(Status::invalid_argument(format!(
                            "unsupported backend: {other}"
                        )));
                    }
                };
            let tmp_path = snapshot_path.with_extension("img.tmp");
            match fs::remove_file(&tmp_path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_status("remove stale snapshot temp image", e)),
            }
            snapshot_mode =
                self.create_snapshot_image(source, &source_path, &tmp_path, size_bytes)?;
            fs::rename(&tmp_path, &snapshot_path)
                .map_err(|e| io_status("commit snapshot image", e))?;
        }

        Ok(SnapshotSpec {
            snapshot_id: snapshot_id.to_string(),
            name_hex: hex_encode(name.as_bytes()),
            source_volume_id: source.volume_id.clone(),
            source_backend: source.backend.clone(),
            size_bytes: size_bytes
                .try_into()
                .map_err(|_| Status::out_of_range("snapshot is too large"))?,
            snapshot_path: snapshot_path.display().to_string(),
            snapshot_mode,
            creation_time_secs: current_unix_time_secs()?,
            ready_to_use: true,
        })
    }

    fn create_snapshot_image(
        &self,
        source: &VolumeSpec,
        source_path: &Path,
        tmp_path: &Path,
        size_bytes: u64,
    ) -> Result<String, Status> {
        if self.cfg.snapshot_mode == "reflink" && source.backend != "file-loop" {
            return Err(Status::failed_precondition(format!(
                "snapshot-mode=reflink requires backend=file-loop, got {}",
                source.backend
            )));
        }

        if source.backend == "file-loop" && self.cfg.snapshot_mode != "copy" {
            match zc_pit_reflink_file(source_path, tmp_path, Some(size_bytes)) {
                Ok(()) => return Ok("reflink".to_string()),
                Err(e) if self.cfg.snapshot_mode == "auto" && zc_pit_is_reflink_unsupported(&e) => {
                    eprintln!(
                        "reflink PIT snapshot unsupported for {}; falling back to full copy: {e}",
                        source_path.display()
                    );
                }
                Err(e) => {
                    return Err(Status::failed_precondition(format!(
                        "zero-copy reflink PIT snapshot failed for {}: {e}",
                        source_path.display()
                    )));
                }
            }
        }

        copy_exact_bytes_to_file(source_path, tmp_path, size_bytes)?;
        Ok("copy".to_string())
    }

    async fn ensure_loop_device(&self, spec: &VolumeSpec) -> Result<PathBuf, Status> {
        self.create_backing_file(spec)?;
        let path = spec
            .file_path
            .as_ref()
            .ok_or_else(|| Status::internal("file-loop volume missing file_path"))?;
        let path = Path::new(path);
        if let Some(existing) = loop_device_for_file(path).await? {
            return Ok(existing);
        }
        let output = Command::new("losetup")
            .args(["--find", "--show", "--nooverlap"])
            .arg(path)
            .output()
            .await
            .map_err(|e| io_status("run losetup", e))?;
        if !output.status.success() {
            return Err(command_status("losetup attach", &output));
        }
        let dev = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if dev.is_empty() {
            return Err(Status::internal("losetup did not report a loop device"));
        }
        Ok(PathBuf::from(dev))
    }

    fn ensure_raw_block_device(&self, spec: &VolumeSpec) -> Result<PathBuf, Status> {
        let raw_device = spec
            .raw_device
            .as_ref()
            .ok_or_else(|| Status::internal("raw-block volume missing raw_device"))?;
        let path = canonical_block_device(Path::new(raw_device))?;
        let partuuid = partuuid_for_device(&path).ok_or_else(|| {
            Status::permission_denied(format!(
                "{} has no PARTUUID allowlist identity",
                path.display()
            ))
        })?;
        self.ensure_partuuid_allowlisted(&partuuid)?;
        Ok(path)
    }

    async fn ensure_zcbrd_device(&self, spec: &VolumeSpec) -> Result<PathBuf, Status> {
        if !self.cfg.configfs_root.is_dir() {
            return Err(Status::failed_precondition(format!(
                "{} is not available; load zcbrd_mod and mount configfs",
                self.cfg.configfs_root.display()
            )));
        }

        let dir = self.cfg.configfs_root.join(&spec.device_name);
        if !dir.exists() {
            fs::create_dir(&dir).map_err(|e| io_status("create zcbrd configfs device", e))?;
        }

        let powered = fs::read_to_string(dir.join("power")).unwrap_or_default();
        let needs_power = powered.trim() != "1";
        if needs_power {
            write_configfs_attr(&dir, "size_mib", spec.size_mib)?;
            write_configfs_attr(&dir, "blocksize", spec.blocksize)?;
            write_configfs_attr(&dir, "queues", spec.queues)?;
            write_configfs_attr(&dir, "queue_depth", spec.queue_depth)?;
            write_configfs_attr(&dir, "descriptor_mode", &spec.descriptor_mode)?;
            write_configfs_attr(&dir, "power", 1)?;
        }

        let dev = self.cfg.dev_root.join(&spec.device_name);
        for _ in 0..50 {
            if dev.exists() {
                if needs_power {
                    if let Some(restore_path) = spec.restore_path.as_ref() {
                        restore_image_to_device(
                            Path::new(restore_path),
                            &dev,
                            spec.capacity_bytes.try_into().map_err(|_| {
                                Status::out_of_range("zcbrd volume capacity must be non-negative")
                            })?,
                        )?;
                    }
                }
                return Ok(dev);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(Status::deadline_exceeded(format!(
            "{} did not appear after powering {}",
            dev.display(),
            spec.device_name
        )))
    }

    fn destroy_zcbrd_device(&self, spec: &VolumeSpec) -> io::Result<()> {
        let dir = self.cfg.configfs_root.join(&spec.device_name);
        if !dir.exists() {
            return Ok(());
        }
        let _ = write_configfs_attr_io(&dir, "power", "0");
        fs::remove_dir(dir)
    }
}

impl VolumeSpec {
    fn to_context(&self) -> BTreeMap<String, String> {
        let mut context = BTreeMap::new();
        context.insert("backend".to_string(), self.backend.clone());
        match self.backend.as_str() {
            "zcbrd" => {
                context.insert("deviceName".to_string(), self.device_name.clone());
                context.insert("sizeMiB".to_string(), self.size_mib.to_string());
                context.insert("blocksize".to_string(), self.blocksize.to_string());
                context.insert("queues".to_string(), self.queues.to_string());
                context.insert("queueDepth".to_string(), self.queue_depth.to_string());
                context.insert("descriptorMode".to_string(), self.descriptor_mode.clone());
            }
            "file-loop" => {
                if let Some(path) = self.file_path.as_ref() {
                    context.insert("filePath".to_string(), path.clone());
                }
                context.insert("sizeMiB".to_string(), self.size_mib.to_string());
            }
            "raw-block" => {
                if let Some(path) = self.raw_device.as_ref() {
                    context.insert("rawDevice".to_string(), path.clone());
                }
            }
            _ => {}
        }
        if let Some(path) = self.restore_path.as_ref() {
            context.insert("restorePath".to_string(), path.clone());
        }
        if let Some(snapshot_id) = self.restore_snapshot_id.as_ref() {
            context.insert("restoreSnapshotId".to_string(), snapshot_id.clone());
        }
        context
    }

    fn to_state(&self) -> String {
        format!(
            "backend={}\nvolume_id={}\nname_hex={}\ndevice_name={}\ncapacity_bytes={}\nsize_mib={}\nblocksize={}\nqueues={}\nqueue_depth={}\ndescriptor_mode={}\nfile_path={}\nraw_device={}\nstaging_path={}\nrestore_path={}\nrestore_snapshot_id={}\n",
            self.backend,
            self.volume_id,
            self.name_hex,
            self.device_name,
            self.capacity_bytes,
            self.size_mib,
            self.blocksize,
            self.queues,
            self.queue_depth,
            self.descriptor_mode,
            self.file_path.clone().unwrap_or_default(),
            self.raw_device.clone().unwrap_or_default(),
            self.staging_path.clone().unwrap_or_default(),
            self.restore_path.clone().unwrap_or_default(),
            self.restore_snapshot_id.clone().unwrap_or_default()
        )
    }

    fn from_state(body: &str) -> Result<Self, Status> {
        let map = parse_key_values(body);
        let backend = map
            .get("backend")
            .cloned()
            .unwrap_or_else(|| "zcbrd".to_string());
        Ok(Self {
            backend,
            volume_id: required(&map, "volume_id")?.to_string(),
            name_hex: required(&map, "name_hex")?.to_string(),
            device_name: required(&map, "device_name")?.to_string(),
            capacity_bytes: parse_i64(required(&map, "capacity_bytes")?, "capacity_bytes")?,
            size_mib: parse_u64(required(&map, "size_mib")?, "size_mib")?,
            blocksize: parse_u64(required(&map, "blocksize")?, "blocksize")?,
            queues: parse_u64(required(&map, "queues")?, "queues")?,
            queue_depth: parse_u64(required(&map, "queue_depth")?, "queue_depth")?,
            descriptor_mode: required(&map, "descriptor_mode")?.to_string(),
            file_path: nonempty_value(&map, "file_path"),
            raw_device: nonempty_value(&map, "raw_device"),
            staging_path: nonempty_value(&map, "staging_path"),
            restore_path: nonempty_value(&map, "restore_path"),
            restore_snapshot_id: nonempty_value(&map, "restore_snapshot_id"),
        })
    }

    fn from_context(volume_id: &str, context: &BTreeMap<String, String>) -> Result<Self, Status> {
        let backend = context
            .get("backend")
            .map(String::as_str)
            .unwrap_or("zcbrd");
        let backend = normalize_backend(backend)?;
        match backend.as_str() {
            "zcbrd" => {
                let device_name = context
                    .get("deviceName")
                    .cloned()
                    .unwrap_or_else(|| volume_id.to_string());
                let size_mib = context
                    .get("sizeMiB")
                    .map(|s| parse_u64(s, "sizeMiB"))
                    .transpose()?
                    .unwrap_or(DEFAULT_SIZE_MIB);
                let blocksize = context
                    .get("blocksize")
                    .map(|s| parse_u64(s, "blocksize"))
                    .transpose()?
                    .unwrap_or(DEFAULT_BLOCKSIZE);
                let queues = context
                    .get("queues")
                    .map(|s| parse_u64(s, "queues"))
                    .transpose()?
                    .unwrap_or(DEFAULT_QUEUES);
                let queue_depth = context
                    .get("queueDepth")
                    .or_else(|| context.get("queue_depth"))
                    .map(|s| parse_u64(s, "queueDepth"))
                    .transpose()?
                    .unwrap_or(DEFAULT_QUEUE_DEPTH);
                let descriptor_mode = normalize_descriptor_mode(
                    context
                        .get("descriptorMode")
                        .or_else(|| context.get("descriptor_mode"))
                        .map(String::as_str)
                        .unwrap_or(DEFAULT_DESCRIPTOR_MODE),
                )?;
                validate_device_options(size_mib, blocksize, queues, queue_depth)?;
                Ok(Self {
                    backend,
                    volume_id: volume_id.to_string(),
                    name_hex: String::new(),
                    device_name,
                    capacity_bytes: checked_mib_to_bytes(size_mib)?,
                    size_mib,
                    blocksize,
                    queues,
                    queue_depth,
                    descriptor_mode,
                    file_path: None,
                    raw_device: None,
                    staging_path: nonempty_value(context, "stagingPath")
                        .or_else(|| nonempty_value(context, "staging_path")),
                    restore_path: nonempty_value(context, "restorePath")
                        .or_else(|| nonempty_value(context, "restore_path")),
                    restore_snapshot_id: nonempty_value(context, "restoreSnapshotId")
                        .or_else(|| nonempty_value(context, "restore_snapshot_id")),
                })
            }
            "file-loop" => {
                let file_path = nonempty_value(context, "filePath")
                    .or_else(|| nonempty_value(context, "file_path"))
                    .ok_or_else(|| {
                        Status::invalid_argument("file-loop context missing filePath")
                    })?;
                let size_mib = context
                    .get("sizeMiB")
                    .map(|s| parse_u64(s, "sizeMiB"))
                    .transpose()?
                    .unwrap_or(DEFAULT_SIZE_MIB);
                Ok(Self {
                    backend,
                    volume_id: volume_id.to_string(),
                    name_hex: String::new(),
                    device_name: String::new(),
                    capacity_bytes: checked_mib_to_bytes(size_mib)?,
                    size_mib,
                    blocksize: DEFAULT_BLOCKSIZE,
                    queues: 0,
                    queue_depth: 0,
                    descriptor_mode: "disabled".to_string(),
                    file_path: Some(file_path),
                    raw_device: None,
                    staging_path: nonempty_value(context, "stagingPath")
                        .or_else(|| nonempty_value(context, "staging_path")),
                    restore_path: nonempty_value(context, "restorePath")
                        .or_else(|| nonempty_value(context, "restore_path")),
                    restore_snapshot_id: nonempty_value(context, "restoreSnapshotId")
                        .or_else(|| nonempty_value(context, "restore_snapshot_id")),
                })
            }
            "raw-block" => {
                let raw_device = nonempty_value(context, "rawDevice")
                    .or_else(|| nonempty_value(context, "raw_device"))
                    .ok_or_else(|| {
                        Status::invalid_argument("raw-block context missing rawDevice")
                    })?;
                Ok(Self {
                    backend,
                    volume_id: volume_id.to_string(),
                    name_hex: String::new(),
                    device_name: String::new(),
                    capacity_bytes: 0,
                    size_mib: 0,
                    blocksize: 0,
                    queues: 0,
                    queue_depth: 0,
                    descriptor_mode: "disabled".to_string(),
                    file_path: None,
                    raw_device: Some(raw_device),
                    staging_path: None,
                    restore_path: nonempty_value(context, "restorePath")
                        .or_else(|| nonempty_value(context, "restore_path")),
                    restore_snapshot_id: nonempty_value(context, "restoreSnapshotId")
                        .or_else(|| nonempty_value(context, "restore_snapshot_id")),
                })
            }
            other => Err(Status::invalid_argument(format!(
                "unsupported backend: {other}"
            ))),
        }
    }
}

impl SnapshotSpec {
    fn from_control(spec: control_api::SnapshotSpec) -> Self {
        Self {
            snapshot_id: spec.snapshot_id,
            name_hex: spec.name_hex,
            source_volume_id: spec.source_volume_id,
            source_backend: spec.source_backend,
            size_bytes: spec.size_bytes,
            snapshot_path: spec.snapshot_path,
            snapshot_mode: spec.snapshot_mode,
            creation_time_secs: spec.creation_time_secs,
            ready_to_use: spec.ready_to_use,
        }
    }

    fn to_csi_snapshot(&self) -> Snapshot {
        Snapshot {
            size_bytes: self.size_bytes,
            snapshot_id: self.snapshot_id.clone(),
            source_volume_id: self.source_volume_id.clone(),
            creation_time: Some(prost_types::Timestamp {
                seconds: self.creation_time_secs,
                nanos: 0,
            }),
            ready_to_use: self.ready_to_use,
        }
    }

    fn to_state(&self) -> String {
        format!(
            "snapshot_id={}\nname_hex={}\nsource_volume_id={}\nsource_backend={}\nsize_bytes={}\nsnapshot_path={}\nsnapshot_mode={}\ncreation_time_secs={}\nready_to_use={}\n",
            self.snapshot_id,
            self.name_hex,
            self.source_volume_id,
            self.source_backend,
            self.size_bytes,
            self.snapshot_path,
            self.snapshot_mode,
            self.creation_time_secs,
            self.ready_to_use
        )
    }

    fn from_state(body: &str) -> Result<Self, Status> {
        let map = parse_key_values(body);
        Ok(Self {
            snapshot_id: required(&map, "snapshot_id")?.to_string(),
            name_hex: required(&map, "name_hex")?.to_string(),
            source_volume_id: required(&map, "source_volume_id")?.to_string(),
            source_backend: required(&map, "source_backend")?.to_string(),
            size_bytes: parse_i64(required(&map, "size_bytes")?, "size_bytes")?,
            snapshot_path: required(&map, "snapshot_path")?.to_string(),
            snapshot_mode: nonempty_value(&map, "snapshot_mode")
                .unwrap_or_else(|| "copy".to_string()),
            creation_time_secs: parse_i64(
                required(&map, "creation_time_secs")?,
                "creation_time_secs",
            )?,
            ready_to_use: parse_bool(required(&map, "ready_to_use")?, "ready_to_use")?,
        })
    }
}

fn spec_from_zcbrd_create_request(
    req: &CreateVolumeRequest,
    volume_id: String,
    requested_bytes: i64,
) -> Result<VolumeSpec, Status> {
    let size_mib = bytes_to_mib(requested_bytes)?;
    let capacity_bytes = checked_mib_to_bytes(size_mib)?;
    if let Some(range) = req.capacity_range.as_ref() {
        if range.limit_bytes > 0 && capacity_bytes > range.limit_bytes {
            return Err(Status::out_of_range(format!(
                "requested capacity rounds to {capacity_bytes} bytes, above limit {}",
                range.limit_bytes
            )));
        }
    }

    let blocksize = parameter_u64(
        &req.parameters,
        &["blocksize", "blockSize"],
        DEFAULT_BLOCKSIZE,
    )?;
    let queues = parameter_u64(&req.parameters, &["queues"], DEFAULT_QUEUES)?;
    let queue_depth = parameter_u64(
        &req.parameters,
        &["queueDepth", "queue_depth"],
        DEFAULT_QUEUE_DEPTH,
    )?;
    let descriptor_mode = normalize_descriptor_mode(
        req.parameters
            .get("descriptorMode")
            .or_else(|| req.parameters.get("descriptor_mode"))
            .map(String::as_str)
            .unwrap_or(DEFAULT_DESCRIPTOR_MODE),
    )?;
    validate_device_options(size_mib, blocksize, queues, queue_depth)?;

    Ok(VolumeSpec {
        backend: "zcbrd".to_string(),
        volume_id: volume_id.clone(),
        name_hex: hex_encode(req.name.as_bytes()),
        device_name: volume_id,
        capacity_bytes,
        size_mib,
        blocksize,
        queues,
        queue_depth,
        descriptor_mode,
        file_path: None,
        raw_device: None,
        staging_path: None,
        restore_path: None,
        restore_snapshot_id: None,
    })
}

fn requested_capacity(range: Option<&CapacityRange>) -> Result<i64, Status> {
    let Some(range) = range else {
        return Ok((DEFAULT_SIZE_MIB * MIB) as i64);
    };
    if range.required_bytes < 0 || range.limit_bytes < 0 {
        return Err(Status::out_of_range("capacity bytes must not be negative"));
    }
    if range.required_bytes > 0 && range.limit_bytes > 0 && range.required_bytes > range.limit_bytes
    {
        return Err(Status::out_of_range(
            "capacity required_bytes must not exceed limit_bytes",
        ));
    }
    if range.required_bytes > 0 {
        Ok(range.required_bytes)
    } else if range.limit_bytes > 0 {
        Ok(range.limit_bytes.min((DEFAULT_SIZE_MIB * MIB) as i64))
    } else {
        Ok((DEFAULT_SIZE_MIB * MIB) as i64)
    }
}

fn validate_volume_capabilities(caps: &[VolumeCapability]) -> Result<(), Status> {
    if caps.is_empty() {
        return Err(Status::invalid_argument("volume_capabilities is required"));
    }
    for cap in caps {
        access_kind(Some(cap))?;
        let mode = cap.access_mode.as_ref().map(|m| m.mode).unwrap_or(0);
        let mode = volume_capability::access_mode::Mode::from_i32(mode)
            .unwrap_or(volume_capability::access_mode::Mode::Unknown);
        match mode {
            volume_capability::access_mode::Mode::SingleNodeWriter
            | volume_capability::access_mode::Mode::SingleNodeReaderOnly => {}
            _ => {
                return Err(Status::invalid_argument(format!(
                    "unsupported access mode: {mode:?}; zcblock-csi supports single-node access"
                )));
            }
        }
    }
    Ok(())
}

fn access_kind(cap: Option<&VolumeCapability>) -> Result<AccessKind, Status> {
    let cap = cap.ok_or_else(|| Status::invalid_argument("volume_capability is required"))?;
    match cap.access_type.as_ref() {
        Some(volume_capability::AccessType::Block(_)) => Ok(AccessKind::Block),
        Some(volume_capability::AccessType::Mount(mount)) => {
            let fs_type = if mount.fs_type.is_empty() {
                "ext4".to_string()
            } else {
                mount.fs_type.clone()
            };
            if fs_type != "ext4" && fs_type != "xfs" {
                return Err(Status::invalid_argument(format!(
                    "unsupported fs_type {fs_type}; supported values are ext4 and xfs"
                )));
            }
            Ok(AccessKind::Mount {
                fs_type,
                flags: mount.mount_flags.clone(),
            })
        }
        None => Err(Status::invalid_argument("volume access type is required")),
    }
}

async fn ensure_filesystem(device: &Path, requested_fs: &str) -> Result<String, Status> {
    if let Some(existing) = detect_filesystem(device).await? {
        return Ok(existing);
    }
    let tool = format!("mkfs.{requested_fs}");
    let device_arg = device_string(device);
    let args = if requested_fs == "xfs" {
        vec!["-f".to_string(), device_arg]
    } else {
        vec!["-F".to_string(), device_arg]
    };
    run_command(&tool, &args).await?;
    Ok(requested_fs.to_string())
}

async fn detect_filesystem(device: &Path) -> Result<Option<String>, Status> {
    let output = Command::new("blkid")
        .args(["-o", "value", "-s", "TYPE"])
        .arg(device)
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| io_status("run blkid", e))?;
    if output.status.success() {
        let fs_type = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if fs_type.is_empty() {
            Ok(None)
        } else {
            Ok(Some(fs_type))
        }
    } else {
        Ok(None)
    }
}

async fn mount_device(
    device: &Path,
    target: &Path,
    fs_type: &str,
    flags: &[String],
) -> Result<(), Status> {
    let mut args = vec!["-t".to_string(), fs_type.to_string()];
    if !flags.is_empty() {
        args.push("-o".to_string());
        args.push(flags.join(","));
    }
    args.push(device_string(device));
    args.push(device_string(target));
    run_command("mount", &args).await
}

async fn bind_mount(source: &Path, target: &Path, readonly: bool) -> Result<(), Status> {
    run_command(
        "mount",
        &[
            "--bind".to_string(),
            device_string(source),
            device_string(target),
        ],
    )
    .await?;
    if readonly {
        run_command(
            "mount",
            &[
                "-o".to_string(),
                "remount,bind,ro".to_string(),
                device_string(target),
            ],
        )
        .await?;
    }
    Ok(())
}

async fn umount_path(target: &Path) -> Result<(), Status> {
    run_command("umount", &[device_string(target)]).await
}

async fn is_mountpoint<P: AsRef<Path>>(target: P) -> bool {
    Command::new("findmnt")
        .args(["-rn", "--mountpoint"])
        .arg(target.as_ref())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

async fn fsfreeze_path(
    path: &Path,
    freeze: bool,
    timeout_duration: Duration,
) -> Result<(), String> {
    let mode = if freeze { "--freeze" } else { "--unfreeze" };
    let output = tokio::time::timeout(
        timeout_duration,
        Command::new("fsfreeze")
            .arg(mode)
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| {
        format!(
            "fsfreeze {mode} timed out after {}ms",
            timeout_duration.as_millis()
        )
    })?
    .map_err(|e| format!("run fsfreeze {mode}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Err(format!(
            "fsfreeze {mode} exited {}: {}{}",
            output.status,
            stderr.trim(),
            if stdout.trim().is_empty() {
                String::new()
            } else {
                format!(" stdout: {}", stdout.trim())
            }
        ))
    }
}

async fn run_command(program: &str, args: &[String]) -> Result<(), Status> {
    let output = Command::new(program)
        .args(args)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .await
        .map_err(|e| io_status(&format!("run {program}"), e))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Err(Status::internal(format!(
            "{program} failed with status {}: {}{}",
            output.status,
            stderr.trim(),
            if stdout.trim().is_empty() {
                String::new()
            } else {
                format!(" stdout: {}", stdout.trim())
            }
        )))
    }
}

fn command_status(action: &str, output: &std::process::Output) -> Status {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    Status::internal(format!(
        "{action} failed with status {}: {}{}",
        output.status,
        stderr.trim(),
        if stdout.trim().is_empty() {
            String::new()
        } else {
            format!(" stdout: {}", stdout.trim())
        }
    ))
}

async fn loop_device_for_file(path: &Path) -> Result<Option<PathBuf>, Status> {
    if !path.exists() {
        return Ok(None);
    }
    let output = Command::new("losetup")
        .args(["--associated", "--output", "NAME", "--noheadings"])
        .arg(path)
        .output()
        .await
        .map_err(|e| io_status("run losetup", e))?;
    if !output.status.success() {
        return Err(command_status("losetup query", &output));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from))
}

async fn loop_devices_matching_backing_path(path: &Path) -> Result<Vec<PathBuf>, Status> {
    let output = Command::new("losetup")
        .arg("--all")
        .output()
        .await
        .map_err(|e| io_status("run losetup", e))?;
    if !output.status.success() {
        return Err(command_status("losetup list", &output));
    }
    let needle = format!("({}", path.display());
    let filename_needle = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("/{name}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut devices = Vec::new();
    for line in stdout.lines() {
        let filename_matches = filename_needle
            .as_ref()
            .map(|needle| line.contains(needle))
            .unwrap_or(false);
        if line.contains(&needle) || filename_matches {
            if let Some((device, _rest)) = line.split_once(':') {
                devices.push(PathBuf::from(device));
            }
        }
    }
    Ok(devices)
}

async fn detach_loop_for_spec(spec: &VolumeSpec) -> Result<(), Status> {
    let Some(path) = spec.file_path.as_ref() else {
        return Ok(());
    };
    let path = Path::new(path);
    loop {
        let mut detached = false;
        while let Some(loop_device) = loop_device_for_file(path).await? {
            detach_loop_device(&loop_device).await?;
            detached = true;
        }
        for loop_device in loop_devices_matching_backing_path(path).await? {
            detach_loop_device(&loop_device).await?;
            detached = true;
        }
        if !detached {
            return Ok(());
        }
    }
}

async fn detach_loop_device(loop_device: &Path) -> Result<(), Status> {
    let output = Command::new("losetup")
        .arg("--detach")
        .arg(loop_device)
        .output()
        .await
        .map_err(|e| io_status("run losetup detach", e))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_status("losetup detach", &output))
    }
}

fn canonical_block_device(path: &Path) -> Result<PathBuf, Status> {
    let canonical = fs::canonicalize(path).map_err(|e| io_status("resolve block device", e))?;
    let meta = fs::metadata(&canonical).map_err(|e| io_status("stat block device", e))?;
    if !meta.file_type().is_block_device() {
        return Err(Status::invalid_argument(format!(
            "{} is not a block device",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn block_device_size(path: &Path) -> Result<u64, Status> {
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|e| io_status("open block device", e))?;
    let mut bytes: u64 = 0;
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), BLKGETSIZE64, &mut bytes) };
    if ret != 0 {
        return Err(io_status(
            "get block device size",
            io::Error::last_os_error(),
        ));
    }
    Ok(bytes)
}

fn copy_exact_bytes_to_file(source: &Path, dest: &Path, bytes: u64) -> Result<(), Status> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| io_status("create image parent", e))?;
    }
    let mut src = OpenOptions::new()
        .read(true)
        .open(source)
        .map_err(|e| io_status("open copy source", e))?;
    let mut dst = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(dest)
        .map_err(|e| io_status("open copy destination", e))?;
    copy_exact_bytes(&mut src, &mut dst, bytes).map_err(|e| io_status("copy bytes", e))?;
    dst.set_len(bytes)
        .map_err(|e| io_status("size copied image", e))?;
    dst.sync_all()
        .map_err(|e| io_status("sync copied image", e))
}

fn restore_image_to_file(image: &Path, dest: &Path, capacity_bytes: u64) -> Result<(), Status> {
    let image_bytes = file_size(image)?;
    if image_bytes > capacity_bytes {
        return Err(Status::out_of_range(format!(
            "snapshot image size {image_bytes} exceeds destination capacity {capacity_bytes}"
        )));
    }
    copy_exact_bytes_to_file(image, dest, image_bytes)?;
    let file = OpenOptions::new()
        .write(true)
        .open(dest)
        .map_err(|e| io_status("open restored file image", e))?;
    file.set_len(capacity_bytes)
        .map_err(|e| io_status("size restored file image", e))?;
    file.sync_all()
        .map_err(|e| io_status("sync restored file image", e))
}

fn restore_image_to_device(image: &Path, dest: &Path, capacity_bytes: u64) -> Result<(), Status> {
    let image_bytes = file_size(image)?;
    if image_bytes > capacity_bytes {
        return Err(Status::out_of_range(format!(
            "snapshot image size {image_bytes} exceeds destination capacity {capacity_bytes}"
        )));
    }
    let mut src = OpenOptions::new()
        .read(true)
        .open(image)
        .map_err(|e| io_status("open snapshot image", e))?;
    let mut dst = OpenOptions::new()
        .write(true)
        .open(dest)
        .map_err(|e| io_status("open restore destination", e))?;
    copy_exact_bytes(&mut src, &mut dst, image_bytes)
        .map_err(|e| io_status("restore snapshot image", e))?;
    dst.sync_all()
        .map_err(|e| io_status("sync restored block device", e))
}

fn copy_exact_bytes(src: &mut File, dst: &mut File, mut remaining: u64) -> io::Result<()> {
    let mut buf = vec![0_u8; 1024 * 1024];
    while remaining > 0 {
        let len = remaining.min(buf.len() as u64) as usize;
        src.read_exact(&mut buf[..len])?;
        dst.write_all(&buf[..len])?;
        remaining -= len as u64;
    }
    Ok(())
}

fn file_size(path: &Path) -> Result<u64, Status> {
    fs::metadata(path)
        .map(|meta| meta.len())
        .map_err(|e| io_status("stat snapshot image", e))
}

fn partuuid_for_device(path: &Path) -> Option<String> {
    let canonical = fs::canonicalize(path).ok()?;
    let entries = fs::read_dir("/dev/disk/by-partuuid").ok()?;
    for entry in entries.flatten() {
        let target = fs::canonicalize(entry.path()).ok()?;
        if target == canonical {
            let uuid = entry.file_name().to_string_lossy().to_string();
            return normalize_partuuid(&uuid).ok();
        }
    }
    None
}

fn read_raw_allowlist(path: &Path) -> Result<Vec<String>, Status> {
    let body = fs::read_to_string(path).map_err(|e| {
        Status::permission_denied(format!(
            "cannot read raw block allowlist {}: {e}",
            path.display()
        ))
    })?;
    let mut allowed = Vec::new();
    for (idx, line) in body.lines().enumerate() {
        let token = line.split('#').next().unwrap_or("").trim();
        if token.is_empty() {
            continue;
        }
        allowed.push(normalize_partuuid(token).map_err(|e| {
            Status::invalid_argument(format!(
                "invalid PARTUUID in {} line {}: {e}",
                path.display(),
                idx + 1
            ))
        })?);
    }
    if allowed.is_empty() {
        return Err(Status::permission_denied(format!(
            "raw block allowlist {} is empty",
            path.display()
        )));
    }
    Ok(allowed)
}

fn current_unix_time_secs() -> Result<i64, Status> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Status::internal("system clock is before the Unix epoch"))?;
    i64::try_from(duration.as_secs()).map_err(|_| Status::out_of_range("timestamp is too large"))
}

fn stat_volume_path(path: &Path) -> Result<VolumeUsage, Status> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| Status::invalid_argument("volume_path contains NUL byte"))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if ret != 0 {
        return Err(io_status("stat volume path", io::Error::last_os_error()));
    }
    let block = stat.f_frsize as i64;
    let total = stat.f_blocks as i64 * block;
    let available = stat.f_bavail as i64 * block;
    Ok(VolumeUsage {
        available,
        total,
        used: total.saturating_sub(available),
        unit: volume_usage::Unit::Bytes as i32,
    })
}

fn write_configfs_attr<T: ToString>(dir: &Path, attr: &str, value: T) -> Result<(), Status> {
    write_configfs_attr_io(dir, attr, &value.to_string()).map_err(|e| {
        Status::failed_precondition(format!("write {} for {} failed: {e}", attr, dir.display()))
    })
}

fn write_configfs_attr_io(dir: &Path, attr: &str, value: &str) -> io::Result<()> {
    fs::write(dir.join(attr), format!("{value}\n"))
}

fn remove_path_if_exists(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn plugin_service_cap(cap: plugin_capability::service::Type) -> PluginCapability {
    PluginCapability {
        r#type: Some(plugin_capability::Type::Service(
            plugin_capability::Service { r#type: cap as i32 },
        )),
    }
}

fn controller_rpc_cap(
    cap: controller_service_capability::rpc::Type,
) -> ControllerServiceCapability {
    ControllerServiceCapability {
        r#type: Some(controller_service_capability::Type::Rpc(
            controller_service_capability::Rpc { r#type: cap as i32 },
        )),
    }
}

fn node_rpc_cap(cap: node_service_capability::rpc::Type) -> NodeServiceCapability {
    NodeServiceCapability {
        r#type: Some(node_service_capability::Type::Rpc(
            node_service_capability::Rpc { r#type: cap as i32 },
        )),
    }
}

fn parameter_u64(
    parameters: &BTreeMap<String, String>,
    names: &[&str],
    default: u64,
) -> Result<u64, Status> {
    for name in names {
        if let Some(value) = parameters.get(*name) {
            return parse_u64(value, name);
        }
    }
    Ok(default)
}

fn validate_device_options(
    size_mib: u64,
    blocksize: u64,
    queues: u64,
    queue_depth: u64,
) -> Result<(), Status> {
    if size_mib == 0 {
        return Err(Status::invalid_argument(
            "sizeMiB must be greater than zero",
        ));
    }
    if blocksize < 512 || !blocksize.is_power_of_two() {
        return Err(Status::invalid_argument(
            "blocksize must be a power of two and at least 512",
        ));
    }
    if queues == 0 {
        return Err(Status::invalid_argument("queues must be greater than zero"));
    }
    if queue_depth == 0 {
        return Err(Status::invalid_argument(
            "queueDepth must be greater than zero",
        ));
    }
    Ok(())
}

fn normalize_backend(value: &str) -> Result<String, Status> {
    match value.trim().to_ascii_lowercase().as_str() {
        "zcbrd" | "brd" => Ok("zcbrd".to_string()),
        "file-loop" | "file" | "loop" | "loop-file" => Ok("file-loop".to_string()),
        "raw-block" | "raw" | "block" => Ok("raw-block".to_string()),
        "mux" => Ok("mux".to_string()),
        other => Err(Status::invalid_argument(format!(
            "unsupported backend: {other}"
        ))),
    }
}

fn normalize_descriptor_mode(value: &str) -> Result<String, Status> {
    match value.trim().to_ascii_lowercase().as_str() {
        "advertise" | "true" | "1" | "enabled" | "on" => Ok("advertise".to_string()),
        "disabled" | "disable" | "false" | "0" | "off" => Ok("disabled".to_string()),
        other => Err(Status::invalid_argument(format!(
            "unsupported descriptorMode: {other}"
        ))),
    }
}

fn normalize_partuuid(value: &str) -> Result<String, Status> {
    let value = value.trim();
    let value = value
        .strip_prefix("PARTUUID=")
        .or_else(|| value.strip_prefix("partuuid="))
        .or_else(|| value.strip_prefix("partuuid:"))
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase();
    if value.is_empty() {
        return Err(Status::invalid_argument("PARTUUID must not be empty"));
    }
    if !value.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err(Status::invalid_argument(format!(
            "invalid PARTUUID syntax: {value}"
        )));
    }
    Ok(value)
}

fn parse_unix_endpoint(endpoint: &str) -> Result<PathBuf, String> {
    let path = endpoint
        .strip_prefix("unix://")
        .or_else(|| endpoint.strip_prefix("unix:"))
        .unwrap_or(endpoint);
    if path.is_empty() {
        return Err("CSI endpoint path must not be empty".to_string());
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(format!(
            "CSI endpoint must be an absolute Unix socket path: {endpoint}"
        ));
    }
    Ok(path)
}

fn normalize_snapshot_mode(value: &str) -> Result<String, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok("auto".to_string()),
        "copy" | "full-copy" => Ok("copy".to_string()),
        "reflink" | "cow" | "zero-copy" | "zerocopy" => Ok("reflink".to_string()),
        other => Err(format!(
            "unsupported snapshot mode {other:?}; expected auto, copy, or reflink"
        )),
    }
}

fn socket_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.file_type().is_socket()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn parse_control_command(line: &str) -> Result<ControlCommand, String> {
    let mut parts = line.trim().split_whitespace();
    let command = parts
        .next()
        .ok_or_else(|| "command is required".to_string())?
        .to_ascii_uppercase();
    let mut args = BTreeMap::new();
    for part in parts {
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| format!("invalid argument {part}; expected key=value"))?;
        args.insert(key.to_ascii_lowercase(), value.to_string());
    }
    match command.as_str() {
        "FREEZE" => {
            let barrier_id = args
                .get("barrier")
                .or_else(|| args.get("barrier_id"))
                .cloned()
                .ok_or_else(|| "FREEZE requires barrier=<id>".to_string())?;
            let ttl_ms = args
                .get("ttl_ms")
                .or_else(|| args.get("ttl"))
                .ok_or_else(|| "FREEZE requires ttl_ms=<milliseconds>".to_string())?
                .parse::<u64>()
                .map_err(|_| "ttl_ms must be an integer".to_string())?;
            Ok(ControlCommand::Freeze { barrier_id, ttl_ms })
        }
        "RELEASE" => {
            let barrier_id = args
                .get("barrier")
                .or_else(|| args.get("barrier_id"))
                .cloned()
                .ok_or_else(|| "RELEASE requires barrier=<id>".to_string())?;
            Ok(ControlCommand::Release { barrier_id })
        }
        "STATUS" => Ok(ControlCommand::Status),
        "REPL_RECV" | "REPLICATION_RECV" => {
            let volume_id = args
                .get("volume")
                .or_else(|| args.get("volume_id"))
                .cloned()
                .ok_or_else(|| "REPL_RECV requires volume=<id>".to_string())?;
            let listen = args
                .get("listen")
                .or_else(|| args.get("listen_address"))
                .cloned()
                .unwrap_or_else(|| "0.0.0.0".to_string());
            let port =
                parse_control_u16(args.get("port").map(String::as_str).unwrap_or("0"), "port")?;
            let token = args
                .get("token")
                .filter(|value| value.as_str() != "auto")
                .cloned();
            if let Some(token) = token.as_ref() {
                validate_control_token(token, "token")?;
            }
            let bytes = parse_optional_control_u64(&args, "bytes")?;
            Ok(ControlCommand::ReplRecv {
                volume_id,
                listen,
                port,
                token,
                bytes,
            })
        }
        "REPL_SEND" | "REPLICATION_SEND" => {
            let volume_id = args
                .get("volume")
                .or_else(|| args.get("volume_id"))
                .cloned();
            let snapshot_id = args
                .get("snapshot")
                .or_else(|| args.get("snapshot_id"))
                .cloned();
            if volume_id.is_some() == snapshot_id.is_some() {
                return Err(
                    "REPL_SEND requires exactly one of volume=<id> or snapshot=<id>".to_string(),
                );
            }
            let peer = args
                .get("peer")
                .or_else(|| args.get("host"))
                .cloned()
                .ok_or_else(|| "REPL_SEND requires peer=<host-or-ip>".to_string())?;
            let port = parse_control_u16(
                args.get("port")
                    .map(String::as_str)
                    .ok_or_else(|| "REPL_SEND requires port=<tcp-port>".to_string())?,
                "port",
            )?;
            if port == 0 {
                return Err("REPL_SEND port must be greater than zero".to_string());
            }
            let token = args
                .get("token")
                .cloned()
                .ok_or_else(|| "REPL_SEND requires token=<token>".to_string())?;
            validate_control_token(&token, "token")?;
            let bytes = parse_optional_control_u64(&args, "bytes")?;
            Ok(ControlCommand::ReplSend {
                volume_id,
                snapshot_id,
                peer,
                port,
                token,
                bytes,
            })
        }
        "REPL_STATUS" | "REPLICATION_STATUS" => Ok(ControlCommand::ReplStatus {
            repl_id: args.get("repl_id").or_else(|| args.get("id")).cloned(),
        }),
        other => Err(format!(
            "unknown command {other}; expected FREEZE, RELEASE, STATUS, REPL_RECV, REPL_SEND, or REPL_STATUS"
        )),
    }
}

fn validate_barrier_id(barrier_id: &str) -> Result<(), String> {
    if barrier_id.is_empty() {
        return Err("barrier id must not be empty".to_string());
    }
    if barrier_id.len() > 128 {
        return Err("barrier id must be at most 128 bytes".to_string());
    }
    if barrier_id.chars().any(|c| c.is_whitespace() || c == '\0') {
        return Err("barrier id must not contain whitespace or NUL".to_string());
    }
    Ok(())
}

fn validate_control_token(value: &str, name: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{name} must not be empty"));
    }
    if value.len() > 512 {
        return Err(format!("{name} must be at most 512 bytes"));
    }
    if value.chars().any(|c| c.is_whitespace() || c == '\0') {
        return Err(format!("{name} must not contain whitespace or NUL"));
    }
    Ok(())
}

fn parse_control_u16(value: &str, name: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|_| format!("{name} must be an integer from 0 to 65535"))
}

fn parse_optional_control_u64(
    args: &BTreeMap<String, String>,
    key: &str,
) -> Result<Option<u64>, String> {
    args.get(key)
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| format!("{key} must be an integer"))
        })
        .transpose()
}

fn volume_capacity_u64(spec: &VolumeSpec) -> Result<u64, Status> {
    spec.capacity_bytes
        .try_into()
        .map_err(|_| Status::out_of_range("volume capacity must be non-negative"))
}

fn replication_limit(requested: Option<u64>, capacity: u64, label: &str) -> Result<u64, Status> {
    let limit = requested.unwrap_or(capacity);
    if limit == 0 {
        return Err(Status::invalid_argument(
            "replication bytes must be greater than zero",
        ));
    }
    if limit > capacity {
        return Err(Status::out_of_range(format!(
            "replication bytes {limit} exceeds {label} capacity {capacity}"
        )));
    }
    Ok(limit)
}

fn new_repl_id(kind: &str, subject: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("zcrepl-{kind}-{nanos}-{}", short_hash(subject, 6))
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn control_field(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_whitespace() || ch == '\0' {
                '_'
            } else {
                ch
            }
        })
        .collect()
}

fn remaining_ms(deadline: Instant) -> u64 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn join_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "-".to_string();
    }
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn is_already_thawed_error(error: &str) -> bool {
    error.contains("Invalid argument") || error.contains("not frozen")
}

fn local_hostname() -> String {
    env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "localhost".to_string())
}

fn bytes_to_mib(bytes: i64) -> Result<u64, Status> {
    if bytes <= 0 {
        return Ok(DEFAULT_SIZE_MIB);
    }
    let bytes = bytes as u64;
    Ok(bytes.saturating_add(MIB - 1) / MIB)
}

fn checked_mib_to_bytes(size_mib: u64) -> Result<i64, Status> {
    let bytes = size_mib
        .checked_mul(MIB)
        .ok_or_else(|| Status::out_of_range("sizeMiB is too large"))?;
    i64::try_from(bytes).map_err(|_| Status::out_of_range("sizeMiB is too large"))
}

fn short_hash(input: &str, bytes: usize) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest
        .iter()
        .take(bytes / 2)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_key_values(body: &str) -> BTreeMap<String, String> {
    body.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

fn required<'a>(map: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str, Status> {
    map.get(key)
        .map(String::as_str)
        .ok_or_else(|| Status::internal(format!("volume state missing {key}")))
}

fn nonempty_value(map: &BTreeMap<String, String>, key: &str) -> Option<String> {
    map.get(key).filter(|value| !value.is_empty()).cloned()
}

fn parse_u64(value: &str, name: &str) -> Result<u64, Status> {
    value
        .parse::<u64>()
        .map_err(|_| Status::invalid_argument(format!("{name} must be an unsigned integer")))
}

fn parse_i64(value: &str, name: &str) -> Result<i64, Status> {
    value
        .parse::<i64>()
        .map_err(|_| Status::internal(format!("{name} in state is invalid")))
}

fn parse_bool(value: &str, name: &str) -> Result<bool, Status> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(Status::internal(format!("{name} in state is invalid"))),
    }
}

fn device_string(path: &Path) -> String {
    path.display().to_string()
}

fn io_status(action: &str, e: io::Error) -> Status {
    Status::internal(format!("{action}: {e}"))
}
