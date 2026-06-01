use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use zcutils::block::control as api;
use zcutils::block::logstream::{DurableLogStream, FileLogStream};
use zcutils::{
    ZcStreamEncryption, zc_pit_is_reflink_unsupported, zc_pit_reflink_file,
    zc_stream_bind_listener, zc_stream_generate_token, zc_stream_receive_listener_to_writer,
    zc_stream_send_reader_to_tcp,
};

const DEFAULT_LISTEN: &str = "127.0.0.1:9788";
const DEFAULT_STATE_DIR: &str = "/var/lib/zcblock-csi";
const DEFAULT_FREEZE_MAX_TTL_MS: u64 = 5_000;
const FREEZE_COMMAND_TIMEOUT_MS: u64 = 250;
const DEFAULT_CONFIGFS_ROOT: &str = "/sys/kernel/config/zcbrd";
const DEFAULT_DEV_ROOT: &str = "/dev";
const DEFAULT_FABRIC_DEVICE_NAME: &str = "zcnblk0";
const DEFAULT_RAW_ALLOWLIST: &str = "/etc/zcblock-csi/allowed-raw-partitions.txt";
const DEFAULT_SNAPSHOT_MODE: &str = "auto";
const DEFAULT_REPLICATION_BUFFER_BYTES: usize = 1024 * 1024;
const DEFAULT_REPLICATION_ATTENTION_IDLE_MS: u64 = 30_000;
const DEFAULT_LOGSTREAM_RELATIVE_PATH: &str = "logstream/zccusan-state.log";
const BLKGETSIZE64: libc::c_ulong = 0x80081272;

static OPENAPI_YAML: &str = include_str!("../openapi/zcblock-control.yaml");

#[derive(Clone, Debug)]
struct Config {
    listen: String,
    state_dir: PathBuf,
    freeze_max_ttl_ms: u64,
    configfs_root: PathBuf,
    dev_root: PathBuf,
    fabric_device_name: String,
    raw_allowlist: PathBuf,
    snapshot_mode: String,
    logstream_path: PathBuf,
}

#[derive(Clone)]
struct ControlApp {
    cfg: Arc<Config>,
    repl: Arc<ReplicationManager>,
    replication_modes: Arc<ReplicationModeStore>,
    replication_routes: Arc<ReplicationRouteStore>,
    freeze: Arc<FreezeManager>,
    log: Arc<FileLogStream>,
}

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

#[derive(Debug)]
struct FreezeManager {
    cfg: Arc<Config>,
    log: Arc<FileLogStream>,
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

#[derive(Debug, Default)]
struct ReplicationManager {
    jobs: Mutex<BTreeMap<String, api::ReplicationJob>>,
}

#[derive(Debug, Default)]
struct ReplicationModeStore {
    policies: Mutex<BTreeMap<String, api::ReplicationModeSpec>>,
}

#[derive(Debug, Default)]
struct ReplicationRouteStore {
    routes: Mutex<BTreeMap<String, api::ReplicationRouteSpec>>,
}

struct BoundedWriter<W> {
    inner: W,
    remaining: u64,
}

struct ProgressReader<R> {
    inner: R,
    jobs: Arc<ReplicationManager>,
    repl_id: String,
}

struct ProgressWriter<W> {
    inner: W,
    jobs: Arc<ReplicationManager>,
    repl_id: String,
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Arc::new(Config::from_args().map_err(invalid_input)?);
    fs::create_dir_all(cfg.volumes_dir())?;
    fs::create_dir_all(cfg.snapshots_dir())?;
    fs::create_dir_all(cfg.snapshot_images_dir())?;
    fs::create_dir_all(cfg.snapshot_devices_dir())?;
    fs::create_dir_all(cfg.compactions_dir())?;
    let log = Arc::new(FileLogStream::open(&cfg.logstream_path)?);

    let freeze = Arc::new(FreezeManager::new(cfg.clone(), log.clone()));
    freeze.thaw_stale_mounts_on_startup();
    let app = ControlApp {
        cfg: cfg.clone(),
        repl: Arc::new(ReplicationManager::default()),
        replication_modes: Arc::new(ReplicationModeStore::default()),
        replication_routes: Arc::new(ReplicationRouteStore::default()),
        freeze,
        log,
    };
    app.materialize_state_from_log()
        .map_err(|e| invalid_input(format!("replay logstream: {e}")))?;

    let listener = TcpListener::bind(&cfg.listen)?;
    eprintln!(
        "zcblock-control {} listening on {} state_dir={} logstream={} snapshot_mode={} fabric_device={} max_freeze_ttl_ms={}",
        env!("CARGO_PKG_VERSION"),
        cfg.listen,
        cfg.state_dir.display(),
        cfg.logstream_path.display(),
        cfg.snapshot_mode,
        cfg.fabric_device_name,
        cfg.freeze_max_ttl_ms
    );

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let app = app.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, app) {
                        eprintln!("control HTTP connection failed: {e}");
                    }
                });
            }
            Err(e) => eprintln!("control HTTP accept failed: {e}"),
        }
    }
    Ok(())
}

impl Config {
    fn from_args() -> Result<Self, String> {
        let mut listen =
            env::var("ZCBLOCK_CONTROL_LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
        let mut state_dir = PathBuf::from(
            env::var("ZCBLOCK_CSI_STATE_DIR")
                .or_else(|_| env::var("ZCBLOCK_CONTROL_STATE_DIR"))
                .unwrap_or_else(|_| DEFAULT_STATE_DIR.into()),
        );
        let mut freeze_max_ttl_ms = env::var("ZCBLOCK_CSI_FREEZE_MAX_TTL_MS")
            .or_else(|_| env::var("ZCBLOCK_CONTROL_FREEZE_MAX_TTL_MS"))
            .ok()
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| "freeze max ttl must be an integer".to_string())
            })
            .transpose()?
            .unwrap_or(DEFAULT_FREEZE_MAX_TTL_MS);
        let mut configfs_root = PathBuf::from(
            env::var("ZCBLOCK_CONFIGFS_ROOT").unwrap_or_else(|_| DEFAULT_CONFIGFS_ROOT.into()),
        );
        let mut dev_root =
            PathBuf::from(env::var("ZCBLOCK_DEV_ROOT").unwrap_or_else(|_| DEFAULT_DEV_ROOT.into()));
        let mut fabric_device_name = env::var("ZCCUSAN_FABRIC_DEVICE_NAME")
            .or_else(|_| env::var("ZCBLOCK_FABRIC_DEVICE_NAME"))
            .unwrap_or_else(|_| DEFAULT_FABRIC_DEVICE_NAME.to_string());
        let mut raw_allowlist = PathBuf::from(
            env::var("ZCBLOCK_RAW_ALLOWLIST").unwrap_or_else(|_| DEFAULT_RAW_ALLOWLIST.into()),
        );
        let mut snapshot_mode = normalize_snapshot_mode(
            &env::var("ZCBLOCK_CSI_SNAPSHOT_MODE")
                .or_else(|_| env::var("ZCBLOCK_CONTROL_SNAPSHOT_MODE"))
                .unwrap_or_else(|_| DEFAULT_SNAPSHOT_MODE.to_string()),
        )?;
        let mut logstream_path = env::var("ZCCUSAN_LOGSTREAM_PATH")
            .or_else(|_| env::var("ZCBLOCK_CONTROL_LOGSTREAM_PATH"))
            .ok()
            .map(PathBuf::from);

        let args = env::args().skip(1).collect::<Vec<_>>();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if let Some(value) = arg.strip_prefix("--listen=") {
                listen = value.to_string();
            } else if let Some(value) = arg.strip_prefix("--state-dir=") {
                state_dir = PathBuf::from(value);
            } else if let Some(value) = arg.strip_prefix("--freeze-max-ttl-ms=") {
                freeze_max_ttl_ms = value
                    .parse::<u64>()
                    .map_err(|_| "--freeze-max-ttl-ms must be an integer".to_string())?;
            } else if let Some(value) = arg.strip_prefix("--configfs-root=") {
                configfs_root = PathBuf::from(value);
            } else if let Some(value) = arg.strip_prefix("--dev-root=") {
                dev_root = PathBuf::from(value);
            } else if let Some(value) = arg.strip_prefix("--fabric-device-name=") {
                fabric_device_name = value.to_string();
            } else if let Some(value) = arg.strip_prefix("--raw-allowlist=") {
                raw_allowlist = PathBuf::from(value);
            } else if let Some(value) = arg.strip_prefix("--snapshot-mode=") {
                snapshot_mode = normalize_snapshot_mode(value)?;
            } else if let Some(value) = arg.strip_prefix("--logstream-path=") {
                logstream_path = Some(PathBuf::from(value));
            } else if arg == "--listen" {
                i += 1;
                listen = value(&args, i, "--listen")?.to_string();
            } else if arg == "--state-dir" {
                i += 1;
                state_dir = PathBuf::from(value(&args, i, "--state-dir")?);
            } else if arg == "--freeze-max-ttl-ms" {
                i += 1;
                freeze_max_ttl_ms = value(&args, i, "--freeze-max-ttl-ms")?
                    .parse::<u64>()
                    .map_err(|_| "--freeze-max-ttl-ms must be an integer".to_string())?;
            } else if arg == "--configfs-root" {
                i += 1;
                configfs_root = PathBuf::from(value(&args, i, "--configfs-root")?);
            } else if arg == "--dev-root" {
                i += 1;
                dev_root = PathBuf::from(value(&args, i, "--dev-root")?);
            } else if arg == "--fabric-device-name" {
                i += 1;
                fabric_device_name = value(&args, i, "--fabric-device-name")?.to_string();
            } else if arg == "--raw-allowlist" {
                i += 1;
                raw_allowlist = PathBuf::from(value(&args, i, "--raw-allowlist")?);
            } else if arg == "--snapshot-mode" {
                i += 1;
                snapshot_mode = normalize_snapshot_mode(value(&args, i, "--snapshot-mode")?)?;
            } else if arg == "--logstream-path" {
                i += 1;
                logstream_path = Some(PathBuf::from(value(&args, i, "--logstream-path")?));
            } else {
                return Err(format!("unknown argument: {arg}"));
            }
            i += 1;
        }

        if freeze_max_ttl_ms == 0 {
            return Err("freeze max ttl must be greater than zero".to_string());
        }
        validate_device_name(&fabric_device_name, "fabric_device_name")?;
        let logstream_path =
            logstream_path.unwrap_or_else(|| state_dir.join(DEFAULT_LOGSTREAM_RELATIVE_PATH));
        Ok(Self {
            listen,
            state_dir,
            freeze_max_ttl_ms,
            configfs_root,
            dev_root,
            fabric_device_name,
            raw_allowlist,
            snapshot_mode,
            logstream_path,
        })
    }

    fn volumes_dir(&self) -> PathBuf {
        self.state_dir.join("volumes")
    }

    fn state_path(&self, volume_id: &str) -> PathBuf {
        self.volumes_dir().join(format!("{volume_id}.conf"))
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

    fn snapshot_devices_dir(&self) -> PathBuf {
        self.state_dir.join("snapshot-devices")
    }

    fn snapshot_device_state_path(&self, device_id: &str) -> PathBuf {
        self.snapshot_devices_dir()
            .join(format!("{device_id}.conf"))
    }

    fn compactions_dir(&self) -> PathBuf {
        self.state_dir.join("compactions")
    }

    fn compaction_state_path(&self, job_id: &str) -> PathBuf {
        self.compactions_dir().join(format!("{job_id}.conf"))
    }
}

impl ControlApp {
    fn create_snapshot(
        &self,
        request: api::CreateSnapshotRequest,
    ) -> Result<api::CreateSnapshotResponse, String> {
        if request.source_volume_id.is_empty() {
            return Err("source_volume_id is required".to_string());
        }
        if request.snapshot_id.is_empty() {
            return Err("snapshot_id is required".to_string());
        }
        if request.name.is_empty() {
            return Err("name is required".to_string());
        }

        if let Some(existing) = self.load_snapshot(&request.snapshot_id)? {
            if existing.source_volume_id != request.source_volume_id {
                return Err(format!(
                    "snapshot {} already exists for a different source volume",
                    request.snapshot_id
                ));
            }
            return Ok(api::CreateSnapshotResponse { snapshot: existing });
        }

        let source = self
            .load_volume(&request.source_volume_id)?
            .ok_or_else(|| format!("volume {} not found", request.source_volume_id))?;
        let snapshot = self.snapshot_volume(&source, &request.snapshot_id, &request.name)?;
        self.append_state_log("snapshot.created", &snapshot.snapshot_id, &snapshot)?;
        self.save_snapshot(&snapshot)?;
        Ok(api::CreateSnapshotResponse { snapshot })
    }

    fn delete_snapshot(&self, snapshot_id: &str) -> Result<api::DeleteSnapshotResponse, String> {
        if snapshot_id.is_empty() {
            return Err("snapshot_id is required".to_string());
        }
        let existing = self.load_snapshot(snapshot_id)?;
        let state = self.cfg.snapshot_state_path(snapshot_id);
        let response = api::DeleteSnapshotResponse {
            deleted: existing.is_some() || state.exists(),
        };
        self.append_state_log("snapshot.deleted", snapshot_id, &response)?;

        let mut deleted = false;
        if let Some(spec) = existing {
            remove_file_if_exists(Path::new(&spec.snapshot_path))
                .map_err(|e| format!("remove snapshot image: {e}"))?;
            deleted = true;
        }
        match fs::remove_file(state) {
            Ok(()) => deleted = true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("remove snapshot state: {e}")),
        }
        Ok(api::DeleteSnapshotResponse { deleted })
    }

    fn create_snapshot_device(
        &self,
        request: api::CreateSnapshotDeviceRequest,
    ) -> Result<api::SnapshotDeviceResponse, String> {
        if request.snapshot_id.is_empty() {
            return Err("snapshot_id is required".to_string());
        }
        let mode = normalize_snapshot_device_mode(&request.mode)?;
        let device_id = match request.device_id {
            Some(device_id) if !device_id.is_empty() => device_id,
            Some(_) => return Err("device_id must not be empty when provided".to_string()),
            None => default_snapshot_device_id(&request.snapshot_id, &mode),
        };
        validate_state_id(&device_id, "device_id")?;
        let readonly = request.readonly.unwrap_or(true);
        if !readonly {
            return Err(
                "snapshot devices are read-only; create a restored volume for writes".to_string(),
            );
        }

        let snapshot = self
            .load_snapshot(&request.snapshot_id)?
            .ok_or_else(|| format!("snapshot {} not found", request.snapshot_id))?;
        if !snapshot.ready_to_use {
            return Err(format!("snapshot {} is not ready", request.snapshot_id));
        }

        if let Some(existing) = self.load_snapshot_device(&device_id)? {
            if existing.snapshot_id != snapshot.snapshot_id || existing.mode != mode {
                return Err(format!(
                    "snapshot device {device_id} already exists for snapshot {} mode {}",
                    existing.snapshot_id, existing.mode
                ));
            }
            return Ok(api::SnapshotDeviceResponse { device: existing });
        }

        let now = unix_now_millis();
        let device_name = self.cfg.fabric_device_name.clone();
        let device_path = self.cfg.dev_root.join(&device_name);
        let mut device = api::SnapshotDeviceSpec {
            device_id: device_id.clone(),
            snapshot_id: snapshot.snapshot_id.clone(),
            source_volume_id: snapshot.source_volume_id.clone(),
            mode,
            device_name,
            device_path: device_path.display().to_string(),
            configfs_path: "userspace:fabric".to_string(),
            size_bytes: snapshot.size_bytes,
            readonly,
            state: "registered".to_string(),
            compaction_job_id: None,
            created_at_millis: now,
            updated_at_millis: now,
        };

        self.register_userspace_snapshot_export(&snapshot, &device)?;
        device.updated_at_millis = unix_now_millis();
        self.append_state_log("snapshot.device.created", &device.device_id, &device)?;
        self.save_snapshot_device(&device)?;

        if request.start_compaction.unwrap_or(false) {
            let job = self.start_snapshot_compaction_for_device(
                &device,
                api::StartSnapshotCompactionRequest::default(),
            )?;
            device.compaction_job_id = Some(job.job_id);
            device.updated_at_millis = unix_now_millis();
            self.append_state_log("snapshot.device.updated", &device.device_id, &device)?;
            self.save_snapshot_device(&device)?;
        }

        Ok(api::SnapshotDeviceResponse { device })
    }

    fn snapshot_device_status(&self, device_id: Option<&str>) -> api::SnapshotDeviceStatusResponse {
        let devices = match device_id {
            Some(device_id) => self
                .load_snapshot_device(device_id)
                .ok()
                .flatten()
                .into_iter()
                .collect(),
            None => self.list_snapshot_device_specs().unwrap_or_default(),
        };
        api::SnapshotDeviceStatusResponse { devices }
    }

    fn delete_snapshot_device(
        &self,
        device_id: &str,
    ) -> Result<api::DeleteSnapshotDeviceResponse, String> {
        if device_id.is_empty() {
            return Err("device_id is required".to_string());
        }
        let existing = self.load_snapshot_device(device_id)?;
        let state_path = self.cfg.snapshot_device_state_path(device_id);
        let response = api::DeleteSnapshotDeviceResponse {
            deleted: existing.is_some() || state_path.exists(),
        };
        self.append_state_log("snapshot.device.deleted", device_id, &response)?;

        let mut deleted = false;
        if let Some(device) = existing.as_ref() {
            self.unregister_userspace_snapshot_export(device)?;
            deleted = true;
        }
        match fs::remove_file(state_path) {
            Ok(()) => deleted = true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("remove snapshot device state: {e}")),
        }
        Ok(api::DeleteSnapshotDeviceResponse { deleted })
    }

    fn start_snapshot_compaction(
        &self,
        device_id: &str,
        request: api::StartSnapshotCompactionRequest,
    ) -> Result<api::SnapshotCompactionResponse, String> {
        if device_id.is_empty() {
            return Err("device_id is required".to_string());
        }
        let mut device = self
            .load_snapshot_device(device_id)?
            .ok_or_else(|| format!("snapshot device {device_id} not found"))?;
        let job = self.start_snapshot_compaction_for_device(&device, request)?;
        device.compaction_job_id = Some(job.job_id.clone());
        device.updated_at_millis = unix_now_millis();
        self.append_state_log("snapshot.device.updated", &device.device_id, &device)?;
        self.save_snapshot_device(&device)?;
        Ok(api::SnapshotCompactionResponse { job })
    }

    fn update_snapshot_compaction(
        &self,
        job_id: &str,
        request: api::UpdateSnapshotCompactionRequest,
    ) -> Result<api::SnapshotCompactionResponse, String> {
        if job_id.is_empty() {
            return Err("job_id is required".to_string());
        }
        let mut job = self
            .load_snapshot_compaction(job_id)?
            .ok_or_else(|| format!("snapshot compaction job {job_id} not found"))?;
        if let Some(state) = request.state {
            job.state = normalize_compaction_state(&state)?;
        }
        if let Some(phase) = request.phase {
            job.phase = normalize_compaction_phase(&phase)?;
        }
        if let Some(bytes_compacted) = request.bytes_compacted {
            job.bytes_compacted = bytes_compacted;
        }
        if let Some(bytes_streamed_out) = request.bytes_streamed_out {
            job.bytes_streamed_out = bytes_streamed_out;
        }
        if let Some(bytes_streamed_in) = request.bytes_streamed_in {
            job.bytes_streamed_in = bytes_streamed_in;
        }
        if let Some(bytes_total) = request.bytes_total {
            job.bytes_total = Some(bytes_total);
        }
        apply_optional_control_value(
            &mut job.outbound_stream_id,
            request.outbound_stream_id,
            "outbound_stream_id",
        )?;
        apply_optional_control_value(
            &mut job.inbound_stream_id,
            request.inbound_stream_id,
            "inbound_stream_id",
        )?;
        apply_optional_control_value(
            &mut job.target_location,
            request.target_location,
            "target_location",
        )?;
        apply_optional_control_value(&mut job.worker_id, request.worker_id, "worker_id")?;
        apply_optional_checkpoint(&mut job.checkpoint, request.checkpoint)?;
        if let Some(error) = request.error {
            let error = error.trim();
            if !error.is_empty() {
                job.error = Some(error.to_string());
                job.state = "failed".to_string();
                job.finished_at_millis.get_or_insert_with(unix_now_millis);
            }
        }
        if request.finished.unwrap_or(false) && job.finished_at_millis.is_none() {
            let finished = unix_now_millis();
            job.finished_at_millis = Some(finished);
            if job.state != "failed" {
                job.state = "succeeded".to_string();
                job.phase = "done".to_string();
            }
        }
        job.updated_at_millis = unix_now_millis();
        self.append_state_log("snapshot.compaction.updated", &job.job_id, &job)?;
        self.save_snapshot_compaction(&job)?;
        Ok(api::SnapshotCompactionResponse { job })
    }

    fn compaction_status(&self, job_id: Option<&str>) -> api::SnapshotCompactionStatusResponse {
        let jobs = match job_id {
            Some(job_id) => self
                .load_snapshot_compaction(job_id)
                .ok()
                .flatten()
                .into_iter()
                .collect(),
            None => self.list_snapshot_compaction_jobs().unwrap_or_default(),
        };
        api::SnapshotCompactionStatusResponse { jobs }
    }

    fn start_receive(
        &self,
        request: api::StartReceiveRequest,
    ) -> Result<api::StartReceiveResponse, String> {
        if request.volume_id.is_empty() {
            return Err("volume_id is required".to_string());
        }
        let listen = if request.listen.is_empty() {
            "0.0.0.0".to_string()
        } else {
            request.listen
        };
        let token = match request.token {
            Some(token) if token != "auto" => {
                validate_token(&token, "token")?;
                token
            }
            _ => zc_stream_generate_token().map_err(|e| format!("generate token: {e}"))?,
        };
        let listener = zc_stream_bind_listener(&listen, (request.port, request.port))
            .map_err(|e| format!("bind replication receiver: {e}"))?;
        let bound_port = listener
            .local_addr()
            .map_err(|e| format!("read replication listener address: {e}"))?
            .port();
        let (writer, limit, target) =
            self.open_replication_target(&request.volume_id, request.bytes)?;
        let repl_id = new_repl_id("recv", &request.volume_id);
        let subject = format!("volume:{}", request.volume_id);
        let replication_mode = self.effective_replication_mode(&subject);
        let replication_route = self.effective_replication_route(&subject);
        let started_at_millis = unix_now_millis();
        self.repl.insert(api::ReplicationJob {
            repl_id: repl_id.clone(),
            role: "receive".to_string(),
            state: "listening".to_string(),
            subject: request.volume_id.clone(),
            peer: listen.clone(),
            port: Some(bound_port),
            bytes: 0,
            bytes_limit: Some(limit),
            error: None,
            started_at_secs: started_at_millis / 1000,
            started_at_millis,
            updated_at_millis: started_at_millis,
            finished_at_secs: None,
            finished_at_millis: None,
            replication_mode: replication_mode.clone(),
            target_cluster: replication_route
                .as_ref()
                .map(|route| route.target_cluster.clone()),
            gateway_endpoint: replication_route
                .as_ref()
                .map(|route| route.gateway_endpoint.clone()),
            spillover_tier: replication_route
                .as_ref()
                .map(|route| route.spillover_tier.clone()),
        });

        let response = api::StartReceiveResponse {
            repl_id: repl_id.clone(),
            role: "receive".to_string(),
            volume_id: request.volume_id.clone(),
            target,
            listen: listen.clone(),
            port: bound_port,
            token: token.clone(),
            replication_mode,
            target_cluster: replication_route
                .as_ref()
                .map(|route| route.target_cluster.clone()),
            gateway_endpoint: replication_route
                .as_ref()
                .map(|route| route.gateway_endpoint.clone()),
            spillover_tier: replication_route
                .as_ref()
                .map(|route| route.spillover_tier.clone()),
        };
        self.append_state_log("stream.receive.started", &repl_id, &response)?;

        let jobs = self.repl.clone();
        let thread_repl_id = repl_id.clone();
        let thread_token = token.clone();
        thread::spawn(move || {
            jobs.mark_state(&thread_repl_id, "running");
            let writer = BoundedWriter::new(writer, limit);
            let writer = ProgressWriter::new(writer, jobs.clone(), thread_repl_id.clone());
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

        Ok(response)
    }

    fn start_send(&self, request: api::StartSendRequest) -> Result<api::StartSendResponse, String> {
        if request.volume_id.is_some() == request.snapshot_id.is_some() {
            return Err("exactly one of volume_id or snapshot_id is required".to_string());
        }
        if request.peer.is_empty() {
            return Err("peer is required".to_string());
        }
        if request.port == 0 {
            return Err("port must be greater than zero".to_string());
        }
        validate_token(&request.token, "token")?;

        let (reader, limit, subject) = self.open_replication_source(
            request.volume_id.as_deref(),
            request.snapshot_id.as_deref(),
            request.bytes,
        )?;
        let repl_id = new_repl_id("send", &subject);
        let replication_mode = self.effective_replication_mode(&subject);
        let replication_route = self.effective_replication_route(&subject);
        let started_at_millis = unix_now_millis();
        self.repl.insert(api::ReplicationJob {
            repl_id: repl_id.clone(),
            role: "send".to_string(),
            state: "queued".to_string(),
            subject: subject.clone(),
            peer: format!("{}:{}", request.peer, request.port),
            port: Some(request.port),
            bytes: 0,
            bytes_limit: Some(limit),
            error: None,
            started_at_secs: started_at_millis / 1000,
            started_at_millis,
            updated_at_millis: started_at_millis,
            finished_at_secs: None,
            finished_at_millis: None,
            replication_mode: replication_mode.clone(),
            target_cluster: replication_route
                .as_ref()
                .map(|route| route.target_cluster.clone()),
            gateway_endpoint: replication_route
                .as_ref()
                .map(|route| route.gateway_endpoint.clone()),
            spillover_tier: replication_route
                .as_ref()
                .map(|route| route.spillover_tier.clone()),
        });

        let response = api::StartSendResponse {
            repl_id: repl_id.clone(),
            role: "send".to_string(),
            source: subject.clone(),
            peer: request.peer.clone(),
            port: request.port,
            bytes_limit: limit,
            replication_mode,
            target_cluster: replication_route
                .as_ref()
                .map(|route| route.target_cluster.clone()),
            gateway_endpoint: replication_route
                .as_ref()
                .map(|route| route.gateway_endpoint.clone()),
            spillover_tier: replication_route
                .as_ref()
                .map(|route| route.spillover_tier.clone()),
        };
        self.append_state_log("stream.send.started", &repl_id, &response)?;

        let jobs = self.repl.clone();
        let thread_repl_id = repl_id.clone();
        let thread_peer = request.peer.clone();
        let thread_token = request.token.clone();
        let port = request.port;
        thread::spawn(move || {
            jobs.mark_state(&thread_repl_id, "running");
            let reader = reader.take(limit);
            let reader = ProgressReader::new(reader, jobs.clone(), thread_repl_id.clone());
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

        Ok(response)
    }

    fn replication_status(&self, repl_id: Option<&str>) -> api::ReplicationStatusResponse {
        api::ReplicationStatusResponse {
            jobs: self.repl.jobs(repl_id),
        }
    }

    fn replication_delay(&self) -> api::ReplicationDelayResponse {
        api::ReplicationDelayResponse {
            samples: self.repl.delay_samples(None),
        }
    }

    fn stats(&self) -> api::StatsResponse {
        build_stats_response(
            self.repl.delay_samples(None),
            self.list_snapshot_compaction_jobs().unwrap_or_default(),
            unix_now_millis(),
        )
    }

    fn prometheus_metrics(&self) -> String {
        render_prometheus_metrics(
            &self.repl.delay_samples(None),
            &self.list_snapshot_compaction_jobs().unwrap_or_default(),
        )
    }

    fn replication_mode_status(&self) -> api::ReplicationModeStatusResponse {
        api::ReplicationModeStatusResponse {
            policies: self.replication_modes.list(),
        }
    }

    fn set_replication_mode(
        &self,
        request: api::SetReplicationModeRequest,
    ) -> Result<api::ReplicationModeResponse, String> {
        let scope = normalize_replication_scope(request.scope.as_deref())?;
        let mode = normalize_replication_mode(&request.mode)?;
        let policy = self
            .replication_modes
            .next_policy(&scope, &mode, unix_now_secs());
        self.append_state_log("replication.mode.changed", &scope, &policy)?;
        self.replication_modes.apply(policy.clone());
        self.repl.apply_replication_mode(&scope, &mode);
        Ok(api::ReplicationModeResponse { policy })
    }

    fn effective_replication_mode(&self, subject: &str) -> String {
        self.replication_modes.effective_mode(subject)
    }

    fn replication_route_status(&self) -> api::ReplicationRouteStatusResponse {
        api::ReplicationRouteStatusResponse {
            routes: self.replication_routes.list(),
        }
    }

    fn set_replication_route(
        &self,
        request: api::SetReplicationRouteRequest,
    ) -> Result<api::ReplicationRouteResponse, String> {
        let scope = normalize_replication_scope(request.scope.as_deref())?;
        let target_cluster = normalize_control_value(&request.target_cluster, "target_cluster")?;
        let gateway_endpoint =
            normalize_control_value(&request.gateway_endpoint, "gateway_endpoint")?;
        let spillover_tier = normalize_control_value(&request.spillover_tier, "spillover_tier")?;
        let route = self.replication_routes.next_route(
            &scope,
            &target_cluster,
            &gateway_endpoint,
            &spillover_tier,
            unix_now_secs(),
        );
        self.append_state_log("replication.route.changed", &scope, &route)?;
        self.replication_routes.apply(route.clone());
        self.repl.apply_replication_route(&scope, &route);
        Ok(api::ReplicationRouteResponse { route })
    }

    fn effective_replication_route(&self, subject: &str) -> Option<api::ReplicationRouteSpec> {
        self.replication_routes.effective_route(subject)
    }

    fn logstream(&self) -> Result<api::LogStreamResponse, String> {
        Ok(api::LogStreamResponse {
            entries: self
                .log
                .replay()
                .map_err(|e| format!("replay logstream: {e}"))?,
        })
    }

    fn append_state_log<T: serde::Serialize>(
        &self,
        kind: &str,
        key: &str,
        value: &T,
    ) -> Result<(), String> {
        self.log
            .append_state(kind, key, value)
            .map(|_| ())
            .map_err(|e| format!("append zccusan state log {kind}/{key}: {e}"))
    }

    fn materialize_state_from_log(&self) -> Result<(), String> {
        let entries = self
            .log
            .replay()
            .map_err(|e| format!("read logstream: {e}"))?;
        let mut snapshots = BTreeMap::<String, api::SnapshotSpec>::new();
        let mut deleted = BTreeSet::<String>::new();
        let mut snapshot_devices = BTreeMap::<String, api::SnapshotDeviceSpec>::new();
        let mut deleted_snapshot_devices = BTreeSet::<String>::new();
        let mut compactions = BTreeMap::<String, api::SnapshotCompactionJob>::new();
        for entry in entries {
            match entry.kind.as_str() {
                "snapshot.created" => {
                    let spec: api::SnapshotSpec = serde_json::from_value(entry.value)
                        .map_err(|e| format!("decode snapshot log entry {}: {e}", entry.key))?;
                    deleted.remove(&spec.snapshot_id);
                    snapshots.insert(spec.snapshot_id.clone(), spec);
                }
                "snapshot.deleted" => {
                    snapshots.remove(&entry.key);
                    deleted.insert(entry.key);
                }
                "snapshot.device.created" | "snapshot.device.updated" => {
                    let spec: api::SnapshotDeviceSpec = serde_json::from_value(entry.value)
                        .map_err(|e| {
                            format!("decode snapshot device log entry {}: {e}", entry.key)
                        })?;
                    deleted_snapshot_devices.remove(&spec.device_id);
                    snapshot_devices.insert(spec.device_id.clone(), spec);
                }
                "snapshot.device.deleted" => {
                    snapshot_devices.remove(&entry.key);
                    deleted_snapshot_devices.insert(entry.key);
                }
                "snapshot.compaction.started" | "snapshot.compaction.updated" => {
                    let job: api::SnapshotCompactionJob = serde_json::from_value(entry.value)
                        .map_err(|e| format!("decode compaction log entry {}: {e}", entry.key))?;
                    compactions.insert(job.job_id.clone(), job);
                }
                "replication.mode.changed" => {
                    let policy: api::ReplicationModeSpec = serde_json::from_value(entry.value)
                        .map_err(|e| {
                            format!("decode replication mode log entry {}: {e}", entry.key)
                        })?;
                    self.replication_modes.apply(policy);
                }
                "replication.route.changed" => {
                    let route: api::ReplicationRouteSpec = serde_json::from_value(entry.value)
                        .map_err(|e| {
                            format!("decode replication route log entry {}: {e}", entry.key)
                        })?;
                    self.replication_routes.apply(route);
                }
                _ => {}
            }
        }
        for snapshot_id in deleted {
            match fs::remove_file(self.cfg.snapshot_state_path(&snapshot_id)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("remove deleted snapshot state: {e}")),
            }
        }
        for spec in snapshots.values() {
            self.save_snapshot(spec)?;
        }
        for device_id in deleted_snapshot_devices {
            match fs::remove_file(self.cfg.snapshot_device_state_path(&device_id)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("remove deleted snapshot device state: {e}")),
            }
        }
        for spec in snapshot_devices.values() {
            self.save_snapshot_device(spec)?;
        }
        for job in compactions.values() {
            self.save_snapshot_compaction(job)?;
        }
        Ok(())
    }

    fn load_volume(&self, volume_id: &str) -> Result<Option<VolumeSpec>, String> {
        let path = self.cfg.state_path(volume_id);
        match fs::read_to_string(&path) {
            Ok(body) => Ok(Some(VolumeSpec::from_state(&body)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("read volume state: {e}")),
        }
    }

    fn load_snapshot(&self, snapshot_id: &str) -> Result<Option<api::SnapshotSpec>, String> {
        let path = self.cfg.snapshot_state_path(snapshot_id);
        match fs::read_to_string(&path) {
            Ok(body) => Ok(Some(snapshot_from_state(&body)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("read snapshot state: {e}")),
        }
    }

    fn save_snapshot(&self, spec: &api::SnapshotSpec) -> Result<(), String> {
        fs::create_dir_all(self.cfg.snapshots_dir())
            .map_err(|e| format!("create snapshot state dir: {e}"))?;
        fs::write(
            self.cfg.snapshot_state_path(&spec.snapshot_id),
            snapshot_to_state(spec),
        )
        .map_err(|e| format!("write snapshot state: {e}"))
    }

    fn load_snapshot_device(
        &self,
        device_id: &str,
    ) -> Result<Option<api::SnapshotDeviceSpec>, String> {
        let path = self.cfg.snapshot_device_state_path(device_id);
        match fs::read_to_string(&path) {
            Ok(body) => Ok(Some(snapshot_device_from_state(&body)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("read snapshot device state: {e}")),
        }
    }

    fn save_snapshot_device(&self, spec: &api::SnapshotDeviceSpec) -> Result<(), String> {
        fs::create_dir_all(self.cfg.snapshot_devices_dir())
            .map_err(|e| format!("create snapshot device state dir: {e}"))?;
        fs::write(
            self.cfg.snapshot_device_state_path(&spec.device_id),
            snapshot_device_to_state(spec),
        )
        .map_err(|e| format!("write snapshot device state: {e}"))
    }

    fn list_snapshot_device_specs(&self) -> Result<Vec<api::SnapshotDeviceSpec>, String> {
        let mut specs = Vec::new();
        match fs::read_dir(self.cfg.snapshot_devices_dir()) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| format!("read snapshot device dir: {e}"))?;
                    if !entry.path().is_file() {
                        continue;
                    }
                    let body = fs::read_to_string(entry.path())
                        .map_err(|e| format!("read snapshot device state: {e}"))?;
                    specs.push(snapshot_device_from_state(&body)?);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("read snapshot device dir: {e}")),
        }
        specs.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        Ok(specs)
    }

    fn load_snapshot_compaction(
        &self,
        job_id: &str,
    ) -> Result<Option<api::SnapshotCompactionJob>, String> {
        let path = self.cfg.compaction_state_path(job_id);
        match fs::read_to_string(&path) {
            Ok(body) => Ok(Some(snapshot_compaction_from_state(&body)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("read snapshot compaction state: {e}")),
        }
    }

    fn save_snapshot_compaction(&self, job: &api::SnapshotCompactionJob) -> Result<(), String> {
        fs::create_dir_all(self.cfg.compactions_dir())
            .map_err(|e| format!("create compaction state dir: {e}"))?;
        fs::write(
            self.cfg.compaction_state_path(&job.job_id),
            snapshot_compaction_to_state(job),
        )
        .map_err(|e| format!("write snapshot compaction state: {e}"))
    }

    fn list_snapshot_compaction_jobs(&self) -> Result<Vec<api::SnapshotCompactionJob>, String> {
        let mut jobs = Vec::new();
        match fs::read_dir(self.cfg.compactions_dir()) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| format!("read compaction dir: {e}"))?;
                    if !entry.path().is_file() {
                        continue;
                    }
                    let body = fs::read_to_string(entry.path())
                        .map_err(|e| format!("read compaction state: {e}"))?;
                    jobs.push(snapshot_compaction_from_state(&body)?);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("read compaction dir: {e}")),
        }
        jobs.sort_by(|a, b| a.job_id.cmp(&b.job_id));
        Ok(jobs)
    }

    fn register_userspace_snapshot_export(
        &self,
        snapshot: &api::SnapshotSpec,
        device: &api::SnapshotDeviceSpec,
    ) -> Result<(), String> {
        if snapshot.snapshot_id != device.snapshot_id {
            return Err("snapshot export does not match source snapshot".to_string());
        }
        if snapshot.source_volume_id != device.source_volume_id {
            return Err("snapshot export does not match source volume".to_string());
        }
        if !device.readonly {
            return Err("snapshot exports must be read-only".to_string());
        }
        Ok(())
    }

    fn unregister_userspace_snapshot_export(
        &self,
        _device: &api::SnapshotDeviceSpec,
    ) -> Result<(), String> {
        Ok(())
    }

    fn start_snapshot_compaction_for_device(
        &self,
        device: &api::SnapshotDeviceSpec,
        request: api::StartSnapshotCompactionRequest,
    ) -> Result<api::SnapshotCompactionJob, String> {
        let strategy = normalize_compaction_strategy(request.strategy.as_deref())?;
        let phase = initial_compaction_phase(&strategy).to_string();
        let now = unix_now_millis();
        let job = api::SnapshotCompactionJob {
            job_id: new_compaction_job_id(&device.device_id),
            device_id: device.device_id.clone(),
            snapshot_id: device.snapshot_id.clone(),
            mode: device.mode.clone(),
            strategy,
            phase,
            state: "queued".to_string(),
            bytes_compacted: 0,
            bytes_streamed_out: 0,
            bytes_streamed_in: 0,
            bytes_total: request
                .bytes_total
                .or_else(|| u64::try_from(device.size_bytes).ok()),
            outbound_stream_id: normalize_optional_control_value(
                request.outbound_stream_id,
                "outbound_stream_id",
            )?,
            inbound_stream_id: normalize_optional_control_value(
                request.inbound_stream_id,
                "inbound_stream_id",
            )?,
            target_location: normalize_optional_control_value(
                request.target_location,
                "target_location",
            )?,
            worker_id: normalize_optional_control_value(request.worker_id, "worker_id")?,
            checkpoint: normalize_optional_checkpoint(request.checkpoint)?,
            started_at_millis: now,
            updated_at_millis: now,
            finished_at_millis: None,
            error: None,
        };
        self.append_state_log("snapshot.compaction.started", &job.job_id, &job)?;
        self.save_snapshot_compaction(&job)?;
        Ok(job)
    }

    fn snapshot_volume(
        &self,
        source: &VolumeSpec,
        snapshot_id: &str,
        name: &str,
    ) -> Result<api::SnapshotSpec, String> {
        fs::create_dir_all(self.cfg.snapshot_images_dir())
            .map_err(|e| format!("create snapshot image dir: {e}"))?;
        let snapshot_path = self
            .cfg
            .snapshot_images_dir()
            .join(format!("{snapshot_id}.img"));
        let size_bytes = volume_capacity_u64(source)?;
        let mut snapshot_mode = "existing".to_string();
        if !snapshot_path.exists() {
            let source_path = self.replication_volume_path(source)?;
            let tmp_path = snapshot_path.with_extension("img.tmp");
            remove_file_if_exists(&tmp_path)
                .map_err(|e| format!("remove stale snapshot temp image: {e}"))?;
            snapshot_mode =
                self.create_snapshot_image(source, &source_path, &tmp_path, size_bytes)?;
            fs::rename(&tmp_path, &snapshot_path).map_err(|e| format!("commit snapshot: {e}"))?;
        }

        Ok(api::SnapshotSpec {
            snapshot_id: snapshot_id.to_string(),
            name_hex: hex_encode(name.as_bytes()),
            source_volume_id: source.volume_id.clone(),
            source_backend: source.backend.clone(),
            size_bytes: i64::try_from(size_bytes).map_err(|_| "snapshot is too large")?,
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
    ) -> Result<String, String> {
        if self.cfg.snapshot_mode == "reflink" && source.backend != "file-loop" {
            return Err(format!(
                "snapshot-mode=reflink requires backend=file-loop, got {}",
                source.backend
            ));
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
                    return Err(format!(
                        "zero-copy reflink PIT snapshot failed for {}: {e}",
                        source_path.display()
                    ));
                }
            }
        }
        copy_exact_bytes_to_file(source_path, tmp_path, size_bytes)?;
        Ok("copy".to_string())
    }

    fn open_replication_source(
        &self,
        volume_id: Option<&str>,
        snapshot_id: Option<&str>,
        bytes: Option<u64>,
    ) -> Result<(Box<dyn Read + Send>, u64, String), String> {
        match (volume_id, snapshot_id) {
            (Some(_), Some(_)) => {
                Err("requires either volume_id or snapshot_id, not both".to_string())
            }
            (None, None) => Err("requires volume_id or snapshot_id".to_string()),
            (Some(volume_id), None) => {
                let spec = self
                    .load_volume(volume_id)?
                    .ok_or_else(|| format!("volume {volume_id} not found"))?;
                let path = self.replication_volume_path(&spec)?;
                let capacity = volume_capacity_u64(&spec)?;
                let limit = replication_limit(bytes, capacity, "source volume")?;
                let file = OpenOptions::new()
                    .read(true)
                    .open(&path)
                    .map_err(|e| format!("open replication source volume: {e}"))?;
                Ok((Box::new(file), limit, format!("volume:{volume_id}")))
            }
            (None, Some(snapshot_id)) => {
                let spec = self
                    .load_snapshot(snapshot_id)?
                    .ok_or_else(|| format!("snapshot {snapshot_id} not found"))?;
                if !spec.ready_to_use {
                    return Err(format!("snapshot {snapshot_id} is not ready"));
                }
                let capacity = u64::try_from(spec.size_bytes)
                    .map_err(|_| "snapshot size must be non-negative".to_string())?;
                let limit = replication_limit(bytes, capacity, "source snapshot")?;
                let file = OpenOptions::new()
                    .read(true)
                    .open(&spec.snapshot_path)
                    .map_err(|e| format!("open replication source snapshot: {e}"))?;
                Ok((Box::new(file), limit, format!("snapshot:{snapshot_id}")))
            }
        }
    }

    fn open_replication_target(
        &self,
        volume_id: &str,
        bytes: Option<u64>,
    ) -> Result<(Box<dyn Write + Send>, u64, String), String> {
        let spec = self
            .load_volume(volume_id)?
            .ok_or_else(|| format!("volume {volume_id} not found"))?;
        let path = self.replication_volume_path(&spec)?;
        let capacity = volume_capacity_u64(&spec)?;
        let limit = replication_limit(bytes, capacity, "target volume")?;
        let file = OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(|e| format!("open replication target volume: {e}"))?;
        Ok((Box::new(file), limit, path.display().to_string()))
    }

    fn replication_volume_path(&self, spec: &VolumeSpec) -> Result<PathBuf, String> {
        match spec.backend.as_str() {
            "zcbrd" => self.ensure_zcbrd_device(spec),
            "file-loop" => {
                self.create_backing_file(spec)?;
                Ok(PathBuf::from(spec.file_path.as_ref().ok_or_else(|| {
                    "file-loop volume missing file_path".to_string()
                })?))
            }
            "raw-block" => self.ensure_raw_block_device(spec),
            other => Err(format!("unsupported backend: {other}")),
        }
    }

    fn create_backing_file(&self, spec: &VolumeSpec) -> Result<(), String> {
        let path = spec
            .file_path
            .as_ref()
            .ok_or_else(|| "file-loop volume missing file_path".to_string())?;
        let path = Path::new(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create file-loop root: {e}"))?;
        }
        if !path.exists() {
            if let Some(restore_path) = spec.restore_path.as_ref() {
                restore_image_to_file(
                    Path::new(restore_path),
                    path,
                    u64::try_from(spec.capacity_bytes)
                        .map_err(|_| "file-loop volume capacity must be non-negative")?,
                )?;
                return Ok(());
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("create file-loop backing file: {e}"))?;
        file.set_len(
            u64::try_from(spec.capacity_bytes)
                .map_err(|_| "file-loop volume capacity must be non-negative")?,
        )
        .map_err(|e| format!("size file-loop backing file: {e}"))
    }

    fn ensure_raw_block_device(&self, spec: &VolumeSpec) -> Result<PathBuf, String> {
        let raw_device = spec
            .raw_device
            .as_ref()
            .ok_or_else(|| "raw-block volume missing raw_device".to_string())?;
        let path = canonical_block_device(Path::new(raw_device))?;
        let partuuid = partuuid_for_device(&path)
            .ok_or_else(|| format!("{} has no PARTUUID allowlist identity", path.display()))?;
        self.ensure_partuuid_allowlisted(&partuuid)?;
        Ok(path)
    }

    fn ensure_partuuid_allowlisted(&self, target: &str) -> Result<(), String> {
        let target = normalize_partuuid(target)?;
        let allowed = read_raw_allowlist(&self.cfg.raw_allowlist)?;
        if allowed.iter().any(|uuid| uuid == &target) {
            Ok(())
        } else {
            Err(format!(
                "PARTUUID={} is not listed in {}",
                target,
                self.cfg.raw_allowlist.display()
            ))
        }
    }

    fn ensure_zcbrd_device(&self, spec: &VolumeSpec) -> Result<PathBuf, String> {
        if !self.cfg.configfs_root.is_dir() {
            return Err(format!(
                "{} is not available; load zcbrd_mod and mount configfs",
                self.cfg.configfs_root.display()
            ));
        }

        let dir = self.cfg.configfs_root.join(&spec.device_name);
        if !dir.exists() {
            fs::create_dir(&dir).map_err(|e| format!("create zcbrd configfs device: {e}"))?;
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
                            u64::try_from(spec.capacity_bytes)
                                .map_err(|_| "zcbrd volume capacity must be non-negative")?,
                        )?;
                    }
                }
                return Ok(dev);
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err(format!(
            "{} did not appear after powering {}",
            dev.display(),
            spec.device_name
        ))
    }
}

impl FreezeManager {
    fn new(cfg: Arc<Config>, log: Arc<FileLogStream>) -> Self {
        Self {
            cfg,
            log,
            state: Mutex::new(FreezeState::default()),
            op: Mutex::new(()),
        }
    }

    fn freeze(
        self: &Arc<Self>,
        barrier_id: String,
        ttl_ms: u64,
    ) -> Result<api::FreezeResponse, String> {
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

        let _guard = self.op.lock().expect("freeze op mutex poisoned");
        self.expire_active_under_op();
        {
            let state = self.state.lock().expect("freeze state mutex poisoned");
            if let Some(active) = state.active.as_ref() {
                if active.barrier_id == barrier_id {
                    return Ok(freeze_response(active));
                }
                return Err(format!(
                    "busy active_barrier={} remaining_ms={}",
                    active.barrier_id,
                    remaining_ms(active.deadline)
                ));
            }
        }

        let deadline = Instant::now() + Duration::from_millis(ttl_ms);
        let mounts = self.discover_freezable_mounts()?;
        let mut frozen = Vec::new();
        for mount in mounts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.thaw_mounts(&frozen);
                return Err("deadline elapsed before freeze completed".to_string());
            }
            let command_timeout = remaining.min(Duration::from_millis(FREEZE_COMMAND_TIMEOUT_MS));
            if let Err(e) = fsfreeze_path(&mount, true, command_timeout) {
                let _ = self.thaw_mounts(&frozen);
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
            let mut state = self.state.lock().expect("freeze state mutex poisoned");
            state.active = Some(active);
        }

        let manager = self.clone();
        let auto_barrier_id = barrier_id.clone();
        thread::spawn(move || {
            thread::sleep(deadline.saturating_duration_since(Instant::now()));
            manager.release_if_expired(&auto_barrier_id);
        });

        Ok(api::FreezeResponse {
            barrier_id,
            frozen_mounts: path_strings(&frozen),
            remaining_ms: remaining_ms(deadline),
        })
    }

    fn release(&self, barrier_id: &str) -> Result<api::FreezeResponse, String> {
        validate_barrier_id(barrier_id)?;
        let _guard = self.op.lock().expect("freeze op mutex poisoned");
        self.release_under_op(barrier_id, false)
    }

    fn status(&self) -> api::FreezeStatusResponse {
        let state = self.state.lock().expect("freeze state mutex poisoned");
        match state.active.as_ref() {
            Some(active) => api::FreezeStatusResponse {
                active: true,
                barrier_id: Some(active.barrier_id.clone()),
                frozen_mounts: path_strings(&active.frozen_mounts),
                remaining_ms: remaining_ms(active.deadline),
            },
            None => api::FreezeStatusResponse {
                active: false,
                barrier_id: None,
                frozen_mounts: Vec::new(),
                remaining_ms: 0,
            },
        }
    }

    fn thaw_stale_mounts_on_startup(&self) {
        let mounts = match self.discover_freezable_mounts() {
            Ok(mounts) => mounts,
            Err(e) => {
                eprintln!("startup thaw discovery failed: {e}");
                return;
            }
        };
        if mounts.is_empty() {
            return;
        }
        let errors = self.thaw_mounts(&mounts);
        if !errors.is_empty() {
            eprintln!("startup thaw errors: {}", errors.join("; "));
        }
    }

    fn release_if_expired(&self, barrier_id: &str) {
        let _guard = self.op.lock().expect("freeze op mutex poisoned");
        let should_log_expiry = {
            let state = self.state.lock().expect("freeze state mutex poisoned");
            matches!(
                state.active.as_ref(),
                Some(active) if active.barrier_id == barrier_id && Instant::now() >= active.deadline
            )
        };
        match self.release_under_op(barrier_id, true) {
            Ok(response) if should_log_expiry => {
                if let Err(e) =
                    self.append_state_log("freeze.expired", &response.barrier_id, &response)
                {
                    eprintln!("append freeze expiry log failed: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("auto-thaw for barrier {barrier_id} failed: {e}"),
        }
    }

    fn release_under_op(
        &self,
        barrier_id: &str,
        only_if_expired: bool,
    ) -> Result<api::FreezeResponse, String> {
        let active = {
            let mut state = self.state.lock().expect("freeze state mutex poisoned");
            let Some(active) = state.active.as_ref() else {
                return Ok(api::FreezeResponse {
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
                return Ok(api::FreezeResponse {
                    barrier_id: barrier_id.to_string(),
                    frozen_mounts: Vec::new(),
                    remaining_ms: remaining_ms(active.deadline),
                });
            }
            state.active.take().expect("active freeze disappeared")
        };
        let thaw_errors = self.thaw_mounts(&active.frozen_mounts);
        if !thaw_errors.is_empty() {
            return Err(format!("thaw errors: {}", thaw_errors.join("; ")));
        }
        Ok(api::FreezeResponse {
            barrier_id: active.barrier_id,
            frozen_mounts: path_strings(&active.frozen_mounts),
            remaining_ms: 0,
        })
    }

    fn expire_active_under_op(&self) {
        let expired = {
            let mut state = self.state.lock().expect("freeze state mutex poisoned");
            match state.active.as_ref() {
                Some(active) if Instant::now() >= active.deadline => state.active.take(),
                _ => None,
            }
        };
        if let Some(active) = expired {
            let errors = self.thaw_mounts(&active.frozen_mounts);
            if !errors.is_empty() {
                eprintln!(
                    "expired barrier {} thaw errors: {}",
                    active.barrier_id,
                    errors.join("; ")
                );
            }
            let response = api::FreezeResponse {
                barrier_id: active.barrier_id.clone(),
                frozen_mounts: path_strings(&active.frozen_mounts),
                remaining_ms: 0,
            };
            if let Err(e) = self.append_state_log("freeze.expired", &response.barrier_id, &response)
            {
                eprintln!("append freeze expiry log failed: {e}");
            }
        }
    }

    fn append_state_log<T: serde::Serialize>(
        &self,
        kind: &str,
        key: &str,
        value: &T,
    ) -> Result<(), String> {
        self.log
            .append_state(kind, key, value)
            .map(|_| ())
            .map_err(|e| format!("append zccusan state log {kind}/{key}: {e}"))
    }

    fn discover_freezable_mounts(&self) -> Result<Vec<PathBuf>, String> {
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
                    let spec = VolumeSpec::from_state(&body)?;
                    if spec.backend == "raw-block" {
                        continue;
                    }
                    let Some(staging_path) = spec.staging_path.as_ref() else {
                        continue;
                    };
                    let path = PathBuf::from(staging_path);
                    if is_mountpoint(&path) {
                        mounts.insert(path);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("read volume state dir: {e}")),
        }
        Ok(mounts.into_iter().collect())
    }

    fn thaw_mounts(&self, mounts: &[PathBuf]) -> Vec<String> {
        let mut errors = Vec::new();
        for mount in mounts.iter().rev() {
            match fsfreeze_path(
                mount,
                false,
                Duration::from_millis(FREEZE_COMMAND_TIMEOUT_MS),
            ) {
                Ok(()) => {}
                Err(e) if is_already_thawed_error(&e) => {}
                Err(e) => errors.push(format!("{}: {e}", mount.display())),
            }
        }
        errors
    }
}

impl ReplicationManager {
    fn insert(&self, job: api::ReplicationJob) {
        let mut jobs = self.jobs.lock().expect("replication job mutex poisoned");
        jobs.insert(job.repl_id.clone(), job);
    }

    fn mark_state(&self, repl_id: &str, state: &str) {
        let mut jobs = self.jobs.lock().expect("replication job mutex poisoned");
        if let Some(job) = jobs.get_mut(repl_id) {
            job.state = state.to_string();
            job.updated_at_millis = unix_now_millis();
        }
    }

    fn add_progress(&self, repl_id: &str, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut jobs = self.jobs.lock().expect("replication job mutex poisoned");
        if let Some(job) = jobs.get_mut(repl_id) {
            job.bytes = job.bytes.saturating_add(bytes);
            job.updated_at_millis = unix_now_millis();
        }
    }

    fn mark_done(&self, repl_id: &str, bytes: u64) {
        let mut jobs = self.jobs.lock().expect("replication job mutex poisoned");
        if let Some(job) = jobs.get_mut(repl_id) {
            let now_millis = unix_now_millis();
            job.state = "succeeded".to_string();
            job.bytes = bytes;
            job.error = None;
            job.updated_at_millis = now_millis;
            job.finished_at_secs = Some(now_millis / 1000);
            job.finished_at_millis = Some(now_millis);
        }
    }

    fn mark_error(&self, repl_id: &str, error: String) {
        let mut jobs = self.jobs.lock().expect("replication job mutex poisoned");
        if let Some(job) = jobs.get_mut(repl_id) {
            let now_millis = unix_now_millis();
            job.state = "failed".to_string();
            job.error = Some(error);
            job.updated_at_millis = now_millis;
            job.finished_at_secs = Some(now_millis / 1000);
            job.finished_at_millis = Some(now_millis);
        }
    }

    fn jobs(&self, repl_id: Option<&str>) -> Vec<api::ReplicationJob> {
        let jobs = self.jobs.lock().expect("replication job mutex poisoned");
        match repl_id {
            Some(repl_id) => jobs.get(repl_id).cloned().into_iter().collect(),
            None => jobs.values().cloned().collect(),
        }
    }

    fn delay_samples(&self, repl_id: Option<&str>) -> Vec<api::ReplicationDelaySample> {
        let now_millis = unix_now_millis();
        let jobs = self.jobs.lock().expect("replication job mutex poisoned");
        let values: Vec<api::ReplicationJob> = match repl_id {
            Some(repl_id) => jobs.get(repl_id).cloned().into_iter().collect(),
            None => jobs.values().cloned().collect(),
        };
        values
            .into_iter()
            .map(|job| delay_sample(job, now_millis))
            .collect()
    }

    fn apply_replication_mode(&self, scope: &str, mode: &str) {
        let mut jobs = self.jobs.lock().expect("replication job mutex poisoned");
        for job in jobs.values_mut() {
            if !is_terminal_replication_state(&job.state) && job_matches_scope(job, scope) {
                job.replication_mode = mode.to_string();
            }
        }
    }

    fn apply_replication_route(&self, scope: &str, route: &api::ReplicationRouteSpec) {
        let mut jobs = self.jobs.lock().expect("replication job mutex poisoned");
        for job in jobs.values_mut() {
            if !is_terminal_replication_state(&job.state) && job_matches_scope(job, scope) {
                job.target_cluster = Some(route.target_cluster.clone());
                job.gateway_endpoint = Some(route.gateway_endpoint.clone());
                job.spillover_tier = Some(route.spillover_tier.clone());
            }
        }
    }
}

impl ReplicationModeStore {
    fn next_policy(
        &self,
        scope: &str,
        mode: &str,
        updated_at_secs: u64,
    ) -> api::ReplicationModeSpec {
        let policies = self
            .policies
            .lock()
            .expect("replication mode mutex poisoned");
        let generation = policies
            .get(scope)
            .map(|policy| policy.generation.saturating_add(1))
            .unwrap_or(1);
        api::ReplicationModeSpec {
            scope: scope.to_string(),
            mode: mode.to_string(),
            generation,
            updated_at_secs,
        }
    }

    fn apply(&self, policy: api::ReplicationModeSpec) {
        let mut policies = self
            .policies
            .lock()
            .expect("replication mode mutex poisoned");
        let should_apply = policies
            .get(&policy.scope)
            .map(|existing| existing.generation <= policy.generation)
            .unwrap_or(true);
        if should_apply {
            policies.insert(policy.scope.clone(), policy);
        }
    }

    fn effective_mode(&self, subject: &str) -> String {
        let policies = self
            .policies
            .lock()
            .expect("replication mode mutex poisoned");
        policies
            .get(subject)
            .or_else(|| policies.get(api::GLOBAL_REPLICATION_SCOPE))
            .map(|policy| policy.mode.clone())
            .unwrap_or_else(default_replication_mode)
    }

    fn list(&self) -> Vec<api::ReplicationModeSpec> {
        let policies = self
            .policies
            .lock()
            .expect("replication mode mutex poisoned");
        let mut values = policies.values().cloned().collect::<Vec<_>>();
        if !policies.contains_key(api::GLOBAL_REPLICATION_SCOPE) {
            values.insert(0, default_replication_mode_policy());
        }
        values.sort_by(|a, b| a.scope.cmp(&b.scope));
        values
    }
}

impl ReplicationRouteStore {
    fn next_route(
        &self,
        scope: &str,
        target_cluster: &str,
        gateway_endpoint: &str,
        spillover_tier: &str,
        updated_at_secs: u64,
    ) -> api::ReplicationRouteSpec {
        let routes = self
            .routes
            .lock()
            .expect("replication route mutex poisoned");
        let generation = routes
            .get(scope)
            .map(|route| route.generation.saturating_add(1))
            .unwrap_or(1);
        api::ReplicationRouteSpec {
            scope: scope.to_string(),
            target_cluster: target_cluster.to_string(),
            gateway_endpoint: gateway_endpoint.to_string(),
            spillover_tier: spillover_tier.to_string(),
            generation,
            updated_at_secs,
        }
    }

    fn apply(&self, route: api::ReplicationRouteSpec) {
        let mut routes = self
            .routes
            .lock()
            .expect("replication route mutex poisoned");
        let should_apply = routes
            .get(&route.scope)
            .map(|existing| existing.generation <= route.generation)
            .unwrap_or(true);
        if should_apply {
            routes.insert(route.scope.clone(), route);
        }
    }

    fn effective_route(&self, subject: &str) -> Option<api::ReplicationRouteSpec> {
        let routes = self
            .routes
            .lock()
            .expect("replication route mutex poisoned");
        routes
            .get(subject)
            .or_else(|| routes.get(api::GLOBAL_REPLICATION_SCOPE))
            .cloned()
    }

    fn list(&self) -> Vec<api::ReplicationRouteSpec> {
        let routes = self
            .routes
            .lock()
            .expect("replication route mutex poisoned");
        let mut values = routes.values().cloned().collect::<Vec<_>>();
        values.sort_by(|a, b| a.scope.cmp(&b.scope));
        values
    }
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

impl<R> ProgressReader<R> {
    fn new(inner: R, jobs: Arc<ReplicationManager>, repl_id: String) -> Self {
        Self {
            inner,
            jobs,
            repl_id,
        }
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.jobs.add_progress(&self.repl_id, read as u64);
        Ok(read)
    }
}

impl<W> ProgressWriter<W> {
    fn new(inner: W, jobs: Arc<ReplicationManager>, repl_id: String) -> Self {
        Self {
            inner,
            jobs,
            repl_id,
        }
    }
}

impl<W: Write> Write for ProgressWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.jobs.add_progress(&self.repl_id, written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl VolumeSpec {
    fn from_state(body: &str) -> Result<Self, String> {
        let map = parse_key_values(body);
        Ok(Self {
            backend: map
                .get("backend")
                .cloned()
                .unwrap_or_else(|| "zcbrd".to_string()),
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
}

fn handle_connection(mut stream: TcpStream, app: ControlApp) -> io::Result<()> {
    let response = match read_http_request(&mut stream) {
        Ok(request) => route_request(request, &app),
        Err(e) => error_response(400, &e.to_string()),
    };
    write_http_response(&mut stream, response)
}

fn route_request(request: HttpRequest, app: &ControlApp) -> HttpResponse {
    let path = request.path.split('?').next().unwrap_or("/");
    match (request.method.as_str(), path) {
        ("GET", "/openapi.yaml") => HttpResponse {
            status: 200,
            content_type: "application/yaml",
            body: OPENAPI_YAML.as_bytes().to_vec(),
        },
        ("GET", "/v1/healthz") => json_response(
            200,
            &api::HealthResponse {
                ok: true,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        ),
        ("GET", "/v1/logstream") => result_response(app.logstream()),
        ("GET", "/metrics") => HttpResponse {
            status: 200,
            content_type: "text/plain; version=0.0.4; charset=utf-8",
            body: app.prometheus_metrics().into_bytes(),
        },
        ("GET", "/v1/stats") => json_response(200, &app.stats()),
        ("GET", "/v1/replication/delay") => json_response(200, &app.replication_delay()),
        ("GET", "/v1/replication/modes") => json_response(200, &app.replication_mode_status()),
        ("PUT", "/v1/replication/modes") | ("POST", "/v1/replication/modes") => {
            let request = match parse_json::<api::SetReplicationModeRequest>(&request.body) {
                Ok(request) => request,
                Err(e) => return error_response(400, &e),
            };
            result_response(app.set_replication_mode(request))
        }
        ("GET", "/v1/replication/routes") => json_response(200, &app.replication_route_status()),
        ("PUT", "/v1/replication/routes") | ("POST", "/v1/replication/routes") => {
            let request = match parse_json::<api::SetReplicationRouteRequest>(&request.body) {
                Ok(request) => request,
                Err(e) => return error_response(400, &e),
            };
            result_response(app.set_replication_route(request))
        }
        ("POST", "/v1/snapshots") => {
            let request = match parse_json::<api::CreateSnapshotRequest>(&request.body) {
                Ok(request) => request,
                Err(e) => return error_response(400, &e),
            };
            result_response(app.create_snapshot(request))
        }
        ("GET", "/v1/snapshot-devices") => json_response(200, &app.snapshot_device_status(None)),
        ("POST", "/v1/snapshot-devices") => {
            let request = match parse_json::<api::CreateSnapshotDeviceRequest>(&request.body) {
                Ok(request) => request,
                Err(e) => return error_response(400, &e),
            };
            result_response(app.create_snapshot_device(request))
        }
        ("GET", "/v1/compactions") => json_response(200, &app.compaction_status(None)),
        ("GET", "/v1/freeze") => json_response(200, &app.freeze.status()),
        ("POST", "/v1/freeze") => {
            let request = match parse_json::<api::FreezeRequest>(&request.body) {
                Ok(request) => request,
                Err(e) => return error_response(400, &e),
            };
            let result = app.freeze.freeze(request.barrier_id, request.ttl_ms);
            match result {
                Ok(response) => {
                    match app.append_state_log("freeze.created", &response.barrier_id, &response) {
                        Ok(()) => json_response(200, &response),
                        Err(e) => error_response(500, &e),
                    }
                }
                Err(e) => error_response(400, &e),
            }
        }
        ("POST", "/v1/streams/receive") => {
            let request = match parse_json::<api::StartReceiveRequest>(&request.body) {
                Ok(request) => request,
                Err(e) => return error_response(400, &e),
            };
            result_response(app.start_receive(request))
        }
        ("POST", "/v1/streams/send") => {
            let request = match parse_json::<api::StartSendRequest>(&request.body) {
                Ok(request) => request,
                Err(e) => return error_response(400, &e),
            };
            result_response(app.start_send(request))
        }
        ("GET", "/v1/streams") => json_response(200, &app.replication_status(None)),
        ("DELETE", path) if path.starts_with("/v1/snapshots/") => {
            let snapshot_id = percent_decode(&path["/v1/snapshots/".len()..]);
            result_response(app.delete_snapshot(&snapshot_id))
        }
        ("POST", path)
            if path.starts_with("/v1/snapshot-devices/") && path.ends_with("/compact") =>
        {
            let tail = &path["/v1/snapshot-devices/".len()..path.len() - "/compact".len()];
            let device_id = percent_decode(tail.trim_end_matches('/'));
            let request =
                match parse_json_or_default::<api::StartSnapshotCompactionRequest>(&request.body) {
                    Ok(request) => request,
                    Err(e) => return error_response(400, &e),
                };
            result_response(app.start_snapshot_compaction(&device_id, request))
        }
        ("GET", path) if path.starts_with("/v1/snapshot-devices/") => {
            let device_id = percent_decode(&path["/v1/snapshot-devices/".len()..]);
            json_response(200, &app.snapshot_device_status(Some(&device_id)))
        }
        ("DELETE", path) if path.starts_with("/v1/snapshot-devices/") => {
            let device_id = percent_decode(&path["/v1/snapshot-devices/".len()..]);
            result_response(app.delete_snapshot_device(&device_id))
        }
        ("GET", path) if path.starts_with("/v1/compactions/") => {
            let job_id = percent_decode(&path["/v1/compactions/".len()..]);
            json_response(200, &app.compaction_status(Some(&job_id)))
        }
        ("PUT", path) | ("PATCH", path) if path.starts_with("/v1/compactions/") => {
            let job_id = percent_decode(&path["/v1/compactions/".len()..]);
            let request = match parse_json::<api::UpdateSnapshotCompactionRequest>(&request.body) {
                Ok(request) => request,
                Err(e) => return error_response(400, &e),
            };
            result_response(app.update_snapshot_compaction(&job_id, request))
        }
        ("DELETE", path) if path.starts_with("/v1/freeze/") => {
            let barrier_id = percent_decode(&path["/v1/freeze/".len()..]);
            let result = app.freeze.release(&barrier_id);
            match result {
                Ok(response) => {
                    match app.append_state_log("freeze.released", &response.barrier_id, &response) {
                        Ok(()) => json_response(200, &response),
                        Err(e) => error_response(500, &e),
                    }
                }
                Err(e) => error_response(400, &e),
            }
        }
        ("GET", path) if path.starts_with("/v1/streams/") => {
            let repl_id = percent_decode(&path["/v1/streams/".len()..]);
            json_response(200, &app.replication_status(Some(&repl_id)))
        }
        _ => error_response(404, "not found"),
    }
}

fn read_http_request(stream: &mut TcpStream) -> io::Result<HttpRequest> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    if request_line.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty request line",
        ));
    }
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing method"))?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing path"))?
        .to_string();
    let mut content_length = 0_usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid content-length")
                })?;
            }
        }
    }
    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(HttpRequest { method, path, body })
}

fn write_http_response(stream: &mut TcpStream, response: HttpResponse) -> io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "OK",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    )?;
    stream.write_all(&response.body)?;
    stream.flush()
}

fn json_response<T: serde::Serialize>(status: u16, value: &T) -> HttpResponse {
    match serde_json::to_vec(value) {
        Ok(body) => HttpResponse {
            status,
            content_type: "application/json",
            body,
        },
        Err(e) => error_response(500, &format!("encode response: {e}")),
    }
}

fn result_response<T: serde::Serialize>(result: Result<T, String>) -> HttpResponse {
    match result {
        Ok(value) => json_response(200, &value),
        Err(e) => error_response(400, &e),
    }
}

fn error_response(status: u16, message: &str) -> HttpResponse {
    json_response(
        status,
        &api::ErrorResponse {
            error: message.to_string(),
        },
    )
}

fn parse_json<T: for<'de> serde::Deserialize<'de>>(body: &[u8]) -> Result<T, String> {
    serde_json::from_slice(body).map_err(|e| format!("decode request: {e}"))
}

fn parse_json_or_default<T>(body: &[u8]) -> Result<T, String>
where
    T: Default + for<'de> serde::Deserialize<'de>,
{
    if body.is_empty() {
        Ok(T::default())
    } else {
        parse_json(body)
    }
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

fn normalize_snapshot_device_mode(value: &str) -> Result<String, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "cow" | "copy-on-write" | "copy_on_write" => Ok("cow".to_string()),
        "wal" | "journal" | "log" => Ok("wal".to_string()),
        other => Err(format!(
            "unsupported snapshot device mode {other:?}; expected cow or wal"
        )),
    }
}

fn normalize_compaction_strategy(value: Option<&str>) -> Result<String, String> {
    match value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("stream-rewrite")
        .to_ascii_lowercase()
        .as_str()
    {
        "stream-rewrite" | "stream_rewrite" | "stream" | "off-machine" | "off_machine" => {
            Ok("stream-rewrite".to_string())
        }
        "in-place" | "in_place" | "local" => Ok("in-place".to_string()),
        other => Err(format!(
            "unsupported compaction strategy {other:?}; expected stream-rewrite or in-place"
        )),
    }
}

fn initial_compaction_phase(strategy: &str) -> &'static str {
    if strategy == "in-place" {
        "in-place"
    } else {
        "stream-out"
    }
}

fn normalize_compaction_phase(value: &str) -> Result<String, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "queued" => Ok("queued".to_string()),
        "stream-out" | "stream_out" | "out" | "outbound" => Ok("stream-out".to_string()),
        "stream-in" | "stream_in" | "in" | "inbound" => Ok("stream-in".to_string()),
        "in-place" | "in_place" | "local" => Ok("in-place".to_string()),
        "registered" | "checkpoint" => Ok("registered".to_string()),
        "done" | "complete" | "completed" | "succeeded" => Ok("done".to_string()),
        other => Err(format!(
            "unsupported compaction phase {other:?}; expected stream-out, stream-in, in-place, registered, or done"
        )),
    }
}

fn normalize_compaction_state(value: &str) -> Result<String, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "queued" => Ok("queued".to_string()),
        "running" | "active" => Ok("running".to_string()),
        "succeeded" | "success" | "done" | "complete" | "completed" => Ok("succeeded".to_string()),
        "failed" | "error" => Ok("failed".to_string()),
        other => Err(format!(
            "unsupported compaction state {other:?}; expected queued, running, succeeded, or failed"
        )),
    }
}

fn normalize_replication_mode(value: &str) -> Result<String, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "async" | "asynchronous" => Ok("async".to_string()),
        "sync" | "synchronous" => Ok("sync".to_string()),
        other => Err(format!(
            "unsupported replication mode {other:?}; expected async or sync"
        )),
    }
}

fn normalize_replication_scope(value: Option<&str>) -> Result<String, String> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => normalize_control_value(value, "scope"),
        None => Ok(api::GLOBAL_REPLICATION_SCOPE.to_string()),
    }
}

fn normalize_control_value(value: &str, label: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{label} is required"));
    }
    if value.len() > 512 || value.chars().any(|ch| ch.is_whitespace() || ch == '\0') {
        return Err(format!(
            "{label} must be at most 512 bytes and contain no whitespace or NUL"
        ));
    }
    Ok(value.to_string())
}

fn normalize_optional_control_value(
    value: Option<String>,
    label: &str,
) -> Result<Option<String>, String> {
    value
        .map(|value| {
            if value.trim().is_empty() {
                Ok(None)
            } else {
                normalize_control_value(&value, label).map(Some)
            }
        })
        .transpose()
        .map(Option::flatten)
}

fn apply_optional_control_value(
    target: &mut Option<String>,
    value: Option<String>,
    label: &str,
) -> Result<(), String> {
    if value.is_some() {
        *target = normalize_optional_control_value(value, label)?;
    }
    Ok(())
}

fn normalize_optional_checkpoint(value: Option<String>) -> Result<Option<String>, String> {
    value
        .map(|value| {
            let value = value.trim();
            if value.is_empty() {
                Ok(None)
            } else if value.len() > 4096 || value.chars().any(|ch| ch == '\0') {
                Err("checkpoint must be at most 4096 bytes and contain no NUL".to_string())
            } else {
                Ok(Some(value.to_string()))
            }
        })
        .transpose()
        .map(Option::flatten)
}

fn apply_optional_checkpoint(
    target: &mut Option<String>,
    value: Option<String>,
) -> Result<(), String> {
    if value.is_some() {
        *target = normalize_optional_checkpoint(value)?;
    }
    Ok(())
}

fn default_replication_mode() -> String {
    "async".to_string()
}

fn default_replication_mode_policy() -> api::ReplicationModeSpec {
    api::ReplicationModeSpec {
        scope: api::GLOBAL_REPLICATION_SCOPE.to_string(),
        mode: default_replication_mode(),
        generation: 0,
        updated_at_secs: 0,
    }
}

fn is_terminal_replication_state(state: &str) -> bool {
    matches!(state, "succeeded" | "failed")
}

fn job_matches_scope(job: &api::ReplicationJob, scope: &str) -> bool {
    scope == api::GLOBAL_REPLICATION_SCOPE
        || job.subject == scope
        || format!("volume:{}", job.subject) == scope
}

fn delay_sample(mut job: api::ReplicationJob, now_millis: u64) -> api::ReplicationDelaySample {
    if job.started_at_millis == 0 {
        job.started_at_millis = job.started_at_secs.saturating_mul(1000);
    }
    if job.updated_at_millis == 0 {
        job.updated_at_millis = job.finished_at_millis.unwrap_or(job.started_at_millis);
    }
    let end_millis = job.finished_at_millis.unwrap_or(now_millis);
    let elapsed_millis = end_millis.saturating_sub(job.started_at_millis);
    let idle_millis = now_millis.saturating_sub(job.updated_at_millis);
    let bytes_remaining = job.bytes_limit.map(|limit| limit.saturating_sub(job.bytes));
    let (stream_kind, volume_id, snapshot_id) = replication_subject_labels(&job.role, &job.subject);
    api::ReplicationDelaySample {
        repl_id: job.repl_id,
        role: job.role,
        state: job.state,
        subject: job.subject,
        stream_kind,
        volume_id,
        snapshot_id,
        peer: job.peer,
        port: job.port,
        replication_mode: job.replication_mode,
        target_cluster: job.target_cluster,
        gateway_endpoint: job.gateway_endpoint,
        spillover_tier: job.spillover_tier,
        bytes: job.bytes,
        bytes_limit: job.bytes_limit,
        bytes_remaining,
        started_at_millis: job.started_at_millis,
        updated_at_millis: job.updated_at_millis,
        finished_at_millis: job.finished_at_millis,
        elapsed_millis,
        idle_millis,
    }
}

fn build_stats_response(
    samples: Vec<api::ReplicationDelaySample>,
    compactions: Vec<api::SnapshotCompactionJob>,
    generated_at_millis: u64,
) -> api::StatsResponse {
    let mut summary = stats_totals();
    let mut placement = stats_node("root", "placement");
    let mut logical = stats_node("root", "logical");

    for sample in &samples {
        add_sample_to_totals(&mut summary, sample);

        let target_cluster = sample
            .target_cluster
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("unplaced");
        let spillover_tier = sample
            .spillover_tier
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("default");
        let gateway_endpoint = sample
            .gateway_endpoint
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("direct");
        let logical_name = logical_replication_name(sample);

        add_sample_to_tree(
            &mut placement,
            &[
                ("target_cluster", target_cluster),
                ("spillover_tier", spillover_tier),
                ("gateway_endpoint", gateway_endpoint),
                (&sample.stream_kind, logical_name.as_str()),
                ("replica_job", &sample.repl_id),
            ],
            sample,
        );
        add_sample_to_tree(
            &mut logical,
            &[
                (&sample.stream_kind, logical_name.as_str()),
                ("target_cluster", target_cluster),
                ("spillover_tier", spillover_tier),
                ("gateway_endpoint", gateway_endpoint),
                ("replica_job", &sample.repl_id),
            ],
            sample,
        );
    }

    api::StatsResponse {
        generated_at_millis,
        summary,
        hierarchy: api::StatsHierarchy { placement, logical },
        compactions: api::SnapshotCompactionStatusResponse { jobs: compactions },
    }
}

fn stats_node(kind: &str, name: &str) -> api::StatsNode {
    api::StatsNode {
        kind: kind.to_string(),
        name: name.to_string(),
        totals: stats_totals(),
        children: Vec::new(),
        samples: Vec::new(),
    }
}

fn stats_totals() -> api::StatsTotals {
    api::StatsTotals {
        attention_idle_threshold_millis: DEFAULT_REPLICATION_ATTENTION_IDLE_MS,
        ..api::StatsTotals::default()
    }
}

fn add_sample_to_tree(
    node: &mut api::StatsNode,
    path: &[(&str, &str)],
    sample: &api::ReplicationDelaySample,
) {
    add_sample_to_totals(&mut node.totals, sample);
    let Some(((kind, name), rest)) = path.split_first() else {
        node.samples.push(sample.clone());
        return;
    };
    let child_index = node
        .children
        .iter()
        .position(|child| child.kind == *kind && child.name == *name)
        .unwrap_or_else(|| {
            node.children.push(stats_node(kind, name));
            node.children.len() - 1
        });
    add_sample_to_tree(&mut node.children[child_index], rest, sample);
}

fn add_sample_to_totals(totals: &mut api::StatsTotals, sample: &api::ReplicationDelaySample) {
    totals.attention_idle_threshold_millis = DEFAULT_REPLICATION_ATTENTION_IDLE_MS;
    totals.jobs_total = totals.jobs_total.saturating_add(1);
    match sample.state.as_str() {
        "succeeded" => totals.jobs_succeeded = totals.jobs_succeeded.saturating_add(1),
        "failed" => {
            totals.jobs_failed = totals.jobs_failed.saturating_add(1);
            totals.attention_failed_jobs = totals.attention_failed_jobs.saturating_add(1);
        }
        _ => {
            totals.jobs_active = totals.jobs_active.saturating_add(1);
            if sample.idle_millis >= DEFAULT_REPLICATION_ATTENTION_IDLE_MS {
                totals.attention_idle_jobs = totals.attention_idle_jobs.saturating_add(1);
            }
        }
    }
    totals.bytes = totals.bytes.saturating_add(sample.bytes);
    if let Some(bytes_limit) = sample.bytes_limit {
        totals.bytes_limit_known = totals.bytes_limit_known.saturating_add(1);
        totals.bytes_limit = totals.bytes_limit.saturating_add(bytes_limit);
    }
    if let Some(bytes_remaining) = sample.bytes_remaining {
        totals.bytes_remaining = totals.bytes_remaining.saturating_add(bytes_remaining);
    }
    totals.max_elapsed_millis = totals.max_elapsed_millis.max(sample.elapsed_millis);
    totals.max_idle_millis = totals.max_idle_millis.max(sample.idle_millis);
    totals.attention_required = totals.attention_failed_jobs > 0 || totals.attention_idle_jobs > 0;
}

fn logical_replication_name(sample: &api::ReplicationDelaySample) -> String {
    sample
        .volume_id
        .as_ref()
        .map(|volume_id| format!("volume:{volume_id}"))
        .or_else(|| {
            sample
                .snapshot_id
                .as_ref()
                .map(|snapshot_id| format!("snapshot:{snapshot_id}"))
        })
        .unwrap_or_else(|| sample.subject.clone())
}

fn render_prometheus_metrics(
    samples: &[api::ReplicationDelaySample],
    compactions: &[api::SnapshotCompactionJob],
) -> String {
    let mut out = String::new();
    let mut summary = stats_totals();
    for sample in samples {
        add_sample_to_totals(&mut summary, sample);
    }
    metric_help_type(
        &mut out,
        "zccusan_replication_attention_required",
        "Master zccusan replication attention signal; 1 means inspect /v1/stats.",
    );
    metric_line_unlabeled(
        &mut out,
        "zccusan_replication_attention_required",
        if summary.attention_required { "1" } else { "0" },
    );
    metric_help_type(
        &mut out,
        "zccusan_replication_attention_failed_jobs",
        "Replication jobs that failed and require operator attention.",
    );
    metric_line_unlabeled(
        &mut out,
        "zccusan_replication_attention_failed_jobs",
        &summary.attention_failed_jobs.to_string(),
    );
    metric_help_type(
        &mut out,
        "zccusan_replication_attention_idle_jobs",
        "Active replication jobs idle long enough to require operator attention.",
    );
    metric_line_unlabeled(
        &mut out,
        "zccusan_replication_attention_idle_jobs",
        &summary.attention_idle_jobs.to_string(),
    );
    metric_help_type(
        &mut out,
        "zccusan_replication_attention_idle_threshold_seconds",
        "Idle duration threshold used by the master zccusan replication attention signal.",
    );
    metric_line_unlabeled(
        &mut out,
        "zccusan_replication_attention_idle_threshold_seconds",
        &millis_as_seconds(summary.attention_idle_threshold_millis),
    );
    metric_help_type(
        &mut out,
        "zccusan_replication_job_info",
        "Replication job label set.",
    );
    metric_help_type(
        &mut out,
        "zccusan_replication_job_bytes",
        "Bytes transferred by a replication job.",
    );
    metric_help_type(
        &mut out,
        "zccusan_replication_job_bytes_limit",
        "Configured byte limit for a replication job.",
    );
    metric_help_type(
        &mut out,
        "zccusan_replication_job_bytes_remaining",
        "Bytes remaining before the replication job reaches its configured byte limit.",
    );
    metric_help_type(
        &mut out,
        "zccusan_replication_job_elapsed_seconds",
        "Elapsed wall time for a replication job.",
    );
    metric_help_type(
        &mut out,
        "zccusan_replication_job_idle_seconds",
        "Seconds since the last observed byte progress for a replication job.",
    );
    metric_help_type(
        &mut out,
        "zccusan_replication_job_started_timestamp_seconds",
        "Unix timestamp when a replication job started.",
    );
    metric_help_type(
        &mut out,
        "zccusan_replication_job_updated_timestamp_seconds",
        "Unix timestamp when a replication job last changed state or byte progress.",
    );
    metric_help_type(
        &mut out,
        "zccusan_replication_job_finished_timestamp_seconds",
        "Unix timestamp when a replication job finished.",
    );
    metric_help_type(
        &mut out,
        "zccusan_snapshot_compaction_job_info",
        "Snapshot COW/WAL compaction job label set.",
    );
    metric_help_type(
        &mut out,
        "zccusan_snapshot_compaction_job_bytes",
        "Bytes compacted by a snapshot COW/WAL compaction job.",
    );
    metric_help_type(
        &mut out,
        "zccusan_snapshot_compaction_job_bytes_streamed_out",
        "Bytes streamed off-machine by a snapshot COW/WAL compaction job.",
    );
    metric_help_type(
        &mut out,
        "zccusan_snapshot_compaction_job_bytes_streamed_in",
        "Bytes streamed back into the compacted location by a snapshot COW/WAL compaction job.",
    );
    metric_help_type(
        &mut out,
        "zccusan_snapshot_compaction_job_bytes_total",
        "Total bytes expected for a snapshot COW/WAL compaction job.",
    );
    metric_help_type(
        &mut out,
        "zccusan_snapshot_compaction_job_bytes_remaining",
        "Bytes remaining for a snapshot COW/WAL compaction job.",
    );
    metric_help_type(
        &mut out,
        "zccusan_snapshot_compaction_job_started_timestamp_seconds",
        "Unix timestamp when a snapshot COW/WAL compaction job started.",
    );
    metric_help_type(
        &mut out,
        "zccusan_snapshot_compaction_job_updated_timestamp_seconds",
        "Unix timestamp when a snapshot COW/WAL compaction job last changed.",
    );
    metric_help_type(
        &mut out,
        "zccusan_snapshot_compaction_job_finished_timestamp_seconds",
        "Unix timestamp when a snapshot COW/WAL compaction job finished.",
    );

    for sample in samples {
        let labels = replication_metric_labels(sample);
        metric_line(&mut out, "zccusan_replication_job_info", &labels, "1");
        metric_line(
            &mut out,
            "zccusan_replication_job_bytes",
            &labels,
            &sample.bytes.to_string(),
        );
        if let Some(bytes_limit) = sample.bytes_limit {
            metric_line(
                &mut out,
                "zccusan_replication_job_bytes_limit",
                &labels,
                &bytes_limit.to_string(),
            );
        }
        if let Some(bytes_remaining) = sample.bytes_remaining {
            metric_line(
                &mut out,
                "zccusan_replication_job_bytes_remaining",
                &labels,
                &bytes_remaining.to_string(),
            );
        }
        metric_line(
            &mut out,
            "zccusan_replication_job_elapsed_seconds",
            &labels,
            &millis_as_seconds(sample.elapsed_millis),
        );
        metric_line(
            &mut out,
            "zccusan_replication_job_idle_seconds",
            &labels,
            &millis_as_seconds(sample.idle_millis),
        );
        metric_line(
            &mut out,
            "zccusan_replication_job_started_timestamp_seconds",
            &labels,
            &millis_as_seconds(sample.started_at_millis),
        );
        metric_line(
            &mut out,
            "zccusan_replication_job_updated_timestamp_seconds",
            &labels,
            &millis_as_seconds(sample.updated_at_millis),
        );
        if let Some(finished_at_millis) = sample.finished_at_millis {
            metric_line(
                &mut out,
                "zccusan_replication_job_finished_timestamp_seconds",
                &labels,
                &millis_as_seconds(finished_at_millis),
            );
        }
    }
    for job in compactions {
        let labels = compaction_metric_labels(job);
        metric_line(
            &mut out,
            "zccusan_snapshot_compaction_job_info",
            &labels,
            "1",
        );
        metric_line(
            &mut out,
            "zccusan_snapshot_compaction_job_bytes",
            &labels,
            &job.bytes_compacted.to_string(),
        );
        metric_line(
            &mut out,
            "zccusan_snapshot_compaction_job_bytes_streamed_out",
            &labels,
            &job.bytes_streamed_out.to_string(),
        );
        metric_line(
            &mut out,
            "zccusan_snapshot_compaction_job_bytes_streamed_in",
            &labels,
            &job.bytes_streamed_in.to_string(),
        );
        if let Some(bytes_total) = job.bytes_total {
            metric_line(
                &mut out,
                "zccusan_snapshot_compaction_job_bytes_total",
                &labels,
                &bytes_total.to_string(),
            );
            let remaining = bytes_total.saturating_sub(job.bytes_compacted);
            metric_line(
                &mut out,
                "zccusan_snapshot_compaction_job_bytes_remaining",
                &labels,
                &remaining.to_string(),
            );
        }
        metric_line(
            &mut out,
            "zccusan_snapshot_compaction_job_started_timestamp_seconds",
            &labels,
            &millis_as_seconds(job.started_at_millis),
        );
        metric_line(
            &mut out,
            "zccusan_snapshot_compaction_job_updated_timestamp_seconds",
            &labels,
            &millis_as_seconds(job.updated_at_millis),
        );
        if let Some(finished_at_millis) = job.finished_at_millis {
            metric_line(
                &mut out,
                "zccusan_snapshot_compaction_job_finished_timestamp_seconds",
                &labels,
                &millis_as_seconds(finished_at_millis),
            );
        }
    }
    out
}

fn metric_help_type(out: &mut String, name: &str, help: &str) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
}

fn metric_line(out: &mut String, name: &str, labels: &str, value: &str) {
    out.push_str(name);
    out.push('{');
    out.push_str(labels);
    out.push_str("} ");
    out.push_str(value);
    out.push('\n');
}

fn metric_line_unlabeled(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push(' ');
    out.push_str(value);
    out.push('\n');
}

fn replication_metric_labels(sample: &api::ReplicationDelaySample) -> String {
    let port = sample.port.map(|port| port.to_string()).unwrap_or_default();
    format!(
        "repl_id=\"{}\",role=\"{}\",state=\"{}\",subject=\"{}\",stream_kind=\"{}\",volume_id=\"{}\",snapshot_id=\"{}\",peer=\"{}\",port=\"{}\",replication_mode=\"{}\",target_cluster=\"{}\",gateway_endpoint=\"{}\",spillover_tier=\"{}\"",
        prometheus_label_value(&sample.repl_id),
        prometheus_label_value(&sample.role),
        prometheus_label_value(&sample.state),
        prometheus_label_value(&sample.subject),
        prometheus_label_value(&sample.stream_kind),
        prometheus_label_value(sample.volume_id.as_deref().unwrap_or("")),
        prometheus_label_value(sample.snapshot_id.as_deref().unwrap_or("")),
        prometheus_label_value(&sample.peer),
        prometheus_label_value(&port),
        prometheus_label_value(&sample.replication_mode),
        prometheus_label_value(sample.target_cluster.as_deref().unwrap_or("")),
        prometheus_label_value(sample.gateway_endpoint.as_deref().unwrap_or("")),
        prometheus_label_value(sample.spillover_tier.as_deref().unwrap_or(""))
    )
}

fn compaction_metric_labels(job: &api::SnapshotCompactionJob) -> String {
    format!(
        "job_id=\"{}\",device_id=\"{}\",snapshot_id=\"{}\",mode=\"{}\",strategy=\"{}\",phase=\"{}\",state=\"{}\"",
        prometheus_label_value(&job.job_id),
        prometheus_label_value(&job.device_id),
        prometheus_label_value(&job.snapshot_id),
        prometheus_label_value(&job.mode),
        prometheus_label_value(&job.strategy),
        prometheus_label_value(&job.phase),
        prometheus_label_value(&job.state)
    )
}

fn replication_subject_labels(
    role: &str,
    subject: &str,
) -> (String, Option<String>, Option<String>) {
    if let Some(volume_id) = subject.strip_prefix("volume:") {
        return ("volume".to_string(), Some(volume_id.to_string()), None);
    }
    if let Some(snapshot_id) = subject.strip_prefix("snapshot:") {
        return ("snapshot".to_string(), None, Some(snapshot_id.to_string()));
    }
    if role == "receive" && !subject.is_empty() {
        return ("volume".to_string(), Some(subject.to_string()), None);
    }
    ("unknown".to_string(), None, None)
}

fn prometheus_label_value(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out
}

fn millis_as_seconds(millis: u64) -> String {
    format!("{}.{:03}", millis / 1000, millis % 1000)
}

fn snapshot_to_state(spec: &api::SnapshotSpec) -> String {
    format!(
        "snapshot_id={}\nname_hex={}\nsource_volume_id={}\nsource_backend={}\nsize_bytes={}\nsnapshot_path={}\nsnapshot_mode={}\ncreation_time_secs={}\nready_to_use={}\n",
        spec.snapshot_id,
        spec.name_hex,
        spec.source_volume_id,
        spec.source_backend,
        spec.size_bytes,
        spec.snapshot_path,
        spec.snapshot_mode,
        spec.creation_time_secs,
        spec.ready_to_use
    )
}

fn snapshot_from_state(body: &str) -> Result<api::SnapshotSpec, String> {
    let map = parse_key_values(body);
    Ok(api::SnapshotSpec {
        snapshot_id: required(&map, "snapshot_id")?.to_string(),
        name_hex: required(&map, "name_hex")?.to_string(),
        source_volume_id: required(&map, "source_volume_id")?.to_string(),
        source_backend: required(&map, "source_backend")?.to_string(),
        size_bytes: parse_i64(required(&map, "size_bytes")?, "size_bytes")?,
        snapshot_path: required(&map, "snapshot_path")?.to_string(),
        snapshot_mode: nonempty_value(&map, "snapshot_mode").unwrap_or_else(|| "copy".to_string()),
        creation_time_secs: parse_i64(required(&map, "creation_time_secs")?, "creation_time_secs")?,
        ready_to_use: parse_bool(required(&map, "ready_to_use")?, "ready_to_use")?,
    })
}

fn snapshot_device_to_state(spec: &api::SnapshotDeviceSpec) -> String {
    format!(
        "device_id={}\nsnapshot_id={}\nsource_volume_id={}\nmode={}\ndevice_name={}\ndevice_path={}\nconfigfs_path={}\nsize_bytes={}\nreadonly={}\nstate={}\ncompaction_job_id={}\ncreated_at_millis={}\nupdated_at_millis={}\n",
        spec.device_id,
        spec.snapshot_id,
        spec.source_volume_id,
        spec.mode,
        spec.device_name,
        spec.device_path,
        spec.configfs_path,
        spec.size_bytes,
        spec.readonly,
        spec.state,
        spec.compaction_job_id.clone().unwrap_or_default(),
        spec.created_at_millis,
        spec.updated_at_millis
    )
}

fn snapshot_device_from_state(body: &str) -> Result<api::SnapshotDeviceSpec, String> {
    let map = parse_key_values(body);
    Ok(api::SnapshotDeviceSpec {
        device_id: required(&map, "device_id")?.to_string(),
        snapshot_id: required(&map, "snapshot_id")?.to_string(),
        source_volume_id: required(&map, "source_volume_id")?.to_string(),
        mode: normalize_snapshot_device_mode(required(&map, "mode")?)?,
        device_name: required(&map, "device_name")?.to_string(),
        device_path: required(&map, "device_path")?.to_string(),
        configfs_path: required(&map, "configfs_path")?.to_string(),
        size_bytes: parse_i64(required(&map, "size_bytes")?, "size_bytes")?,
        readonly: parse_bool(required(&map, "readonly")?, "readonly")?,
        state: required(&map, "state")?.to_string(),
        compaction_job_id: nonempty_value(&map, "compaction_job_id"),
        created_at_millis: parse_u64(required(&map, "created_at_millis")?, "created_at_millis")?,
        updated_at_millis: parse_u64(required(&map, "updated_at_millis")?, "updated_at_millis")?,
    })
}

fn snapshot_compaction_to_state(job: &api::SnapshotCompactionJob) -> String {
    format!(
        "job_id={}\ndevice_id={}\nsnapshot_id={}\nmode={}\nstrategy={}\nphase={}\nstate={}\nbytes_compacted={}\nbytes_streamed_out={}\nbytes_streamed_in={}\nbytes_total={}\noutbound_stream_id={}\ninbound_stream_id={}\ntarget_location={}\nworker_id={}\ncheckpoint={}\nstarted_at_millis={}\nupdated_at_millis={}\nfinished_at_millis={}\nerror={}\n",
        job.job_id,
        job.device_id,
        job.snapshot_id,
        job.mode,
        job.strategy,
        job.phase,
        job.state,
        job.bytes_compacted,
        job.bytes_streamed_out,
        job.bytes_streamed_in,
        job.bytes_total
            .map(|value| value.to_string())
            .unwrap_or_default(),
        job.outbound_stream_id.clone().unwrap_or_default(),
        job.inbound_stream_id.clone().unwrap_or_default(),
        job.target_location.clone().unwrap_or_default(),
        job.worker_id.clone().unwrap_or_default(),
        job.checkpoint.clone().unwrap_or_default(),
        job.started_at_millis,
        job.updated_at_millis,
        job.finished_at_millis
            .map(|value| value.to_string())
            .unwrap_or_default(),
        job.error.clone().unwrap_or_default()
    )
}

fn snapshot_compaction_from_state(body: &str) -> Result<api::SnapshotCompactionJob, String> {
    let map = parse_key_values(body);
    Ok(api::SnapshotCompactionJob {
        job_id: required(&map, "job_id")?.to_string(),
        device_id: required(&map, "device_id")?.to_string(),
        snapshot_id: required(&map, "snapshot_id")?.to_string(),
        mode: normalize_snapshot_device_mode(required(&map, "mode")?)?,
        strategy: normalize_compaction_strategy(nonempty_value(&map, "strategy").as_deref())?,
        phase: normalize_compaction_phase(nonempty_value(&map, "phase").as_deref().unwrap_or(
            initial_compaction_phase(&normalize_compaction_strategy(
                nonempty_value(&map, "strategy").as_deref(),
            )?),
        ))?,
        state: required(&map, "state")?.to_string(),
        bytes_compacted: parse_u64(required(&map, "bytes_compacted")?, "bytes_compacted")?,
        bytes_streamed_out: nonempty_value(&map, "bytes_streamed_out")
            .map(|value| parse_u64(&value, "bytes_streamed_out"))
            .transpose()?
            .unwrap_or(0),
        bytes_streamed_in: nonempty_value(&map, "bytes_streamed_in")
            .map(|value| parse_u64(&value, "bytes_streamed_in"))
            .transpose()?
            .unwrap_or(0),
        bytes_total: nonempty_value(&map, "bytes_total")
            .map(|value| parse_u64(&value, "bytes_total"))
            .transpose()?,
        outbound_stream_id: nonempty_value(&map, "outbound_stream_id"),
        inbound_stream_id: nonempty_value(&map, "inbound_stream_id"),
        target_location: nonempty_value(&map, "target_location"),
        worker_id: nonempty_value(&map, "worker_id"),
        checkpoint: nonempty_value(&map, "checkpoint"),
        started_at_millis: parse_u64(required(&map, "started_at_millis")?, "started_at_millis")?,
        updated_at_millis: parse_u64(required(&map, "updated_at_millis")?, "updated_at_millis")?,
        finished_at_millis: nonempty_value(&map, "finished_at_millis")
            .map(|value| parse_u64(&value, "finished_at_millis"))
            .transpose()?,
        error: nonempty_value(&map, "error"),
    })
}

fn copy_exact_bytes_to_file(source: &Path, dest: &Path, bytes: u64) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create image parent: {e}"))?;
    }
    let mut src = OpenOptions::new()
        .read(true)
        .open(source)
        .map_err(|e| format!("open copy source: {e}"))?;
    let mut dst = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(dest)
        .map_err(|e| format!("open copy destination: {e}"))?;
    copy_exact_bytes(&mut src, &mut dst, bytes).map_err(|e| format!("copy bytes: {e}"))?;
    dst.set_len(bytes)
        .map_err(|e| format!("size copied image: {e}"))?;
    dst.sync_all()
        .map_err(|e| format!("sync copied image: {e}"))
}

fn restore_image_to_file(image: &Path, dest: &Path, capacity_bytes: u64) -> Result<(), String> {
    let image_bytes = file_size(image)?;
    if image_bytes > capacity_bytes {
        return Err(format!(
            "snapshot image size {image_bytes} exceeds destination capacity {capacity_bytes}"
        ));
    }
    copy_exact_bytes_to_file(image, dest, image_bytes)?;
    let file = OpenOptions::new()
        .write(true)
        .open(dest)
        .map_err(|e| format!("open restored file image: {e}"))?;
    file.set_len(capacity_bytes)
        .map_err(|e| format!("size restored file image: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("sync restored file image: {e}"))
}

fn restore_image_to_device(image: &Path, dest: &Path, capacity_bytes: u64) -> Result<(), String> {
    let image_bytes = file_size(image)?;
    if image_bytes > capacity_bytes {
        return Err(format!(
            "snapshot image size {image_bytes} exceeds destination capacity {capacity_bytes}"
        ));
    }
    let mut src = OpenOptions::new()
        .read(true)
        .open(image)
        .map_err(|e| format!("open snapshot image: {e}"))?;
    let mut dst = OpenOptions::new()
        .write(true)
        .open(dest)
        .map_err(|e| format!("open restore destination: {e}"))?;
    copy_exact_bytes(&mut src, &mut dst, image_bytes)
        .map_err(|e| format!("restore snapshot image: {e}"))?;
    dst.sync_all()
        .map_err(|e| format!("sync restored block device: {e}"))
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

fn file_size(path: &Path) -> Result<u64, String> {
    fs::metadata(path)
        .map(|meta| meta.len())
        .map_err(|e| format!("stat snapshot image: {e}"))
}

fn canonical_block_device(path: &Path) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(path).map_err(|e| format!("resolve block device: {e}"))?;
    let meta = fs::metadata(&canonical).map_err(|e| format!("stat block device: {e}"))?;
    if !meta.file_type().is_block_device() {
        return Err(format!("{} is not a block device", canonical.display()));
    }
    Ok(canonical)
}

fn block_device_size(path: &Path) -> Result<u64, String> {
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|e| format!("open block device: {e}"))?;
    let mut bytes: u64 = 0;
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), BLKGETSIZE64, &mut bytes) };
    if ret != 0 {
        return Err(format!(
            "get block device size: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(bytes)
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

fn read_raw_allowlist(path: &Path) -> Result<Vec<String>, String> {
    let body = fs::read_to_string(path)
        .map_err(|e| format!("cannot read raw block allowlist {}: {e}", path.display()))?;
    let mut allowed = Vec::new();
    for (idx, line) in body.lines().enumerate() {
        let token = line.split('#').next().unwrap_or("").trim();
        if token.is_empty() {
            continue;
        }
        allowed.push(normalize_partuuid(token).map_err(|e| {
            format!(
                "invalid PARTUUID in {} line {}: {e}",
                path.display(),
                idx + 1
            )
        })?);
    }
    if allowed.is_empty() {
        return Err(format!("raw block allowlist {} is empty", path.display()));
    }
    Ok(allowed)
}

fn normalize_partuuid(value: &str) -> Result<String, String> {
    let value = value
        .trim()
        .strip_prefix("PARTUUID=")
        .or_else(|| value.trim().strip_prefix("partuuid="))
        .or_else(|| value.trim().strip_prefix("partuuid:"))
        .unwrap_or(value.trim())
        .trim()
        .to_ascii_lowercase();
    if value.is_empty() {
        return Err("PARTUUID must not be empty".to_string());
    }
    if !value.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err(format!("invalid PARTUUID syntax: {value}"));
    }
    Ok(value)
}

fn write_configfs_attr<T: ToString>(dir: &Path, attr: &str, value: T) -> Result<(), String> {
    fs::write(dir.join(attr), format!("{}\n", value.to_string()))
        .map_err(|e| format!("write {} for {} failed: {e}", attr, dir.display()))
}

fn fsfreeze_path(path: &Path, freeze: bool, timeout_duration: Duration) -> Result<(), String> {
    let mode = if freeze { "--freeze" } else { "--unfreeze" };
    let mut child = Command::new("fsfreeze")
        .arg(mode)
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("run fsfreeze {mode}: {e}"))?;
    let deadline = Instant::now() + timeout_duration;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child_output_status(child, &format!("fsfreeze {mode}")),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "fsfreeze {mode} timed out after {}ms",
                    timeout_duration.as_millis()
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(format!("wait fsfreeze {mode}: {e}")),
        }
    }
}

fn child_output_status(child: Child, action: &str) -> Result<(), String> {
    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait {action}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Err(format!(
            "{action} exited {}: {}{}",
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

fn is_mountpoint(path: &Path) -> bool {
    Command::new("findmnt")
        .args(["-rn", "--mountpoint"])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn current_unix_time_secs() -> Result<i64, String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch".to_string())?;
    i64::try_from(duration.as_secs()).map_err(|_| "timestamp is too large".to_string())
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn unix_now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn new_repl_id(kind: &str, subject: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("zcrepl-{kind}-{nanos}-{}", short_hash(subject, 6))
}

fn new_compaction_job_id(device_id: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("zccompact-{nanos}-{}", short_hash(device_id, 8))
}

fn default_snapshot_device_id(snapshot_id: &str, mode: &str) -> String {
    format!("snapdev-{mode}-{}", short_hash(snapshot_id, 12))
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

fn volume_capacity_u64(spec: &VolumeSpec) -> Result<u64, String> {
    match spec.backend.as_str() {
        "raw-block" => {
            if let Some(raw) = spec.raw_device.as_ref() {
                block_device_size(Path::new(raw)).or_else(|_| {
                    u64::try_from(spec.capacity_bytes)
                        .map_err(|_| "volume capacity must be non-negative".to_string())
                })
            } else {
                u64::try_from(spec.capacity_bytes)
                    .map_err(|_| "volume capacity must be non-negative".to_string())
            }
        }
        _ => u64::try_from(spec.capacity_bytes)
            .map_err(|_| "volume capacity must be non-negative".to_string()),
    }
}

fn replication_limit(requested: Option<u64>, capacity: u64, label: &str) -> Result<u64, String> {
    let limit = requested.unwrap_or(capacity);
    if limit == 0 {
        return Err("replication bytes must be greater than zero".to_string());
    }
    if limit > capacity {
        return Err(format!(
            "replication bytes {limit} exceeds {label} capacity {capacity}"
        ));
    }
    Ok(limit)
}

fn freeze_response(active: &ActiveFreeze) -> api::FreezeResponse {
    api::FreezeResponse {
        barrier_id: active.barrier_id.clone(),
        frozen_mounts: path_strings(&active.frozen_mounts),
        remaining_ms: remaining_ms(active.deadline),
    }
}

fn remaining_ms(deadline: Instant) -> u64 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn path_strings(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect()
}

fn is_already_thawed_error(error: &str) -> bool {
    error.contains("Invalid argument") || error.contains("not frozen")
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn validate_token(value: &str, name: &str) -> Result<(), String> {
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

fn validate_state_id(value: &str, name: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{name} must not be empty"));
    }
    if value.len() > 128 {
        return Err(format!("{name} must be at most 128 bytes"));
    }
    if value
        .bytes()
        .any(|b| !(b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-'))
    {
        return Err(format!(
            "{name} may contain only ASCII letters, digits, dot, underscore, and dash"
        ));
    }
    Ok(())
}

fn validate_device_name(value: &str, name: &str) -> Result<(), String> {
    validate_state_id(value, name)?;
    if value.contains('.') || value.contains('_') {
        return Err(format!(
            "{name} may contain only ASCII letters, digits, and dash"
        ));
    }
    Ok(())
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

fn parse_key_values(body: &str) -> BTreeMap<String, String> {
    body.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

fn required<'a>(map: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str, String> {
    map.get(key)
        .map(String::as_str)
        .ok_or_else(|| format!("state missing {key}"))
}

fn nonempty_value(map: &BTreeMap<String, String>, key: &str) -> Option<String> {
    map.get(key).filter(|value| !value.is_empty()).cloned()
}

fn parse_u64(value: &str, name: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{name} must be an unsigned integer"))
}

fn parse_i64(value: &str, name: &str) -> Result<i64, String> {
    value
        .parse::<i64>()
        .map_err(|_| format!("{name} in state is invalid"))
}

fn parse_bool(value: &str, name: &str) -> Result<bool, String> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(format!("{name} in state is invalid")),
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn value<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn invalid_input(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replication_sample(state: &str, idle_millis: u64) -> api::ReplicationDelaySample {
        api::ReplicationDelaySample {
            repl_id: "test-repl".to_string(),
            role: "send".to_string(),
            state: state.to_string(),
            subject: "volume:test-volume".to_string(),
            stream_kind: "volume".to_string(),
            volume_id: Some("test-volume".to_string()),
            snapshot_id: None,
            peer: "127.0.0.1".to_string(),
            port: Some(19000),
            replication_mode: "async".to_string(),
            target_cluster: Some("cluster-b".to_string()),
            gateway_endpoint: Some("gw-b".to_string()),
            spillover_tier: Some("fast".to_string()),
            bytes: 0,
            bytes_limit: Some(4096),
            bytes_remaining: Some(4096),
            started_at_millis: 1_000,
            updated_at_millis: 1_000,
            finished_at_millis: None,
            elapsed_millis: idle_millis,
            idle_millis,
        }
    }

    #[test]
    fn stats_attention_counts_failed_and_idle_active_jobs() {
        let mut failed = stats_totals();
        add_sample_to_totals(&mut failed, &replication_sample("failed", 0));
        assert!(failed.attention_required);
        assert_eq!(failed.attention_failed_jobs, 1);
        assert_eq!(failed.attention_idle_jobs, 0);

        let mut idle = stats_totals();
        add_sample_to_totals(
            &mut idle,
            &replication_sample("sending", DEFAULT_REPLICATION_ATTENTION_IDLE_MS),
        );
        assert!(idle.attention_required);
        assert_eq!(idle.attention_failed_jobs, 0);
        assert_eq!(idle.attention_idle_jobs, 1);

        let mut succeeded = stats_totals();
        add_sample_to_totals(
            &mut succeeded,
            &replication_sample("succeeded", DEFAULT_REPLICATION_ATTENTION_IDLE_MS * 2),
        );
        assert!(!succeeded.attention_required);
        assert_eq!(succeeded.attention_failed_jobs, 0);
        assert_eq!(succeeded.attention_idle_jobs, 0);
    }

    #[test]
    fn prometheus_attention_metric_is_label_free() {
        let metrics = render_prometheus_metrics(&[], &[]);
        assert!(metrics.contains("\nzccusan_replication_attention_required 0\n"));
        assert!(
            metrics.contains("\nzccusan_replication_attention_idle_threshold_seconds 30.000\n")
        );
    }

    #[test]
    fn snapshot_device_modes_are_userspace_cow_or_wal_only() {
        assert_eq!(normalize_snapshot_device_mode("cow").unwrap(), "cow");
        assert_eq!(
            normalize_snapshot_device_mode("copy-on-write").unwrap(),
            "cow"
        );
        assert_eq!(normalize_snapshot_device_mode("wal").unwrap(), "wal");
        assert!(normalize_snapshot_device_mode("loop").is_err());
        assert!(normalize_snapshot_device_mode("copy").is_err());
    }

    #[test]
    fn compaction_strategy_defaults_to_stream_rewrite() {
        assert_eq!(
            normalize_compaction_strategy(None).unwrap(),
            "stream-rewrite"
        );
        assert_eq!(
            normalize_compaction_strategy(Some("off-machine")).unwrap(),
            "stream-rewrite"
        );
        assert_eq!(
            normalize_compaction_strategy(Some("in-place")).unwrap(),
            "in-place"
        );
    }

    #[test]
    fn compaction_job_state_round_trips() {
        let job = api::SnapshotCompactionJob {
            job_id: "zccompact-test".to_string(),
            device_id: "snapdev-test".to_string(),
            snapshot_id: "snap-test".to_string(),
            mode: "wal".to_string(),
            strategy: "stream-rewrite".to_string(),
            phase: "stream-out".to_string(),
            state: "running".to_string(),
            bytes_compacted: 1024,
            bytes_streamed_out: 2048,
            bytes_streamed_in: 1024,
            bytes_total: Some(4096),
            outbound_stream_id: Some("stream-out-a".to_string()),
            inbound_stream_id: Some("stream-in-a".to_string()),
            target_location: Some("region-b/device-x".to_string()),
            worker_id: Some("worker-a".to_string()),
            checkpoint: Some("wal=7:4096".to_string()),
            started_at_millis: 10,
            updated_at_millis: 20,
            finished_at_millis: None,
            error: None,
        };
        let decoded = snapshot_compaction_from_state(&snapshot_compaction_to_state(&job)).unwrap();
        assert_eq!(decoded, job);

        let metrics = render_prometheus_metrics(&[], &[job]);
        assert!(metrics.contains("zccusan_snapshot_compaction_job_info"));
        assert!(metrics.contains("mode=\"wal\""));
        assert!(metrics.contains("strategy=\"stream-rewrite\""));
        assert!(metrics.contains("phase=\"stream-out\""));
        assert!(metrics.contains("state=\"running\""));
    }
}
