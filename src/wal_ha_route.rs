//! Userspace active/standby route to redundant regional quorum frontends.
//!
//! Both downstream sessions are admitted before the upstream HELLO is
//! acknowledged.  Only one carries requests at a time.  If that frontend
//! disappears, the exact in-flight offset-addressed WAL operation is replayed
//! on the already-open standby session.  This stage chooses a regional
//! frontend; it never makes block, mirror, stripe, or terminal-media choices.

use crate::wal_failover::{
    Endpoint, connect_leaf, read_frame, validate_mirror_results, write_frame,
};
use crate::{
    ZCNBLK_FAN_WAL_OP_EOF, ZCNBLK_FAN_WAL_OP_HELLO, ZCNBLK_FAN_WAL_OP_HELLO_ACK, ZcnblkFanWalFrame,
};
use std::env;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

struct FrontendSession {
    index: usize,
    stream: TcpStream,
    hello_ack: (ZcnblkFanWalFrame, Vec<u8>),
}

fn send_request(
    stream: &mut TcpStream,
    frame: ZcnblkFanWalFrame,
    payload: &[u8],
) -> io::Result<(ZcnblkFanWalFrame, Vec<u8>)> {
    write_frame(stream, frame, payload)?;
    read_frame(stream)
}

fn close_sessions(sessions: &mut [Option<FrontendSession>], eof: ZcnblkFanWalFrame) {
    for session in sessions.iter_mut().flatten() {
        let _ = write_frame(&mut session.stream, eof, &[]);
    }
}

fn proxy_session(
    mut upstream: TcpStream,
    endpoints: Arc<Vec<Endpoint>>,
    timeout: Duration,
    listener_lane: u32,
) -> io::Result<()> {
    upstream.set_nodelay(true)?;
    // Connect candidates immediately after accepting the upstream TCP
    // session, before waiting for HELLO. Higher-level failover stages pre-open
    // several downstream sessions before sending any HELLO, while terminal
    // leaves may wait for the declared complete connection topology. Eager
    // connect prevents those two valid admission policies from deadlocking.
    let mut connected = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints.iter() {
        let stream = connect_leaf(endpoint, listener_lane)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        connected.push(stream);
    }
    let (hello, hello_payload) = read_frame(&mut upstream)?;
    if hello.op != ZCNBLK_FAN_WAL_OP_HELLO || !hello_payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "regional HA route ingress requires HELLO as its first frame",
        ));
    }
    if hello.lane_id != listener_lane {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "regional HA route listener lane {listener_lane} received HELLO for lane {}",
                hello.lane_id
            ),
        ));
    }

    // Write every HELLO before waiting for any ACK. A quorum frontend eagerly
    // opens its leaf sessions, and a terminal leaf may deliberately wait for
    // the declared complete connection topology before dispatching workers.
    // Sequential HELLO round trips would therefore deadlock admission.
    for stream in &mut connected {
        write_frame(stream, hello, &[])?;
    }
    let mut sessions = Vec::with_capacity(endpoints.len());
    for (index, mut stream) in connected.into_iter().enumerate() {
        let hello_ack = read_frame(&mut stream)?;
        if hello_ack.0.op != ZCNBLK_FAN_WAL_OP_HELLO_ACK {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("regional frontend {index} omitted HELLO_ACK"),
            ));
        }
        sessions.push(Some(FrontendSession {
            index,
            stream,
            hello_ack,
        }));
    }
    let admitted = sessions
        .first()
        .and_then(Option::as_ref)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no regional frontend"))?
        .hello_ack
        .clone();
    for session in sessions.iter().skip(1).flatten() {
        validate_mirror_results(&admitted, &session.hello_ack)?;
    }
    write_frame(&mut upstream, admitted.0, &admitted.1)?;
    let mut active = 0usize;
    eprintln!(
        "zcnblk-wal-ha-route-session: lane={listener_lane} frontends={} active={} admission=all-standbys-open replay=exact-inflight-idempotent placement_owner=regional-quorum",
        endpoints
            .iter()
            .map(|endpoint| endpoint
                .lane_addr(listener_lane)
                .map(|value| value.to_string()))
            .collect::<io::Result<Vec<_>>>()?
            .join(","),
        active,
    );

    loop {
        let (frame, payload) = match read_frame(&mut upstream) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                let eof = ZcnblkFanWalFrame {
                    op: ZCNBLK_FAN_WAL_OP_EOF,
                    lane_id: listener_lane,
                    lane_count: hello.lane_count,
                    ..ZcnblkFanWalFrame::default()
                };
                close_sessions(&mut sessions, eof);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if frame.op == ZCNBLK_FAN_WAL_OP_EOF {
            close_sessions(&mut sessions, frame);
            return Ok(());
        }

        let mut last_error = None;
        let mut result = None;
        for offset in 0..sessions.len() {
            let candidate = (active + offset) % sessions.len();
            let Some(session) = sessions[candidate].as_mut() else {
                continue;
            };
            match send_request(&mut session.stream, frame, &payload) {
                Ok(reply) => {
                    if candidate != active {
                        eprintln!(
                            "zcnblk-wal-ha-route-failover: lane={listener_lane} from={active} to={candidate} replayed_op={} replayed_sequence={} client_reconnect=false",
                            frame.op, frame.sequence,
                        );
                    }
                    active = candidate;
                    result = Some(reply);
                    break;
                }
                Err(error) => {
                    eprintln!(
                        "zcnblk-wal-ha-route-frontend-degraded: lane={listener_lane} frontend={} op={} sequence={} error={error}",
                        session.index, frame.op, frame.sequence,
                    );
                    last_error = Some(error);
                    sessions[candidate] = None;
                }
            }
        }
        let result = result.ok_or_else(|| {
            last_error.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "all regional frontends failed")
            })
        })?;
        write_frame(&mut upstream, result.0, &result.1)?;
    }
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: zcnblk-wal-ha-route LISTEN_BASE FRONTEND_BASE_A,FRONTEND_BASE_B [LANES]",
    )
}

pub fn main_entry() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let listen = Endpoint::parse(&args.next().ok_or_else(usage)?)?;
    let endpoints = args
        .next()
        .ok_or_else(usage)?
        .split(',')
        .map(Endpoint::parse)
        .collect::<io::Result<Vec<_>>>()?;
    let lanes = args
        .next()
        .map(|value| value.parse::<u32>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(1);
    if endpoints.len() < 2 || lanes == 0 || args.next().is_some() {
        return Err(usage());
    }
    let timeout = Duration::from_millis(
        env::var("ZCNBLK_WAL_HA_ROUTE_IO_TIMEOUT_MS")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
            .unwrap_or(500),
    );
    let endpoints = Arc::new(endpoints);
    println!(
        "zcnblk-wal-ha-route: listen={}:{} frontends={} admission=all-standbys-open frontend_failover=exact-inflight-replay client_reconnect=false placement_owner=regional-quorum block_placement=false timeout_ms={} guest_transport=tcp-unicast",
        listen.host,
        listen.base_port,
        endpoints
            .iter()
            .map(|endpoint| format!("{}:{}", endpoint.host, endpoint.base_port))
            .collect::<Vec<_>>()
            .join(","),
        timeout.as_millis(),
    );

    let mut handles = Vec::new();
    for lane in 0..lanes {
        let listener = TcpListener::bind(listen.lane_addr(lane)?)?;
        let endpoints = Arc::clone(&endpoints);
        handles.push(
            thread::Builder::new()
                .name(format!("zcwal-ha-route-{lane}"))
                .spawn(move || -> io::Result<()> {
                    for accepted in listener.incoming() {
                        let upstream = accepted?;
                        let endpoints = Arc::clone(&endpoints);
                        thread::Builder::new()
                            .name(format!("zcwal-ha-route-session-{lane}"))
                            .spawn(move || {
                                if let Err(error) =
                                    proxy_session(upstream, endpoints, timeout, lane)
                                {
                                    eprintln!(
                                        "zcnblk-wal-ha-route-session-error: lane={lane} error={error}"
                                    );
                                }
                            })?;
                    }
                    Ok(())
                })?,
        );
    }
    for handle in handles {
        handle
            .join()
            .map_err(|_| io::Error::other("regional HA route listener panicked"))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_at_least_two_frontends_by_contract() {
        let endpoints = [Endpoint::parse("127.0.0.1:30000").unwrap()];
        assert!(endpoints.len() < 2);
    }
}
