//! Userspace regional WAL quorum stage.
//!
//! This stage owns replicated placement after the `/dev/zcnblk0` edge.  It
//! sends each WAL operation to three independent userspace leaf writers and
//! admits a result only after two matching leaves reply.  Block devices may
//! appear only behind those terminal leaf writers.

use crate::wal_failover::{
    Endpoint, connect_leaf, is_write, read_frame, validate_mirror_results, write_frame,
};
use crate::{
    ZCNBLK_FAN_WAL_OP_EOF, ZCNBLK_FAN_WAL_OP_HELLO, ZCNBLK_FAN_WAL_OP_HELLO_ACK,
    ZCNBLK_FAN_WAL_OP_SYNC, ZcnblkFanWalFrame,
};
use std::env;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug)]
struct RegionalQuorumState {
    write_generation: AtomicU64,
    durable_generation: AtomicU64,
    placement_epoch: AtomicU64,
    leaf_failures: Mutex<Vec<u64>>,
}

impl RegionalQuorumState {
    fn new(leaves: usize) -> Self {
        Self {
            write_generation: AtomicU64::new(0),
            durable_generation: AtomicU64::new(0),
            placement_epoch: AtomicU64::new(1),
            leaf_failures: Mutex::new(vec![0; leaves]),
        }
    }

    fn note_failure(&self, leaf: usize, lane: u32, operation: u16, error: &io::Error) {
        let failures = self
            .leaf_failures
            .lock()
            .map(|mut failures| {
                failures[leaf] = failures[leaf].saturating_add(1);
                failures[leaf]
            })
            .unwrap_or(0);
        eprintln!(
            "zcnblk-wal-quorum-leaf-degraded: leaf={leaf} lane={lane} op={operation} failures={failures} error={error}"
        );
    }

    fn status(&self) -> String {
        let failures = self
            .leaf_failures
            .lock()
            .map(|values| {
                values
                    .iter()
                    .enumerate()
                    .map(|(leaf, count)| format!("{leaf}:{count}"))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_else(|_| "poisoned".into());
        format!(
            "placement_epoch={} write_generation={} durable_generation={} leaf_failures={failures}",
            self.placement_epoch.load(Ordering::Acquire),
            self.write_generation.load(Ordering::Acquire),
            self.durable_generation.load(Ordering::Acquire),
        )
    }
}

fn matching_quorum(
    results: &[(usize, (ZcnblkFanWalFrame, Vec<u8>))],
    quorum: usize,
) -> io::Result<(ZcnblkFanWalFrame, Vec<u8>)> {
    for (candidate_index, (_, candidate)) in results.iter().enumerate() {
        let matching = results
            .iter()
            .enumerate()
            .filter(|(index, (_, result))| {
                *index == candidate_index || validate_mirror_results(candidate, result).is_ok()
            })
            .count();
        if matching >= quorum {
            return Ok(candidate.clone());
        }
    }
    for left in 0..results.len() {
        for right in left + 1..results.len() {
            if let Err(error) = validate_mirror_results(&results[left].1, &results[right].1) {
                eprintln!(
                    "zcnblk-wal-quorum-result-mismatch: left_leaf={} right_leaf={} error={} left_frame={:?} right_frame={:?} left_payload_prefix={:02x?} right_payload_prefix={:02x?}",
                    results[left].0,
                    results[right].0,
                    error,
                    results[left].1.0,
                    results[right].1.0,
                    &results[left].1.1[..results[left].1.1.len().min(32)],
                    &results[right].1.1[..results[right].1.1.len().min(32)],
                );
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "regional WAL replicas have no matching result quorum: replies={} required={quorum}",
            results.len()
        ),
    ))
}

fn round_trip_quorum(
    streams: &mut [Option<TcpStream>],
    frame: ZcnblkFanWalFrame,
    payload: &[u8],
    quorum: usize,
    state: &RegionalQuorumState,
    lane: u32,
) -> io::Result<(ZcnblkFanWalFrame, Vec<u8>)> {
    let mut sent = Vec::new();
    for leaf in 0..streams.len() {
        let Some(stream) = streams[leaf].as_mut() else {
            continue;
        };
        let send_result = write_frame(stream, frame, payload);
        match send_result {
            Ok(()) => sent.push(leaf),
            Err(error) => {
                state.note_failure(leaf, lane, frame.op, &error);
                streams[leaf] = None;
            }
        }
    }
    if sent.len() < quorum {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            format!(
                "regional WAL write admission lost quorum: sent={} required={quorum}",
                sent.len()
            ),
        ));
    }

    let mut results = Vec::new();
    for leaf in sent {
        let Some(stream) = streams[leaf].as_mut() else {
            continue;
        };
        match read_frame(stream) {
            Ok(result) => results.push((leaf, result)),
            Err(error) => {
                state.note_failure(leaf, lane, frame.op, &error);
                streams[leaf] = None;
            }
        }
    }
    if results.len() < quorum {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            format!(
                "regional WAL result lost quorum: replies={} required={quorum}",
                results.len()
            ),
        ));
    }
    matching_quorum(&results, quorum)
}

fn proxy_session(
    mut upstream: TcpStream,
    endpoints: Arc<Vec<Endpoint>>,
    quorum: usize,
    state: Arc<RegionalQuorumState>,
    timeout: Duration,
    listener_lane: u32,
) -> io::Result<()> {
    upstream.set_nodelay(true)?;
    // Connect terminal leaves as soon as the upstream TCP session is
    // accepted, before waiting for HELLO.  Leaf writers admit their declared
    // connection topology before dispatching workers; eager connection avoids
    // a hierarchical HELLO deadlock when several quorum sessions share them.
    let lane = listener_lane;
    let mut streams = Vec::with_capacity(endpoints.len());
    for (leaf, endpoint) in endpoints.iter().enumerate() {
        match connect_leaf(endpoint, lane) {
            Ok(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                streams.push(Some(stream));
            }
            Err(error) => {
                state.note_failure(leaf, lane, ZCNBLK_FAN_WAL_OP_HELLO, &error);
                streams.push(None);
            }
        }
    }
    let (hello, payload) = read_frame(&mut upstream)?;
    if hello.op != ZCNBLK_FAN_WAL_OP_HELLO || !payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "regional quorum ingress requires HELLO as its first frame",
        ));
    }
    if hello.lane_id != listener_lane {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "regional quorum listener lane {listener_lane} received HELLO for lane {}",
                hello.lane_id
            ),
        ));
    }
    let hello_ack = round_trip_quorum(&mut streams, hello, &[], quorum, &state, lane)?;
    if hello_ack.0.op != ZCNBLK_FAN_WAL_OP_HELLO_ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "regional quorum leaves omitted HELLO_ACK",
        ));
    }
    write_frame(&mut upstream, hello_ack.0, &hello_ack.1)?;
    eprintln!(
        "zcnblk-wal-quorum-session: lane={lane} leaves={} quorum={quorum} completion=matching-2-of-3 {}",
        endpoints
            .iter()
            .map(|endpoint| endpoint.lane_addr(lane).map(|a| a.to_string()))
            .collect::<io::Result<Vec<_>>>()?
            .join(","),
        state.status(),
    );

    loop {
        let (frame, payload) = match read_frame(&mut upstream) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        if frame.op == ZCNBLK_FAN_WAL_OP_EOF {
            for stream in streams.iter_mut().flatten() {
                let _ = write_frame(stream, frame, &payload);
            }
            return Ok(());
        }
        let write = is_write(frame, &payload)?;
        let generation = if write {
            state.write_generation.fetch_add(1, Ordering::AcqRel) + 1
        } else {
            state.write_generation.load(Ordering::Acquire)
        };
        let result = round_trip_quorum(&mut streams, frame, &payload, quorum, &state, lane)?;
        if frame.op == ZCNBLK_FAN_WAL_OP_SYNC {
            state
                .durable_generation
                .fetch_max(generation, Ordering::AcqRel);
        }
        write_frame(&mut upstream, result.0, &result.1)?;
    }
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: zcnblk-wal-quorum LISTEN_BASE LEAF_BASE_A,LEAF_BASE_B,LEAF_BASE_C [LANES] [QUORUM]",
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
    let quorum = args
        .next()
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .unwrap_or(2);
    if endpoints.len() != 3 || quorum != 2 || lanes == 0 || args.next().is_some() {
        return Err(usage());
    }
    let timeout = Duration::from_millis(
        env::var("ZCNBLK_WAL_QUORUM_IO_TIMEOUT_MS")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
            .unwrap_or(500),
    );
    let endpoints = Arc::new(endpoints);
    let state = Arc::new(RegionalQuorumState::new(endpoints.len()));
    println!(
        "zcnblk-wal-quorum: listen={}:{} leaves={} replicas=3 quorum=2 placement_owner=userspace block_devices_are_terminal_only=true completion=matching-quorum timeout_ms={} guest_transport=tcp-unicast",
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
        let state = Arc::clone(&state);
        handles.push(
            thread::Builder::new()
                .name(format!("zcwal-quorum-{lane}"))
                .spawn(move || -> io::Result<()> {
                    for accepted in listener.incoming() {
                        let upstream = accepted?;
                        let endpoints = Arc::clone(&endpoints);
                        let state = Arc::clone(&state);
                        thread::Builder::new()
                            .name(format!("zcwal-quorum-session-{lane}"))
                            .spawn(move || {
                                if let Err(error) =
                                    proxy_session(upstream, endpoints, quorum, state, timeout, lane)
                                {
                                    eprintln!(
                                        "zcnblk-wal-quorum-session-error: lane={lane} error={error}"
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
            .map_err(|_| io::Error::other("regional quorum listener panicked"))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(payload: &[u8], preferred_worker: u32) -> (ZcnblkFanWalFrame, Vec<u8>) {
        (
            ZcnblkFanWalFrame {
                op: crate::ZCNBLK_FAN_WAL_OP_RESULT,
                payload_len: payload.len() as u32,
                topology_preferred_worker: preferred_worker,
                ..ZcnblkFanWalFrame::default()
            },
            payload.to_vec(),
        )
    }

    #[test]
    fn majority_selects_two_matching_payloads_even_when_first_diverges() {
        let replies = vec![
            (0, result(b"wrong", 0)),
            (1, result(b"right", 1)),
            (2, result(b"right", 2)),
        ];
        assert_eq!(matching_quorum(&replies, 2).unwrap().1, b"right");
    }

    #[test]
    fn two_disagreeing_replies_do_not_form_a_quorum() {
        let replies = vec![(0, result(b"left", 0)), (1, result(b"right", 1))];
        assert!(matching_quorum(&replies, 2).is_err());
    }

    #[test]
    fn embedded_leaf_local_cpu_hint_does_not_make_data_diverge() {
        let outer = ZcnblkFanWalFrame {
            op: crate::ZCNBLK_FAN_WAL_OP_RESULT_BATCH,
            segment_count: 1,
            payload_len: (crate::ZCNBLK_FAN_WAL_HEADER_LEN + 4) as u32,
            ..ZcnblkFanWalFrame::default()
        };
        let mut left_descriptor = ZcnblkFanWalFrame {
            op: crate::ZCNBLK_FAN_WAL_OP_RESULT,
            payload_len: 4,
            topology_preferred_cpu: 1,
            ..ZcnblkFanWalFrame::default()
        };
        let mut right_descriptor = left_descriptor;
        right_descriptor.topology_preferred_cpu = 3;
        let mut left = left_descriptor.encode().to_vec();
        let mut right = right_descriptor.encode().to_vec();
        left.extend_from_slice(b"data");
        right.extend_from_slice(b"data");
        let left_result = (outer, left);
        let right_result = (outer, right);
        assert!(
            validate_mirror_results(&left_result, &right_result).is_ok(),
            "leaf-local topology hints are not replica content"
        );
        left_descriptor.request_id = 99;
        let mut divergent = left_descriptor.encode().to_vec();
        divergent.extend_from_slice(b"data");
        assert!(validate_mirror_results(&(outer, divergent), &right_result).is_err());
    }
}
