use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZcFanoutLocalPath {
    pub path_id: usize,
    pub address: String,
    pub cpu_list: String,
    pub numa_node: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZcFanoutTargetPath {
    pub path_id: usize,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZcFanoutTarget {
    pub target_id: usize,
    pub name: String,
    pub paths: Vec<ZcFanoutTargetPath>,
    pub base_port: u16,
    pub port_end: u16,
    pub receiver_command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZcFanoutLane {
    pub global_lane: usize,
    pub target_id: usize,
    pub target_name: String,
    pub target_lane: usize,
    pub path_id: usize,
    pub local_address: String,
    pub remote_address: String,
    pub cpu_list: String,
    pub numa_node: Option<i32>,
    pub port: u16,
    pub first_chunk: u64,
    pub chunk_stride: u64,
    pub chunk_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZcFanoutPlan {
    pub version: u32,
    pub mode: String,
    pub source: String,
    pub bytes: u64,
    pub chunk_bytes: u64,
    pub branch_count: usize,
    pub lanes_per_target: usize,
    pub total_lanes: usize,
    pub local_paths: Vec<ZcFanoutLocalPath>,
    pub targets: Vec<ZcFanoutTarget>,
    pub lanes: Vec<ZcFanoutLane>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
struct FanoutPlanArgs {
    source: String,
    bytes: u64,
    chunk_bytes: u64,
    lanes_per_target: usize,
    base_port: u16,
    port_stride: Option<u16>,
    local_paths: Vec<ZcFanoutLocalPath>,
    targets: Vec<(String, Vec<String>)>,
    receive_cpu_list: Option<String>,
    receiver_output_prefix: String,
    encryption: String,
    token: Option<String>,
    disable_authentication: bool,
}

impl Default for FanoutPlanArgs {
    fn default() -> Self {
        Self {
            source: "-".to_string(),
            bytes: 0,
            chunk_bytes: 1024 * 1024,
            lanes_per_target: 64,
            base_port: 46000,
            port_stride: None,
            local_paths: Vec::new(),
            targets: Vec::new(),
            receive_cpu_list: None,
            receiver_output_prefix: "/dev/null".to_string(),
            encryption: "none".to_string(),
            token: None,
            disable_authentication: true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ZcFanoutSendLaneResult {
    global_lane: usize,
    target_name: String,
    target_lane: usize,
    remote_address: String,
    port: u16,
    bytes: u64,
    chunks: u64,
    max_end: u64,
    seconds: f64,
    topology: String,
}

#[derive(Debug, Clone, Serialize)]
struct ZcFanoutSendResult {
    mode: String,
    source: String,
    bytes: u64,
    chunk_bytes: u64,
    branch_count: usize,
    lanes_per_target: usize,
    total_lanes: usize,
    seconds: f64,
    mibps: f64,
    gbitps: f64,
    logical_4k_iops: f64,
    lanes: Vec<ZcFanoutSendLaneResult>,
}

fn parse_size_arg(value: &str, flag: &str) -> io::Result<u64> {
    let value = value.trim();
    if value.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{flag} cannot be empty"),
        ));
    }
    let split = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());
    let number = value[..split].parse::<u64>().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("parse {flag} value {value:?}: {err}"),
        )
    })?;
    let suffix = value[split..].to_ascii_lowercase();
    let multiplier = match suffix.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "ki" | "kib" => 1024,
        "m" | "mb" | "mi" | "mib" => 1024_u64.pow(2),
        "g" | "gb" | "gi" | "gib" => 1024_u64.pow(3),
        "t" | "tb" | "ti" | "tib" => 1024_u64.pow(4),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown {flag} suffix in {value:?}"),
            ));
        }
    };
    number.checked_mul(multiplier).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{flag} value {value:?} overflows u64"),
        )
    })
}

fn next_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> io::Result<String> {
    args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{flag} requires a value"),
        )
    })
}

fn parse_usize_value(value: &str, flag: &str) -> io::Result<usize> {
    value.parse::<usize>().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("parse {flag} value {value:?}: {err}"),
        )
    })
}

fn parse_u16_value(value: &str, flag: &str) -> io::Result<u16> {
    value.parse::<u16>().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("parse {flag} value {value:?}: {err}"),
        )
    })
}

fn parse_local_path(value: &str, path_id: usize) -> io::Result<ZcFanoutLocalPath> {
    let mut parts = value.split('@');
    let address = parts.next().unwrap_or_default();
    let cpu_list = parts.next().unwrap_or_default();
    let numa_node = parts
        .next()
        .map(|part| {
            part.parse::<i32>().map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("parse --local-path NUMA in {value:?}: {err}"),
                )
            })
        })
        .transpose()?;
    if parts.next().is_some() || address.is_empty() || cpu_list.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local path must be ADDR@CPU_LIST[@NUMA]",
        ));
    }
    Ok(ZcFanoutLocalPath {
        path_id,
        address: address.to_string(),
        cpu_list: cpu_list.to_string(),
        numa_node,
    })
}

fn parse_target(value: &str) -> io::Result<(String, Vec<String>)> {
    let (name, addresses) = value.split_once('=').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "target must be NAME=ADDR[,ADDR...]",
        )
    })?;
    let paths = addresses
        .split(',')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if name.is_empty() || paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target must include a non-empty name and at least one address",
        ));
    }
    Ok((name.to_string(), paths))
}

fn receiver_command(
    target: &str,
    paths: &[ZcFanoutTargetPath],
    base_port: u16,
    lanes_per_target: usize,
    chunk_bytes: u64,
    receive_cpu_list: Option<&str>,
    output_prefix: &str,
    encryption: &str,
    disable_authentication: bool,
) -> String {
    let port_end = base_port + lanes_per_target as u16 - 1;
    let output = if output_prefix == "/dev/null" {
        "/dev/null".to_string()
    } else {
        format!("{output_prefix}/{target}.zcraid")
    };
    let listen = if paths.len() == 1 {
        format!("--listen-address {}", paths[0].address)
    } else {
        format!(
            "--listen-addresses {}",
            paths
                .iter()
                .map(|path| path.address.as_str())
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let mut cmd = format!(
        "zc-tcpmux-receive {listen} --listen-port-range {base_port}-{port_end} --lanes {lanes_per_target} --receive-assembly ordered --output {output} --buffer-bytes {chunk_bytes} --encryption {encryption}"
    );
    if disable_authentication {
        cmd.push_str(" --disable-authentication");
    }
    if let Some(cpu_list) = receive_cpu_list {
        cmd.push_str(" --pin-cpus --cpu-list ");
        cmd.push_str(cpu_list);
    }
    cmd
}

fn path_index(lane: usize, lanes: usize, path_count: usize) -> usize {
    lane.saturating_mul(path_count) / lanes.max(1)
}

fn first_lane_for_path(path_id: usize, lanes: usize, path_count: usize) -> usize {
    path_id
        .saturating_mul(lanes)
        .saturating_add(path_count.saturating_sub(1))
        / path_count.max(1)
}

fn path_local_port(
    base_port: u16,
    lane: usize,
    lanes: usize,
    path_count: usize,
) -> io::Result<u16> {
    let path_id = path_index(lane, lanes, path_count);
    let first_lane = first_lane_for_path(path_id, lanes, path_count);
    let port_offset = lane.checked_sub(first_lane).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "fanout lane/path port schedule underflow",
        )
    })?;
    base_port
        .checked_add(u16::try_from(port_offset).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "fanout path-local port offset exceeds u16",
            )
        })?)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "port range overflows"))
}

fn build_plan(args: FanoutPlanArgs) -> io::Result<ZcFanoutPlan> {
    if args.bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zcfanplan requires --bytes N",
        ));
    }
    if args.chunk_bytes == 0 || args.lanes_per_target == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk bytes and lanes per target must be non-zero",
        ));
    }
    if args.local_paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zcfanplan requires at least one --local-path ADDR@CPU_LIST[@NUMA]",
        ));
    }
    if args.targets.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zcfanplan requires at least one --target NAME=ADDR[,ADDR...]",
        ));
    }
    let branch_count = args.targets.len();
    let port_stride = args.port_stride.unwrap_or(args.lanes_per_target as u16);
    let total_lanes = branch_count
        .checked_mul(args.lanes_per_target)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "lane count overflows"))?;
    let mut targets = Vec::with_capacity(branch_count);
    let mut lanes = Vec::with_capacity(total_lanes);

    for (target_id, (name, path_addresses)) in args.targets.iter().enumerate() {
        let base_port = args
            .base_port
            .checked_add((target_id as u16).saturating_mul(port_stride))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "port range overflows"))?;
        let port_end = base_port
            .checked_add(args.lanes_per_target as u16 - 1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "port range overflows"))?;
        let paths = path_addresses
            .iter()
            .enumerate()
            .map(|(path_id, address)| ZcFanoutTargetPath {
                path_id,
                address: address.clone(),
            })
            .collect::<Vec<_>>();
        if paths.len() > args.lanes_per_target {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "target {name} has {} paths but only {} lanes",
                    paths.len(),
                    args.lanes_per_target
                ),
            ));
        }
        let receiver_command = receiver_command(
            name,
            &paths,
            base_port,
            args.lanes_per_target,
            args.chunk_bytes,
            args.receive_cpu_list.as_deref(),
            &args.receiver_output_prefix,
            &args.encryption,
            args.disable_authentication,
        );
        targets.push(ZcFanoutTarget {
            target_id,
            name: name.clone(),
            paths: paths.clone(),
            base_port,
            port_end,
            receiver_command,
        });

        for target_lane in 0..args.lanes_per_target {
            let path_id = path_index(target_lane, args.lanes_per_target, paths.len());
            let local_path_id =
                path_index(target_lane, args.lanes_per_target, args.local_paths.len());
            let local = &args.local_paths[local_path_id];
            let remote = &paths[path_id];
            let global_lane = target_id * args.lanes_per_target + target_lane;
            lanes.push(ZcFanoutLane {
                global_lane,
                target_id,
                target_name: name.clone(),
                target_lane,
                path_id,
                local_address: local.address.clone(),
                remote_address: remote.address.clone(),
                cpu_list: local.cpu_list.clone(),
                numa_node: local.numa_node,
                port: path_local_port(base_port, target_lane, args.lanes_per_target, paths.len())?,
                first_chunk: (target_id + branch_count * target_lane) as u64,
                chunk_stride: (branch_count * args.lanes_per_target) as u64,
                chunk_bytes: args.chunk_bytes,
            });
        }
    }

    Ok(ZcFanoutPlan {
        version: 1,
        mode: "seekable-striped-fanout".to_string(),
        source: args.source,
        bytes: args.bytes,
        chunk_bytes: args.chunk_bytes,
        branch_count,
        lanes_per_target: args.lanes_per_target,
        total_lanes,
        local_paths: args.local_paths,
        targets,
        lanes,
        notes: vec![
            "Each lane owns chunks first_chunk + n * chunk_stride and reads them with pread/read_at from a seekable source or WAL descriptor extent table.".to_string(),
            "Use one local path per NIC/card. On c8gn.48xlarge, two paths should map card0 to CPUs 0-95 and card1 to CPUs 96-191.".to_string(),
            "This plan intentionally avoids stdin fanout; stdin would reintroduce a single producer bottleneck.".to_string(),
        ],
    })
}

fn fanout_send_lane(
    lane: ZcFanoutLane,
    lanes_per_target: usize,
    file: Arc<fs::File>,
    file_len: u64,
    token: Option<String>,
    encryption: crate::ZcTcpmuxEncryption,
    already_encrypted: bool,
) -> io::Result<ZcFanoutSendLaneResult> {
    let started = Instant::now();
    let chunk_bytes = usize::try_from(lane.chunk_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "zcfanplan lane chunk size exceeds usize",
        )
    })?;
    let affinity =
        crate::ZcTcpmuxAffinityConfig::from_cli(true, Some(&lane.cpu_list), "--local-path")?;
    let (mut stream, topology, nonce_base, cipher) = crate::zc_tcpmux_connect_parallel_sender(
        lane.target_lane,
        lanes_per_target,
        chunk_bytes,
        &lane.remote_address,
        lane.port,
        Some(&lane.local_address),
        token.as_deref(),
        encryption,
        already_encrypted,
        &affinity,
    )?;

    let mut offset = lane
        .first_chunk
        .checked_mul(lane.chunk_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "lane offset overflows"))?;
    let stride = lane
        .chunk_stride
        .checked_mul(lane.chunk_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "lane stride overflows"))?;
    let mut sequence = 0u64;
    let mut bytes = 0u64;
    let mut chunks = 0u64;
    let mut max_end = 0u64;

    while offset < file_len {
        let len = file_len.saturating_sub(offset).min(lane.chunk_bytes) as usize;
        let data = crate::zc_read_file_chunk_at(&file, offset, len)?;
        if data.is_empty() {
            break;
        }
        let (plaintext_len, end) = crate::zc_tcpmux_parallel_write_data_frame(
            &mut stream,
            cipher.as_ref(),
            lane.target_lane,
            topology.lane_id,
            &nonce_base,
            sequence,
            offset,
            data,
        )?;
        bytes = bytes.checked_add(plaintext_len as u64).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "fanout lane byte count overflow",
            )
        })?;
        chunks = chunks.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "fanout lane chunk count overflow",
            )
        })?;
        max_end = max_end.max(end);
        sequence = sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "fanout lane sequence overflow")
        })?;
        offset = offset.checked_add(stride).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "fanout lane offset overflow")
        })?;
    }
    crate::zc_tcpmux_parallel_write_eof(&mut stream)?;
    let seconds = started.elapsed().as_secs_f64();
    Ok(ZcFanoutSendLaneResult {
        global_lane: lane.global_lane,
        target_name: lane.target_name,
        target_lane: lane.target_lane,
        remote_address: lane.remote_address,
        port: lane.port,
        bytes,
        chunks,
        max_end,
        seconds,
        topology: topology.log_fields(),
    })
}

fn send_plan(args: FanoutPlanArgs) -> io::Result<ZcFanoutSendResult> {
    if args.source == "-" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zcfanplan send requires --source PATH",
        ));
    }
    let encryption = crate::ZcTcpmuxEncryption::parse(&args.encryption, "--encryption")?;
    if args.disable_authentication && args.token.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot combine --token and --disable-authentication",
        ));
    }
    if args.disable_authentication && encryption.requires_token() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--disable-authentication requires --encryption none",
        ));
    }
    if !args.disable_authentication && args.token.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zcfanplan send requires --token unless --disable-authentication is used",
        ));
    }
    if let Some(token) = args.token.as_deref() {
        crate::zc_tcpmux_validate_token(token)?;
    }

    let plan = build_plan(args.clone())?;
    let source = fs::File::open(&args.source).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("open zcfanplan source {}: {err}", args.source),
        )
    })?;
    let file_len = source.metadata()?.len();
    if args.bytes != file_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "zcfanplan --bytes {} does not match source length {file_len}",
                args.bytes
            ),
        ));
    }
    let source = Arc::new(source);
    let started = Instant::now();
    let mut handles = Vec::with_capacity(plan.lanes.len());
    for lane in plan.lanes.iter().cloned() {
        let file = Arc::clone(&source);
        let token = args.token.clone();
        let lanes_per_target = plan.lanes_per_target;
        handles.push(thread::spawn(move || {
            fanout_send_lane(
                lane,
                lanes_per_target,
                file,
                file_len,
                token,
                encryption,
                false,
            )
        }));
    }
    let mut lanes = Vec::with_capacity(handles.len());
    let mut total = 0u64;
    for (idx, handle) in handles.into_iter().enumerate() {
        let lane = handle
            .join()
            .map_err(|_| io::Error::other(format!("zcfanplan send lane {idx} panicked")))??;
        total = total.checked_add(lane.bytes).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "zcfanplan byte count overflow")
        })?;
        eprintln!(
            "zcfanplan-send-lane-result: global_lane={} target={} target_lane={} bytes={} chunks={} max_end={} seconds={:.6} remote={}:{} topology={}",
            lane.global_lane,
            lane.target_name,
            lane.target_lane,
            lane.bytes,
            lane.chunks,
            lane.max_end,
            lane.seconds,
            lane.remote_address,
            lane.port,
            lane.topology
        );
        lanes.push(lane);
    }
    lanes.sort_by_key(|lane| lane.global_lane);
    if total != file_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("zcfanplan sent {total} bytes but source length was {file_len}"),
        ));
    }
    let seconds = started.elapsed().as_secs_f64();
    let mibps = if seconds > 0.0 {
        total as f64 / 1024.0 / 1024.0 / seconds
    } else {
        0.0
    };
    let gbitps = if seconds > 0.0 {
        total as f64 * 8.0 / 1_000_000_000.0 / seconds
    } else {
        0.0
    };
    let logical_4k_iops = if seconds > 0.0 {
        total as f64 / 4096.0 / seconds
    } else {
        0.0
    };
    eprintln!(
        "zcfanplan-send-result: mode={} bytes={total} branches={} lanes_per_target={} total_lanes={} chunk_bytes={} seconds={seconds:.6} MiBps={mibps:.2} Gbitps={gbitps:.2} logical_4k_iops={logical_4k_iops:.0}",
        plan.mode, plan.branch_count, plan.lanes_per_target, plan.total_lanes, plan.chunk_bytes
    );
    Ok(ZcFanoutSendResult {
        mode: plan.mode,
        source: plan.source,
        bytes: total,
        chunk_bytes: plan.chunk_bytes,
        branch_count: plan.branch_count,
        lanes_per_target: plan.lanes_per_target,
        total_lanes: plan.total_lanes,
        seconds,
        mibps,
        gbitps,
        logical_4k_iops,
        lanes,
    })
}

fn print_usage() {
    println!(
        "zcfanplan - define a seekable multi-NIC fanout schedule\n\
         \n\
         zcfanplan --source PATH --bytes N --chunk-bytes 1m --lanes-per-target 128 \\\n\
         \t--local-path LOCAL0@0-95@0 --local-path LOCAL1@96-191@1 \\\n\
         \t--target n2=REMOTE0,REMOTE1 --target n3=REMOTE0,REMOTE1\n\
         zcfanplan send --source PATH --bytes N --chunk-bytes 1m --lanes-per-target 64 \\\n\
         \t--local-path LOCAL@0-95@0 --target n2=ADDR --target n3=ADDR --encryption none\n\
         \n\
         The output is JSON. Each lane owns a deterministic sparse extent stream:\n\
         chunk = first_chunk + k * chunk_stride. That is the structure needed for\n\
         fast RAID0 fanout without a single stdin splitter. The send subcommand\n\
         expects matching zc-tcpmux-receive processes to already be listening."
    );
}

pub fn cli(args: impl Iterator<Item = String>) -> io::Result<()> {
    let mut args_iter = args.peekable();
    if matches!(
        args_iter.peek().map(String::as_str),
        None | Some("help" | "--help" | "-h")
    ) {
        print_usage();
        return Ok(());
    }

    let mut parsed = FanoutPlanArgs::default();
    let mut send = false;
    if matches!(args_iter.peek().map(String::as_str), Some("plan")) {
        args_iter.next();
    } else if matches!(args_iter.peek().map(String::as_str), Some("send")) {
        args_iter.next();
        send = true;
    }
    while let Some(arg) = args_iter.next() {
        match arg.as_str() {
            "--source" | "--input" => parsed.source = next_arg(&mut args_iter, &arg)?,
            "--bytes" => parsed.bytes = parse_size_arg(&next_arg(&mut args_iter, &arg)?, &arg)?,
            "--chunk-bytes" | "--segment-bytes" => {
                parsed.chunk_bytes = parse_size_arg(&next_arg(&mut args_iter, &arg)?, &arg)?
            }
            "--lanes-per-target" | "--lanes-per-branch" => {
                parsed.lanes_per_target = parse_usize_value(&next_arg(&mut args_iter, &arg)?, &arg)?
            }
            "--base-port" => {
                parsed.base_port = parse_u16_value(&next_arg(&mut args_iter, &arg)?, &arg)?
            }
            "--port-stride" => {
                parsed.port_stride = Some(parse_u16_value(&next_arg(&mut args_iter, &arg)?, &arg)?)
            }
            "--local-path" => {
                let path_id = parsed.local_paths.len();
                parsed
                    .local_paths
                    .push(parse_local_path(&next_arg(&mut args_iter, &arg)?, path_id)?);
            }
            "--target" => parsed
                .targets
                .push(parse_target(&next_arg(&mut args_iter, &arg)?)?),
            "--receive-cpu-list" => parsed.receive_cpu_list = Some(next_arg(&mut args_iter, &arg)?),
            "--receiver-output-prefix" => {
                parsed.receiver_output_prefix = next_arg(&mut args_iter, &arg)?
            }
            "--encryption" => parsed.encryption = next_arg(&mut args_iter, &arg)?,
            "--token" => parsed.token = Some(next_arg(&mut args_iter, &arg)?),
            "--disable-authentication" => parsed.disable_authentication = true,
            "--require-authentication" => parsed.disable_authentication = false,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown zcfanplan option {other:?}"),
                ));
            }
        }
    }

    if send {
        let result = send_plan(parsed)?;
        serde_json::to_writer_pretty(io::stdout(), &result)
            .map_err(|err| io::Error::other(format!("write zcfanplan send JSON: {err}")))?;
    } else {
        let plan = build_plan(parsed)?;
        serde_json::to_writer_pretty(io::stdout(), &plan)
            .map_err(|err| io::Error::other(format!("write zcfanplan JSON: {err}")))?;
    }
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stripe_lanes_have_sparse_chunk_schedule() {
        let plan = build_plan(FanoutPlanArgs {
            source: "/dev/shm/x".to_string(),
            bytes: 8 * 1024 * 1024,
            chunk_bytes: 1024 * 1024,
            lanes_per_target: 2,
            base_port: 46000,
            port_stride: None,
            local_paths: vec![
                parse_local_path("10.0.0.1@0-95@0", 0).unwrap(),
                parse_local_path("10.0.1.1@96-191@1", 1).unwrap(),
            ],
            targets: vec![
                parse_target("n2=10.0.0.2,10.0.1.2").unwrap(),
                parse_target("n3=10.0.0.3,10.0.1.3").unwrap(),
            ],
            receive_cpu_list: Some("0-191".to_string()),
            receiver_output_prefix: "/dev/null".to_string(),
            encryption: "none".to_string(),
            token: None,
            disable_authentication: true,
        })
        .unwrap();
        assert_eq!(plan.branch_count, 2);
        assert_eq!(plan.total_lanes, 4);
        assert_eq!(plan.lanes[0].first_chunk, 0);
        assert_eq!(plan.lanes[1].first_chunk, 2);
        assert_eq!(plan.lanes[2].first_chunk, 1);
        assert_eq!(plan.lanes[3].first_chunk, 3);
        assert_eq!(plan.lanes[0].chunk_stride, 4);
        assert_eq!(plan.lanes[1].local_address, "10.0.1.1");
        assert_eq!(plan.lanes[0].port, 46000);
        assert_eq!(plan.lanes[1].port, 46000);
        assert_eq!(plan.lanes[2].port, 46002);
        assert_eq!(plan.lanes[3].port, 46002);
        assert!(
            plan.targets[0]
                .receiver_command
                .contains("--listen-addresses")
        );
    }
}
