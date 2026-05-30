use std::env;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use zcutils::block::control as control_api;
use zcutils::{
    ZcStreamEncryption, zc_stream_bind_listener, zc_stream_generate_token,
    zc_stream_receive_listener_to_writer, zc_stream_send_reader_to_tcp,
};

const DEFAULT_CONTROL_SOCKET: &str = "/var/lib/zcblock-csi/control.sock";
const DEFAULT_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Default)]
struct Opts {
    socket: Option<PathBuf>,
    control_url: Option<String>,
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    listen: Option<String>,
    peer: Option<String>,
    port: Option<u16>,
    token: Option<String>,
    generate_token: bool,
    volume: Option<String>,
    snapshot: Option<String>,
    repl_id: Option<String>,
    bytes: Option<u64>,
    mode: Option<String>,
    scope: Option<String>,
    target_cluster: Option<String>,
    gateway_endpoint: Option<String>,
    spillover_tier: Option<String>,
}

struct BoundedWriter<W> {
    inner: W,
    remaining: u64,
}

impl<W> BoundedWriter<W> {
    fn new(inner: W, remaining: u64) -> Self {
        Self { inner, remaining }
    }
}

impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() as u64 > self.remaining {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "replication byte limit exceeded",
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

fn main() {
    if let Err(e) = run() {
        eprintln!("zcrepl: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print!("{}", usage());
        return Ok(());
    }
    let command = args.remove(0);
    let opts = parse_opts(args)?;
    match command.as_str() {
        "token" | "generate-token" => {
            println!("{}", zc_stream_generate_token().map_err(|e| e.to_string())?);
            Ok(())
        }
        "recv" | "receive" => recv(opts),
        "send" => send(opts),
        "csi-recv" | "controller-recv" => csi_recv(opts),
        "csi-send" | "controller-send" => csi_send(opts),
        "csi-status" | "controller-status" => csi_status(opts),
        "csi-mode" | "controller-mode" => csi_mode(opts),
        "csi-route" | "controller-route" => csi_route(opts),
        other => Err(format!("unknown command {other}\n{}", usage())),
    }
}

fn recv(opts: Opts) -> Result<(), String> {
    let listen = opts.listen.as_deref().unwrap_or("0.0.0.0");
    let port = opts.port.unwrap_or(0);
    let output = opts.output.ok_or("recv requires --output PATH")?;
    let token = match (opts.token, opts.generate_token) {
        (Some(token), _) => token,
        (None, true) => zc_stream_generate_token().map_err(|e| e.to_string())?,
        (None, false) => return Err("recv requires --token TOKEN or --generate-token".to_string()),
    };
    validate_token(&token, "token")?;

    let listener =
        zc_stream_bind_listener(listen, (port, port)).map_err(|e| format!("bind receiver: {e}"))?;
    let bound_port = listener
        .local_addr()
        .map_err(|e| format!("read listener address: {e}"))?
        .port();
    eprintln!("zcrepl-ready listen={listen} port={bound_port} token={token}");

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&output)
        .map_err(|e| format!("open output {}: {e}", output.display()))?;
    let limit = opts.bytes.unwrap_or(u64::MAX);
    let writer = BoundedWriter::new(file, limit);
    let (peer, bytes) = zc_stream_receive_listener_to_writer(
        listener,
        writer,
        Some(&token),
        ZcStreamEncryption::Aes256,
        false,
        DEFAULT_BUFFER_BYTES,
    )
    .map_err(|e| format!("receive stream: {e}"))?;
    println!(
        "zcrepl-recv-result peer={} bytes={} output={}",
        peer,
        bytes,
        output.display()
    );
    Ok(())
}

fn send(opts: Opts) -> Result<(), String> {
    let input = opts.input.ok_or("send requires --input PATH")?;
    let peer = opts.peer.ok_or("send requires --peer HOST")?;
    let port = opts.port.ok_or("send requires --port PORT")?;
    if port == 0 {
        return Err("send --port must be greater than zero".to_string());
    }
    let token = opts.token.ok_or("send requires --token TOKEN")?;
    validate_token(&token, "token")?;
    let file = OpenOptions::new()
        .read(true)
        .open(&input)
        .map_err(|e| format!("open input {}: {e}", input.display()))?;
    let reader = file.take(opts.bytes.unwrap_or(u64::MAX));
    let bytes = zc_stream_send_reader_to_tcp(
        reader,
        &peer,
        port,
        None,
        Some(&token),
        ZcStreamEncryption::Aes256,
        false,
        DEFAULT_BUFFER_BYTES,
    )
    .map_err(|e| format!("send stream: {e}"))?;
    println!(
        "zcrepl-send-result peer={peer}:{port} bytes={bytes} input={}",
        input.display()
    );
    Ok(())
}

fn csi_recv(opts: Opts) -> Result<(), String> {
    let control_url = control_url(opts.control_url.clone());
    let volume = opts.volume.ok_or("csi-recv requires --volume ID")?;
    let listen = opts.listen.unwrap_or_else(|| "0.0.0.0".to_string());
    let port = opts.port.unwrap_or(0);
    let token = opts.token.unwrap_or_else(|| "auto".to_string());
    if let Some(control_url) = control_url {
        let token_request = if token == "auto" { None } else { Some(token) };
        let client = control_api::HttpControlClient::new(&control_url)?;
        let response = client.start_receive(&control_api::StartReceiveRequest {
            volume_id: volume,
            listen,
            port,
            token: token_request,
            bytes: opts.bytes,
        })?;
        let mut fields = format!(
            "OK repl_id={} role={} volume={} target={} listen={} port={} token={} replication_mode={}",
            response.repl_id,
            response.role,
            response.volume_id,
            control_field(&response.target),
            control_field(&response.listen),
            response.port,
            response.token,
            control_field(&response.replication_mode)
        );
        append_route_response_fields(
            &mut fields,
            response.target_cluster.as_deref(),
            response.gateway_endpoint.as_deref(),
            response.spillover_tier.as_deref(),
        );
        println!("{fields}");
        return Ok(());
    }
    let mut line = format!("REPL_RECV volume={volume} listen={listen} port={port} token={token}");
    if let Some(bytes) = opts.bytes {
        line.push_str(&format!(" bytes={bytes}"));
    }
    print!("{}", send_control(&control_socket(opts.socket), &line)?);
    Ok(())
}

fn csi_send(opts: Opts) -> Result<(), String> {
    let control_url = control_url(opts.control_url.clone());
    if opts.volume.is_some() == opts.snapshot.is_some() {
        return Err("csi-send requires exactly one of --volume ID or --snapshot ID".to_string());
    }
    let peer = opts.peer.ok_or("csi-send requires --peer HOST")?;
    let port = opts.port.ok_or("csi-send requires --port PORT")?;
    if port == 0 {
        return Err("csi-send --port must be greater than zero".to_string());
    }
    let token = opts.token.ok_or("csi-send requires --token TOKEN")?;
    validate_token(&token, "token")?;
    if let Some(control_url) = control_url {
        let client = control_api::HttpControlClient::new(&control_url)?;
        let response = client.start_send(&control_api::StartSendRequest {
            volume_id: opts.volume,
            snapshot_id: opts.snapshot,
            peer,
            port,
            token,
            bytes: opts.bytes,
        })?;
        let mut fields = format!(
            "OK repl_id={} role={} source={} peer={} port={} bytes_limit={} replication_mode={}",
            response.repl_id,
            response.role,
            control_field(&response.source),
            control_field(&response.peer),
            response.port,
            response.bytes_limit,
            control_field(&response.replication_mode)
        );
        append_route_response_fields(
            &mut fields,
            response.target_cluster.as_deref(),
            response.gateway_endpoint.as_deref(),
            response.spillover_tier.as_deref(),
        );
        println!("{fields}");
        return Ok(());
    }
    let mut line = if let Some(volume) = opts.volume {
        format!("REPL_SEND volume={volume} peer={peer} port={port} token={token}")
    } else {
        format!(
            "REPL_SEND snapshot={} peer={peer} port={port} token={token}",
            opts.snapshot.expect("snapshot checked")
        )
    };
    if let Some(bytes) = opts.bytes {
        line.push_str(&format!(" bytes={bytes}"));
    }
    print!("{}", send_control(&control_socket(opts.socket), &line)?);
    Ok(())
}

fn csi_status(opts: Opts) -> Result<(), String> {
    if let Some(control_url) = control_url(opts.control_url.clone()) {
        let client = control_api::HttpControlClient::new(&control_url)?;
        let response = client.replication_status(opts.repl_id.as_deref())?;
        if let Some(repl_id) = opts.repl_id.as_ref() {
            if response.jobs.is_empty() {
                println!("ERR repl_id {repl_id} not found");
            } else {
                println!("OK {}", status_fields(&response.jobs[0]));
            }
        } else {
            println!("OK jobs={}", response.jobs.len());
            for job in response.jobs {
                println!("JOB {}", status_fields(&job));
            }
        }
        return Ok(());
    }
    let line = match opts.repl_id {
        Some(repl_id) => format!("REPL_STATUS repl_id={repl_id}"),
        None => "REPL_STATUS".to_string(),
    };
    print!("{}", send_control(&control_socket(opts.socket), &line)?);
    Ok(())
}

fn csi_mode(opts: Opts) -> Result<(), String> {
    let control_url = control_url(opts.control_url.clone())
        .ok_or("csi-mode requires --control-url URL or ZCBLOCK_CONTROL_URL")?;
    let client = control_api::HttpControlClient::new(&control_url)?;
    if let Some(mode) = opts.mode {
        let response = client.set_replication_mode(&control_api::SetReplicationModeRequest {
            scope: opts.scope,
            mode,
        })?;
        println!("OK {}", mode_fields(&response.policy));
        return Ok(());
    }

    let response = client.replication_modes()?;
    if let Some(scope) = opts.scope.as_ref() {
        match response
            .policies
            .iter()
            .find(|policy| &policy.scope == scope)
        {
            Some(policy) => println!("OK {}", mode_fields(policy)),
            None => println!("ERR scope {} not found", control_field(scope)),
        }
    } else {
        println!("OK policies={}", response.policies.len());
        for policy in response.policies {
            println!("POLICY {}", mode_fields(&policy));
        }
    }
    Ok(())
}

fn csi_route(opts: Opts) -> Result<(), String> {
    let control_url = control_url(opts.control_url.clone())
        .ok_or("csi-route requires --control-url URL or ZCBLOCK_CONTROL_URL")?;
    let client = control_api::HttpControlClient::new(&control_url)?;
    if opts.target_cluster.is_some()
        || opts.gateway_endpoint.is_some()
        || opts.spillover_tier.is_some()
    {
        let response = client.set_replication_route(&control_api::SetReplicationRouteRequest {
            scope: opts.scope,
            target_cluster: opts
                .target_cluster
                .ok_or("csi-route set requires --target-cluster NAME")?,
            gateway_endpoint: opts
                .gateway_endpoint
                .ok_or("csi-route set requires --gateway-endpoint HOST:PORT")?,
            spillover_tier: opts
                .spillover_tier
                .ok_or("csi-route set requires --spillover-tier NAME")?,
        })?;
        println!("OK {}", route_fields(&response.route));
        return Ok(());
    }

    let response = client.replication_routes()?;
    if let Some(scope) = opts.scope.as_ref() {
        match response.routes.iter().find(|route| &route.scope == scope) {
            Some(route) => println!("OK {}", route_fields(route)),
            None => println!("ERR scope {} not found", control_field(scope)),
        }
    } else {
        println!("OK routes={}", response.routes.len());
        for route in response.routes {
            println!("ROUTE {}", route_fields(&route));
        }
    }
    Ok(())
}

fn parse_opts(args: Vec<String>) -> Result<Opts, String> {
    let mut opts = Opts::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--socket" => {
                i += 1;
                opts.socket = Some(PathBuf::from(value(&args, i, "--socket")?));
            }
            "--control-url" => {
                i += 1;
                opts.control_url = Some(value(&args, i, "--control-url")?.to_string());
            }
            "--input" | "--source" => {
                i += 1;
                opts.input = Some(PathBuf::from(value(&args, i, arg)?));
            }
            "--output" | "--target" => {
                i += 1;
                opts.output = Some(PathBuf::from(value(&args, i, arg)?));
            }
            "--listen" | "--listen-address" => {
                i += 1;
                opts.listen = Some(value(&args, i, arg)?.to_string());
            }
            "--peer" | "--peer-address" => {
                i += 1;
                opts.peer = Some(value(&args, i, arg)?.to_string());
            }
            "--port" => {
                i += 1;
                opts.port = Some(parse_u16(value(&args, i, "--port")?, "--port")?);
            }
            "--token" => {
                i += 1;
                opts.token = Some(value(&args, i, "--token")?.to_string());
            }
            "--generate-token" => opts.generate_token = true,
            "--volume" | "--volume-id" => {
                i += 1;
                opts.volume = Some(value(&args, i, arg)?.to_string());
            }
            "--snapshot" | "--snapshot-id" => {
                i += 1;
                opts.snapshot = Some(value(&args, i, arg)?.to_string());
            }
            "--repl-id" | "--id" => {
                i += 1;
                opts.repl_id = Some(value(&args, i, arg)?.to_string());
            }
            "--bytes" => {
                i += 1;
                opts.bytes = Some(parse_u64(value(&args, i, "--bytes")?, "--bytes")?);
            }
            "--mode" => {
                i += 1;
                opts.mode = Some(value(&args, i, "--mode")?.to_string());
            }
            "--scope" => {
                i += 1;
                opts.scope = Some(value(&args, i, "--scope")?.to_string());
            }
            "--target-cluster" => {
                i += 1;
                opts.target_cluster = Some(value(&args, i, "--target-cluster")?.to_string());
            }
            "--gateway-endpoint" => {
                i += 1;
                opts.gateway_endpoint = Some(value(&args, i, "--gateway-endpoint")?.to_string());
            }
            "--spillover-tier" => {
                i += 1;
                opts.spillover_tier = Some(value(&args, i, "--spillover-tier")?.to_string());
            }
            _ if arg.starts_with("--socket=") => {
                opts.socket = Some(PathBuf::from(arg.trim_start_matches("--socket=")));
            }
            _ if arg.starts_with("--control-url=") => {
                opts.control_url = Some(arg.trim_start_matches("--control-url=").to_string());
            }
            _ if arg.starts_with("--input=") => {
                opts.input = Some(PathBuf::from(arg.trim_start_matches("--input=")));
            }
            _ if arg.starts_with("--source=") => {
                opts.input = Some(PathBuf::from(arg.trim_start_matches("--source=")));
            }
            _ if arg.starts_with("--output=") => {
                opts.output = Some(PathBuf::from(arg.trim_start_matches("--output=")));
            }
            _ if arg.starts_with("--target=") => {
                opts.output = Some(PathBuf::from(arg.trim_start_matches("--target=")));
            }
            _ if arg.starts_with("--listen=") => {
                opts.listen = Some(arg.trim_start_matches("--listen=").to_string());
            }
            _ if arg.starts_with("--listen-address=") => {
                opts.listen = Some(arg.trim_start_matches("--listen-address=").to_string());
            }
            _ if arg.starts_with("--peer=") => {
                opts.peer = Some(arg.trim_start_matches("--peer=").to_string());
            }
            _ if arg.starts_with("--peer-address=") => {
                opts.peer = Some(arg.trim_start_matches("--peer-address=").to_string());
            }
            _ if arg.starts_with("--port=") => {
                opts.port = Some(parse_u16(arg.trim_start_matches("--port="), "--port")?);
            }
            _ if arg.starts_with("--token=") => {
                opts.token = Some(arg.trim_start_matches("--token=").to_string());
            }
            _ if arg.starts_with("--volume=") => {
                opts.volume = Some(arg.trim_start_matches("--volume=").to_string());
            }
            _ if arg.starts_with("--volume-id=") => {
                opts.volume = Some(arg.trim_start_matches("--volume-id=").to_string());
            }
            _ if arg.starts_with("--snapshot=") => {
                opts.snapshot = Some(arg.trim_start_matches("--snapshot=").to_string());
            }
            _ if arg.starts_with("--snapshot-id=") => {
                opts.snapshot = Some(arg.trim_start_matches("--snapshot-id=").to_string());
            }
            _ if arg.starts_with("--repl-id=") => {
                opts.repl_id = Some(arg.trim_start_matches("--repl-id=").to_string());
            }
            _ if arg.starts_with("--id=") => {
                opts.repl_id = Some(arg.trim_start_matches("--id=").to_string());
            }
            _ if arg.starts_with("--bytes=") => {
                opts.bytes = Some(parse_u64(arg.trim_start_matches("--bytes="), "--bytes")?);
            }
            _ if arg.starts_with("--mode=") => {
                opts.mode = Some(arg.trim_start_matches("--mode=").to_string());
            }
            _ if arg.starts_with("--scope=") => {
                opts.scope = Some(arg.trim_start_matches("--scope=").to_string());
            }
            _ if arg.starts_with("--target-cluster=") => {
                opts.target_cluster = Some(arg.trim_start_matches("--target-cluster=").to_string());
            }
            _ if arg.starts_with("--gateway-endpoint=") => {
                opts.gateway_endpoint =
                    Some(arg.trim_start_matches("--gateway-endpoint=").to_string());
            }
            _ if arg.starts_with("--spillover-tier=") => {
                opts.spillover_tier = Some(arg.trim_start_matches("--spillover-tier=").to_string());
            }
            _ => return Err(format!("unknown option {arg}\n{}", usage())),
        }
        i += 1;
    }
    Ok(opts)
}

fn send_control(socket: &PathBuf, line: &str) -> Result<String, String> {
    validate_raw_line(line)?;
    let mut stream =
        UnixStream::connect(socket).map_err(|e| format!("connect {}: {e}", socket.display()))?;
    stream
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("write control command: {e}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("shutdown control write: {e}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("read control response: {e}"))?;
    Ok(response)
}

fn control_socket(socket: Option<PathBuf>) -> PathBuf {
    socket
        .or_else(|| {
            env::var("ZCBLOCK_CSI_CONTROL_SOCKET")
                .ok()
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONTROL_SOCKET))
}

fn control_url(value: Option<String>) -> Option<String> {
    value.or_else(|| env::var("ZCBLOCK_CONTROL_URL").ok())
}

fn value<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_u16(value: &str, flag: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|_| format!("{flag} must be an integer from 0 to 65535"))
}

fn parse_u64(value: &str, flag: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("{flag} must be an integer"))?;
    if parsed == 0 {
        return Err(format!("{flag} must be greater than zero"));
    }
    Ok(parsed)
}

fn validate_token(value: &str, name: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 512
        || value.chars().any(|c| c.is_whitespace() || c == '\0')
    {
        return Err(format!(
            "{name} must be non-empty, at most 512 bytes, and contain no whitespace or NUL"
        ));
    }
    Ok(())
}

fn validate_raw_line(value: &str) -> Result<(), String> {
    if value.is_empty() || value.contains('\n') || value.contains('\0') {
        return Err("control command must be one non-empty line without NUL".to_string());
    }
    Ok(())
}

fn status_fields(job: &control_api::ReplicationJob) -> String {
    let mut fields = format!(
        "repl_id={} role={} state={} subject={} peer={} bytes={} replication_mode={} started_at_secs={}",
        job.repl_id,
        job.role,
        job.state,
        control_field(&job.subject),
        control_field(&job.peer),
        job.bytes,
        control_field(&job.replication_mode),
        job.started_at_secs
    );
    if let Some(port) = job.port {
        fields.push_str(&format!(" port={port}"));
    }
    if let Some(bytes_limit) = job.bytes_limit {
        fields.push_str(&format!(" bytes_limit={bytes_limit}"));
        fields.push_str(&format!(
            " bytes_remaining={}",
            bytes_limit.saturating_sub(job.bytes)
        ));
    }
    if job.started_at_millis != 0 {
        fields.push_str(&format!(" started_at_millis={}", job.started_at_millis));
    }
    if job.updated_at_millis != 0 {
        fields.push_str(&format!(" updated_at_millis={}", job.updated_at_millis));
    }
    if let Some(finished_at_secs) = job.finished_at_secs {
        fields.push_str(&format!(" finished_at_secs={finished_at_secs}"));
    }
    if let Some(finished_at_millis) = job.finished_at_millis {
        fields.push_str(&format!(" finished_at_millis={finished_at_millis}"));
    }
    if let Some(error) = job.error.as_ref() {
        fields.push_str(&format!(" error={}", control_field(error)));
    }
    if let Some(target_cluster) = job.target_cluster.as_ref() {
        fields.push_str(&format!(
            " target_cluster={}",
            control_field(target_cluster)
        ));
    }
    if let Some(gateway_endpoint) = job.gateway_endpoint.as_ref() {
        fields.push_str(&format!(
            " gateway_endpoint={}",
            control_field(gateway_endpoint)
        ));
    }
    if let Some(spillover_tier) = job.spillover_tier.as_ref() {
        fields.push_str(&format!(
            " spillover_tier={}",
            control_field(spillover_tier)
        ));
    }
    fields
}

fn mode_fields(policy: &control_api::ReplicationModeSpec) -> String {
    format!(
        "scope={} mode={} generation={} updated_at_secs={}",
        control_field(&policy.scope),
        control_field(&policy.mode),
        policy.generation,
        policy.updated_at_secs
    )
}

fn route_fields(route: &control_api::ReplicationRouteSpec) -> String {
    format!(
        "scope={} target_cluster={} gateway_endpoint={} spillover_tier={} generation={} updated_at_secs={}",
        control_field(&route.scope),
        control_field(&route.target_cluster),
        control_field(&route.gateway_endpoint),
        control_field(&route.spillover_tier),
        route.generation,
        route.updated_at_secs
    )
}

fn append_route_response_fields(
    fields: &mut String,
    target_cluster: Option<&str>,
    gateway_endpoint: Option<&str>,
    spillover_tier: Option<&str>,
) {
    if let Some(target_cluster) = target_cluster {
        fields.push_str(&format!(
            " target_cluster={}",
            control_field(target_cluster)
        ));
    }
    if let Some(gateway_endpoint) = gateway_endpoint {
        fields.push_str(&format!(
            " gateway_endpoint={}",
            control_field(gateway_endpoint)
        ));
    }
    if let Some(spillover_tier) = spillover_tier {
        fields.push_str(&format!(
            " spillover_tier={}",
            control_field(spillover_tier)
        ));
    }
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

fn usage() -> &'static str {
    "usage:
  zcrepl token
  zcrepl recv --output PATH [--listen ADDR] [--port PORT] (--token TOKEN | --generate-token) [--bytes N]
  zcrepl send --input PATH --peer HOST --port PORT --token TOKEN [--bytes N]
  zcrepl csi-recv (--socket PATH | --control-url URL) --volume ID [--listen ADDR] [--port PORT] [--token TOKEN|auto] [--bytes N]
  zcrepl csi-send (--socket PATH | --control-url URL) (--volume ID | --snapshot ID) --peer HOST --port PORT --token TOKEN [--bytes N]
  zcrepl csi-status (--socket PATH | --control-url URL) [--repl-id ID]
  zcrepl csi-mode --control-url URL [--scope SCOPE] [--mode async|sync]
  zcrepl csi-route --control-url URL [--scope SCOPE] [--target-cluster NAME --gateway-endpoint HOST:PORT --spillover-tier NAME]
"
}
