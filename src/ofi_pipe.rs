use super::{
    ZcOfiEndpoint, maybe_pin_current_thread, zcofi_client_exchange_peer, zcofi_control_port,
    zcofi_server_exchange_peer,
};
use std::env;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const PIPE_MAGIC: &[u8; 8] = b"ZCOFIP01";
const PIPE_VERSION: u16 = 1;
const PIPE_FLAG_EOF: u16 = 1;
const PIPE_HEADER_LEN: usize = 32;
const DEFAULT_FRAME_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PipeHeader {
    flags: u16,
    payload_len: u32,
    sequence: u64,
}

impl PipeHeader {
    fn encode(self) -> [u8; PIPE_HEADER_LEN] {
        let mut out = [0u8; PIPE_HEADER_LEN];
        out[..8].copy_from_slice(PIPE_MAGIC);
        out[8..10].copy_from_slice(&PIPE_VERSION.to_le_bytes());
        out[10..12].copy_from_slice(&self.flags.to_le_bytes());
        out[12..16].copy_from_slice(&self.payload_len.to_le_bytes());
        out[16..24].copy_from_slice(&self.sequence.to_le_bytes());
        out
    }

    fn decode(input: &[u8]) -> io::Result<Self> {
        if input.len() < PIPE_HEADER_LEN || &input[..8] != PIPE_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid OFI pipe frame header",
            ));
        }
        let version = u16::from_le_bytes(input[8..10].try_into().unwrap());
        if version != PIPE_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported OFI pipe version {version}"),
            ));
        }
        let flags = u16::from_le_bytes(input[10..12].try_into().unwrap());
        if flags & !PIPE_FLAG_EOF != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown OFI pipe flags {flags:#x}"),
            ));
        }
        let payload_len = u32::from_le_bytes(input[12..16].try_into().unwrap());
        let sequence = u64::from_le_bytes(input[16..24].try_into().unwrap());
        if flags & PIPE_FLAG_EOF != 0 && payload_len != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "OFI pipe EOF frame carried a payload",
            ));
        }
        Ok(Self {
            flags,
            payload_len,
            sequence,
        })
    }
}

#[derive(Clone)]
struct DirectionConfig {
    label: &'static str,
    provider: Arc<String>,
    endpoint: Arc<String>,
    ofi_node: Arc<String>,
    service: u16,
    server: bool,
    frame_bytes: usize,
    cpu_index: usize,
}

fn parse_usize_env(name: &str, default: usize) -> io::Result<usize> {
    match env::var(name) {
        Ok(value) => value.parse::<usize>().map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid {name}={value:?}: {err}"),
            )
        }),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(io::Error::new(io::ErrorKind::InvalidInput, err)),
    }
}

fn open_direction(config: &DirectionConfig) -> io::Result<ZcOfiEndpoint> {
    let service = config.service.to_string();
    let mut endpoint = ZcOfiEndpoint::open(
        config.provider.as_str(),
        config.endpoint.as_str(),
        config.ofi_node.as_str(),
        &service,
        config.server,
    )?;
    if endpoint.max_msg_size() != 0 && config.frame_bytes > endpoint.max_msg_size() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "OFI pipe frame_bytes={} exceeds provider max_msg_size={} direction={}",
                config.frame_bytes,
                endpoint.max_msg_size(),
                config.label
            ),
        ));
    }
    let control_port = zcofi_control_port(&service)?;
    if config.server {
        zcofi_server_exchange_peer(config.ofi_node.as_str(), control_port, &mut endpoint)?;
    } else {
        zcofi_client_exchange_peer(config.ofi_node.as_str(), control_port, &mut endpoint)?;
    }
    Ok(endpoint)
}

fn send_direction(mut stream: TcpStream, config: DirectionConfig) -> io::Result<()> {
    let affinity = maybe_pin_current_thread("zcwal-ofi-pipe-send", config.cpu_index);
    let mut endpoint = open_direction(&config)?;
    let payload_cap = config.frame_bytes - PIPE_HEADER_LEN;
    let mut payload = vec![0u8; payload_cap];
    let mut message = Vec::with_capacity(config.frame_bytes);
    let mut sequence = 0u64;
    let mut bytes = 0u64;
    let mut frames = 0u64;
    let started = Instant::now();
    loop {
        let got = stream.read(&mut payload)?;
        let eof = got == 0;
        let header = PipeHeader {
            flags: if eof { PIPE_FLAG_EOF } else { 0 },
            payload_len: u32::try_from(got).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "OFI pipe payload exceeds u32")
            })?,
            sequence,
        };
        message.clear();
        message.extend_from_slice(&header.encode());
        message.extend_from_slice(&payload[..got]);
        endpoint.send(&message)?;
        frames = frames.saturating_add(1);
        bytes = bytes.saturating_add(got as u64);
        sequence = sequence.saturating_add(1);
        if eof {
            break;
        }
    }
    eprintln!(
        "zcwal-ofi-pipe-direction: direction={} action=send service={} frames={} bytes={} seconds={:.6} target_cpu={} affinity_applied={}",
        config.label,
        config.service,
        frames,
        bytes,
        started.elapsed().as_secs_f64(),
        if affinity.target_cpu < 0 {
            "none".to_string()
        } else {
            affinity.target_cpu.to_string()
        },
        affinity.applied,
    );
    Ok(())
}

fn recv_direction(mut stream: TcpStream, config: DirectionConfig) -> io::Result<()> {
    let affinity = maybe_pin_current_thread("zcwal-ofi-pipe-recv", config.cpu_index);
    let mut endpoint = open_direction(&config)?;
    let mut message = vec![0u8; config.frame_bytes];
    let mut expected_sequence = 0u64;
    let mut bytes = 0u64;
    let mut frames = 0u64;
    let started = Instant::now();
    loop {
        let got = endpoint.recv(&mut message)?;
        if got < PIPE_HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("short OFI pipe frame: {got}"),
            ));
        }
        let header = PipeHeader::decode(&message[..PIPE_HEADER_LEN])?;
        if header.sequence != expected_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "OFI pipe sequence mismatch direction={} expected={} got={}",
                    config.label, expected_sequence, header.sequence
                ),
            ));
        }
        let payload_len = header.payload_len as usize;
        let expected_len = PIPE_HEADER_LEN.checked_add(payload_len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "OFI pipe frame length overflow")
        })?;
        if got != expected_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("OFI pipe frame length mismatch got={got} expected={expected_len}"),
            ));
        }
        frames = frames.saturating_add(1);
        expected_sequence = expected_sequence.saturating_add(1);
        if header.flags & PIPE_FLAG_EOF != 0 {
            stream.shutdown(Shutdown::Write)?;
            break;
        }
        stream.write_all(&message[PIPE_HEADER_LEN..expected_len])?;
        bytes = bytes.saturating_add(payload_len as u64);
    }
    eprintln!(
        "zcwal-ofi-pipe-direction: direction={} action=recv service={} frames={} bytes={} seconds={:.6} target_cpu={} affinity_applied={}",
        config.label,
        config.service,
        frames,
        bytes,
        started.elapsed().as_secs_f64(),
        if affinity.target_cpu < 0 {
            "none".to_string()
        } else {
            affinity.target_cpu.to_string()
        },
        affinity.applied,
    );
    Ok(())
}

fn connect_retry(address: SocketAddr) -> io::Result<TcpStream> {
    let timeout_ms = parse_usize_env("URING_PLAY_OFI_PIPE_LOCAL_CONNECT_TIMEOUT_MS", 30_000)?;
    let started = Instant::now();
    loop {
        match TcpStream::connect(address) {
            Ok(stream) => return Ok(stream),
            Err(err) if started.elapsed() < Duration::from_millis(timeout_ms as u64) => {
                let _ = err;
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err),
        }
    }
}

fn lane_address(base: SocketAddr, lane: usize) -> io::Result<SocketAddr> {
    let port = usize::from(base.port())
        .checked_add(lane)
        .and_then(|port| u16::try_from(port).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "local lane port overflow"))?;
    Ok(SocketAddr::new(base.ip(), port))
}

fn run_connected_lane(
    role: &str,
    lane: usize,
    local_address: SocketAddr,
    local_stream: TcpStream,
    ofi_node: &str,
    request_service: u16,
    provider: &str,
    endpoint: &str,
    frame_bytes: usize,
) -> io::Result<()> {
    let response_service = request_service.checked_add(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "OFI pipe response service overflow",
        )
    })?;
    local_stream.set_nodelay(true)?;
    let reverse_stream = local_stream.try_clone()?;
    let provider = Arc::new(provider.to_string());
    let endpoint = Arc::new(endpoint.to_string());
    let ofi_node = Arc::new(ofi_node.to_string());
    let server = role == "server";
    eprintln!(
        "zcwal-ofi-pipe-lane: role={role} lane={lane} local_address={local_address} ofi_node={ofi_node} provider={provider} endpoint={endpoint} request_service={request_service} response_service={response_service} frame_bytes={frame_bytes} placement_owner=external-userspace-stage block_client_placement=no"
    );
    let request = DirectionConfig {
        label: "request",
        provider: Arc::clone(&provider),
        endpoint: Arc::clone(&endpoint),
        ofi_node: Arc::clone(&ofi_node),
        service: request_service,
        server,
        frame_bytes,
        cpu_index: lane.saturating_mul(2),
    };
    let response = DirectionConfig {
        label: "response",
        provider,
        endpoint,
        ofi_node,
        service: response_service,
        server,
        frame_bytes,
        cpu_index: lane.saturating_mul(2).saturating_add(1),
    };
    let (send_config, recv_config) = if role == "client" {
        (request, response)
    } else {
        (response, request)
    };
    let send = thread::spawn(move || send_direction(local_stream, send_config));
    let recv = thread::spawn(move || recv_direction(reverse_stream, recv_config));
    send.join()
        .map_err(|_| io::Error::other("OFI pipe send thread panicked"))??;
    recv.join()
        .map_err(|_| io::Error::other("OFI pipe receive thread panicked"))??;
    Ok(())
}

fn run_pipe(
    role: &str,
    local_address: &str,
    ofi_node: &str,
    base_service: u16,
    provider: &str,
    endpoint: &str,
    lanes: usize,
) -> io::Result<()> {
    if !matches!(role, "client" | "server") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "OFI pipe role must be client or server",
        ));
    }
    if lanes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "OFI pipe lane count must be positive",
        ));
    }
    let local_base = local_address.parse::<SocketAddr>().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid local socket address {local_address:?}: {err}"),
        )
    })?;
    let frame_bytes = parse_usize_env("URING_PLAY_OFI_PIPE_FRAME_BYTES", DEFAULT_FRAME_BYTES)?;
    if frame_bytes <= PIPE_HEADER_LEN || frame_bytes > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "OFI pipe frame bytes must be in {}..={}: {frame_bytes}",
                PIPE_HEADER_LEN + 1,
                u32::MAX
            ),
        ));
    }
    let last_service = usize::from(base_service)
        .checked_add(lanes.saturating_mul(2).saturating_sub(1))
        .and_then(|service| u16::try_from(service).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "OFI lane service overflow"))?;
    let _ = lane_address(local_base, lanes - 1)?;
    eprintln!(
        "zcwal-ofi-pipe-topology: role={role} lanes={lanes} local_base={local_base} ofi_node={ofi_node} service_range={base_service}-{last_service} worker_mapping=lane_N:cpu_indexes_2N,2N+1 aggregate_streams={lanes}"
    );

    let mut workers = Vec::with_capacity(lanes);
    if role == "client" {
        let mut listeners = Vec::with_capacity(lanes);
        for lane in 0..lanes {
            let address = lane_address(local_base, lane)?;
            listeners.push((lane, address, TcpListener::bind(address)?));
        }
        eprintln!("zcwal-ofi-pipe-local: role=client action=listening lanes={lanes}");
        for (lane, address, listener) in listeners {
            let ofi_node = ofi_node.to_string();
            let provider = provider.to_string();
            let endpoint = endpoint.to_string();
            let request_service = base_service + u16::try_from(lane * 2).unwrap();
            workers.push(thread::spawn(move || {
                let (stream, peer) = listener.accept()?;
                eprintln!("zcwal-ofi-pipe-local: role=client lane={lane} action=accept address={address} peer={peer}");
                run_connected_lane(
                    "client",
                    lane,
                    address,
                    stream,
                    &ofi_node,
                    request_service,
                    &provider,
                    &endpoint,
                    frame_bytes,
                )
            }));
        }
    } else {
        for lane in 0..lanes {
            let address = lane_address(local_base, lane)?;
            let ofi_node = ofi_node.to_string();
            let provider = provider.to_string();
            let endpoint = endpoint.to_string();
            let request_service = base_service + u16::try_from(lane * 2).unwrap();
            workers.push(thread::spawn(move || {
                let stream = connect_retry(address)?;
                eprintln!(
                    "zcwal-ofi-pipe-local: role=server lane={lane} action=connect address={address}"
                );
                run_connected_lane(
                    "server",
                    lane,
                    address,
                    stream,
                    &ofi_node,
                    request_service,
                    &provider,
                    &endpoint,
                    frame_bytes,
                )
            }));
        }
    }
    for worker in workers {
        worker
            .join()
            .map_err(|_| io::Error::other("OFI pipe lane thread panicked"))??;
    }
    Ok(())
}

pub fn cli(mut args: impl Iterator<Item = String>) -> io::Result<()> {
    let usage = "usage: zcwal-ofi-pipe <client|server> <local-listen-or-connect-base> <ofi-peer-or-bind> [base-service] [provider] [endpoint] [lanes]";
    let role = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, usage))?;
    let local_address = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, usage))?;
    let ofi_node = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, usage))?;
    let base_service = args
        .next()
        .unwrap_or_else(|| "37000".to_string())
        .parse::<u16>()
        .map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid base service: {err}"),
            )
        })?;
    let provider = args.next().unwrap_or_else(|| "efa".to_string());
    let endpoint = args.next().unwrap_or_else(|| "rdm".to_string());
    let lanes = args
        .next()
        .unwrap_or_else(|| "1".to_string())
        .parse::<usize>()
        .map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid lane count: {err}"),
            )
        })?;
    if args.next().is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, usage));
    }
    run_pipe(
        &role,
        &local_address,
        &ofi_node,
        base_service,
        &provider,
        &endpoint,
        lanes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_header_round_trips_data_and_eof() {
        for header in [
            PipeHeader {
                flags: 0,
                payload_len: 4096,
                sequence: 41,
            },
            PipeHeader {
                flags: PIPE_FLAG_EOF,
                payload_len: 0,
                sequence: 42,
            },
        ] {
            assert_eq!(PipeHeader::decode(&header.encode()).unwrap(), header);
        }
    }

    #[test]
    fn pipe_header_rejects_payload_on_eof() {
        let encoded = PipeHeader {
            flags: PIPE_FLAG_EOF,
            payload_len: 1,
            sequence: 0,
        }
        .encode();
        assert!(PipeHeader::decode(&encoded).is_err());
    }

    #[test]
    fn lane_address_increments_only_the_port() {
        let base = "127.0.0.1:28900".parse().unwrap();
        assert_eq!(
            lane_address(base, 31).unwrap().to_string(),
            "127.0.0.1:28931"
        );
        let v6 = "[::1]:28900".parse().unwrap();
        assert_eq!(lane_address(v6, 2).unwrap().to_string(), "[::1]:28902");
    }

    #[test]
    fn lane_address_rejects_port_overflow() {
        let base = "127.0.0.1:65535".parse().unwrap();
        assert!(lane_address(base, 1).is_err());
    }
}
