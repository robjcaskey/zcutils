//! Serial userspace mirror: client -> local terminal + downstream terminal.
//! The two upstream result lanes carry independent durable frontiers, not a
//! merged slowest-leg ACK. Payload is received once into an aligned arena and
//! borrowed by the local WAL and downstream sender until BOTH finish with it.

use crate::*;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub bind: String,
    pub data_base_port: u16,
    pub receipt_base_port: u16,
    pub remote: String,
    pub remote_base_port: u16,
    pub lanes: usize,
    pub extents_per_lane: usize,
    pub extent_bytes: usize,
    pub window: usize,
    pub terminal_target: String,
    pub queue_windows: usize,
    #[serde(default)]
    pub rdma: Option<rdma_custody::Config>,
}

/// The underlying legacy buffer type exposes raw pointers. This wrapper only
/// permits mutation under an exclusive Rust borrow, before sharing any lease.
pub(super) struct Arena(FixedSendBuffers, usize);
// SAFETY: mmap lifetime is owned by FixedSendBuffers; this wrapper exposes only
// immutable slices when shared. A returned pool credit follows dropping the
// downstream Arc, so get_mut cannot succeed while any consumer retains it.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}
impl Arena {
    pub(super) fn new(bytes: usize) -> io::Result<Self> {
        let storage = if env_enabled_or("URING_PLAY_HUGETLB", false) {
            FixedSendBuffers::new_hugetlb(1, bytes)?
        } else {
            FixedSendBuffers::new(1, bytes)?
        };
        Ok(Self(storage, bytes))
    }
    pub(super) fn bytes(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.0.ptr(0), self.1) }
    }
    pub(super) fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.0.ptr(0), self.1) }
    }
}

struct Forward {
    pool_slot: usize,
    payload: Payload,
    headers: Vec<ZcWalExtentHeader>,
}

enum Payload {
    Tcp(Arc<Arena>),
    Rdma {
        pool: Arc<rdma_custody::Pool>,
        sequence: usize,
        count: usize,
    },
}
impl Payload {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Tcp(arena) => arena.bytes(),
            // The upstream doorbell follows delivery completion. Both remote
            // receipts are withheld until local and forwarding consumers end.
            Self::Rdma {
                pool,
                sequence,
                count,
            } => unsafe { pool.received(*sequence, *count) },
        }
    }
}

pub(super) fn read_window(
    stream: &mut TcpStream,
    payload: &mut [u8],
    count: usize,
    extent_bytes: usize,
) -> io::Result<Vec<ZcWalExtentHeader>> {
    let mut headers = vec![[0u8; ZC_WAL_EXTENT_HEADER_LEN]; count];
    // Both control and data land directly in their final receive allocations.
    let mut iov = Vec::with_capacity(count * 2);
    for (header, data) in headers
        .iter_mut()
        .zip(payload.chunks_exact_mut(extent_bytes))
    {
        iov.push(std::io::IoSliceMut::new(header));
        iov.push(std::io::IoSliceMut::new(data));
    }
    let mut remaining = &mut iov[..];
    while !remaining.is_empty() {
        let count = remaining.len().min(1024);
        match stream.read_vectored(&mut remaining[..count]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(bytes) => std::io::IoSliceMut::advance_slices(&mut remaining, bytes),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    drop(iov);
    headers.iter().map(ZcWalExtentHeader::decode).collect()
}

pub(super) fn append_window(
    terminal: &ZcRaidMirrorTerminal,
    ring: &mut Option<RawRing>,
    headers: &[ZcWalExtentHeader],
    payload: &[u8],
    extent_bytes: usize,
) -> io::Result<()> {
    if let ZcnblkWalLeafBackend::PersistentJournal { store, .. } = terminal.backend.as_ref() {
        // PersistentWal already has scatter logical-page metadata. Batch the
        // metadata, not the payload: retain the original contiguous RX arena.
        let mut pages = Vec::with_capacity(payload.len() / ZC_WAL_RECORD_SIZE);
        for header in headers {
            terminal
                .backend
                .validate_range(header.base_wal_offset, extent_bytes)?;
            for page in 0..extent_bytes / ZC_WAL_RECORD_SIZE {
                pages.push(header.base_wal_offset / ZC_WAL_RECORD_SIZE as u64 + page as u64);
            }
        }
        let max_pages = persistent_wal::MAX_APPEND_RECORDS;
        for (index, chunk) in pages.chunks(max_pages).enumerate() {
            let start = index * max_pages * ZC_WAL_RECORD_SIZE;
            store.append_pages(
                chunk,
                &payload[start..start + chunk.len() * ZC_WAL_RECORD_SIZE],
            )?;
        }
    } else {
        for (header, data) in headers.iter().zip(payload.chunks_exact(extent_bytes)) {
            terminal.write(ring, *header, data)?;
        }
    }
    terminal.sync()
}

fn forward_worker(
    lane: usize,
    config: Config,
    mut remote: TcpStream,
    mut receipts: TcpStream,
    jobs: mpsc::Receiver<Forward>,
    done: mpsc::SyncSender<usize>,
    rdma_pool: Option<Arc<rdma_custody::Pool>>,
) -> io::Result<u64> {
    maybe_pin_current_thread("zcraid-serial-forward", config.lanes + lane);
    let mut rdma = match (&config.rdma, rdma_pool) {
        (Some(rdma), Some(pool)) => Some(unsafe {
            rdma_custody::Sender::connect(
                &mut remote,
                rdma,
                lane,
                config.window.min(config.extents_per_lane),
                config.extent_bytes,
                pool.ptr(),
                pool.bytes,
                Some(pool.clone()),
            )?
        }),
        (None, None) => None,
        _ => return Err(io::Error::other("RDMA pool configuration mismatch")),
    };
    let mut forwarded = 0u64;
    while let Ok(mut job) = jobs.recv() {
        let count = job.headers.len();
        for header in &mut job.headers {
            header.shard_id = 1;
        }
        let encoded = job
            .headers
            .iter()
            .map(|header| header.encode())
            .collect::<Vec<_>>();
        if let Some(rdma) = rdma.as_mut() {
            unsafe {
                rdma.write_window(
                    job.headers[0].extent_sequence as usize,
                    count,
                    job.payload.bytes().as_ptr(),
                    false,
                )?;
            }
        }
        for first in (0..count).step_by(512) {
            let last = (first + 512).min(count);
            let mut iov = Vec::with_capacity((last - first) * 2);
            for (slot, header) in encoded.iter().enumerate().take(last).skip(first) {
                iov.push(IoSlice::new(header));
                if rdma.is_none() {
                    iov.push(IoSlice::new(
                        &job.payload.bytes()
                            [slot * config.extent_bytes..(slot + 1) * config.extent_bytes],
                    ));
                }
            }
            tcp_write_all_vectored(&mut remote, &mut iov, "serial mirror same-arena forward")?;
        }
        let mut acks = vec![0u8; count * ZC_WAL_ACK_HEADER_LEN];
        remote.read_exact(&mut acks)?;
        for (bytes, header) in acks.chunks_exact(ZC_WAL_ACK_HEADER_LEN).zip(&job.headers) {
            if zcwal_read_u32(bytes, 0x0c) != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "serial tail did not attest durable custody",
                ));
            }
            zcraid_mirror_verify_ack(
                ZcWalAckHeader::decode(bytes.try_into().unwrap())?,
                1,
                lane,
                config.lanes,
                header.extent_sequence as usize,
                config.extent_bytes,
                ZcRaidZlaneCoordMode::LaneOwner,
            )?;
        }
        forwarded += (count * config.extent_bytes) as u64;
        let slot = job.pool_slot;
        drop(job); // Release payload ownership BEFORE publishing reusable credit.
        if done.send(slot).is_err() {
            break;
        }
        // Publish the downstream durability/slot-reuse frontier only after
        // this forwarder no longer holds the source arena lease.
        receipts.write_all(&acks)?;
    }
    Ok(forwarded)
}

pub(super) fn lane(
    config: Config,
    lane: usize,
    data: TcpListener,
    receipts: TcpListener,
    terminal: ZcRaidMirrorTerminal,
) -> io::Result<()> {
    let affinity = maybe_pin_current_thread("zcraid-serial-ingress", lane);
    terminal.validate_persistent_receipts()?;
    let remote = tcp_bench_connect_with_source_ip(
        &config.remote,
        tcp_bench_port(config.remote_base_port, lane)?,
        None,
        None,
    )?;
    set_tcp_nodelay_from_env(&remote)?;
    set_tcp_bench_buffers(&remote);
    remote.set_read_timeout(Some(Duration::from_secs(30)))?;
    remote.set_write_timeout(Some(Duration::from_secs(30)))?;
    println!(
        "zcraid-serial-ready: lane={lane} ingress_cpu={} forward_cpu_slot={} data={} receipts={} remote={} payload_copies=0 placement_owner=userspace-raid",
        affinity.target_cpu,
        config.lanes + lane,
        data.local_addr()?,
        receipts.local_addr()?,
        remote.peer_addr()?
    );
    let (mut upstream, _) = data.accept()?;
    let (upstream_receipts, _) = receipts.accept()?;
    for stream in [&upstream, &upstream_receipts] {
        set_tcp_nodelay_from_env(stream)?;
        set_tcp_bench_buffers(stream);
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    }
    let window = config.window.min(config.extents_per_lane);
    let bytes = window
        .checked_mul(config.extent_bytes)
        .ok_or_else(|| io::Error::other("serial arena overflow"))?;
    let slots = config.queue_windows + 1;
    let rdma_rx = config
        .rdma
        .as_ref()
        .map(|rdma| {
            rdma_custody::Receiver::accept(
                &mut upstream,
                rdma,
                lane,
                rdma_custody::Pool::new(slots, window, config.extent_bytes)?,
            )
        })
        .transpose()?;
    let rdma_pool = rdma_rx.as_ref().map(|rx| rx.pool.clone());
    let mut pool = (0..if rdma_rx.is_some() { 0 } else { slots })
        .map(|_| Arena::new(bytes).map(Arc::new))
        .collect::<io::Result<Vec<_>>>()?;
    let mut free = (0..slots).rev().collect::<Vec<_>>();
    let (jobs_tx, jobs_rx) = mpsc::sync_channel(config.queue_windows);
    let (done_tx, done_rx) = mpsc::sync_channel(slots);
    let forward_config = config.clone();
    let forward = thread::spawn(move || {
        forward_worker(
            lane,
            forward_config,
            remote,
            upstream_receipts,
            jobs_rx,
            done_tx,
            rdma_pool,
        )
    });
    let result = (|| {
        let mut ring = terminal.ring()?;
        for first in (0..config.extents_per_lane).step_by(window) {
            let slot = match free.pop() {
                Some(slot) => slot,
                None => done_rx
                    .recv_timeout(Duration::from_secs(30))
                    .map_err(io::Error::other)?,
            };
            let count = window.min(config.extents_per_lane - first);
            let headers = if let Some(rx) = &rdma_rx {
                if rx.pool.slot(first) != slot {
                    return Err(io::Error::other("RDMA pool slot reused out of order"));
                }
                let mut wire = vec![0; count * ZC_WAL_EXTENT_HEADER_LEN];
                upstream.read_exact(&mut wire)?;
                wire.chunks_exact(ZC_WAL_EXTENT_HEADER_LEN)
                    .map(|bytes| ZcWalExtentHeader::decode(bytes.try_into().unwrap()))
                    .collect::<io::Result<Vec<_>>>()?
            } else {
                let arena = Arc::get_mut(&mut pool[slot]).ok_or_else(|| {
                    io::Error::other("serial payload reused before consumer completion")
                })?;
                read_window(
                    &mut upstream,
                    &mut arena.bytes_mut()[..count * config.extent_bytes],
                    count,
                    config.extent_bytes,
                )?
            };
            for (index, header) in headers.iter().enumerate() {
                zcraid_mirror_verify_extent_header(
                    *header,
                    lane,
                    config.lanes,
                    0,
                    first + index,
                    config.extent_bytes,
                    ZcRaidZlaneCoordMode::LaneOwner,
                )?;
                if header.flags & raid_mirror_client_wal::REQUIRE_DURABLE == 0 {
                    return Err(io::Error::other(
                        "serial custody requires an explicitly opted-in source",
                    ));
                }
            }
            // Only descriptors/Arc ownership cross this bounded per-lane queue.
            jobs_tx
                .send(Forward {
                    pool_slot: slot,
                    payload: if let Some(rx) = &rdma_rx {
                        Payload::Rdma {
                            pool: rx.pool.clone(),
                            sequence: first,
                            count,
                        }
                    } else {
                        Payload::Tcp(pool[slot].clone())
                    },
                    headers: headers.clone(),
                })
                .map_err(|_| {
                    io::Error::other("serial forwarder failed; no payload credit returned")
                })?;
            append_window(
                &terminal,
                &mut ring,
                &headers,
                if let Some(rx) = &rdma_rx {
                    unsafe { rx.pool.received(first, count) }
                } else {
                    &pool[slot].bytes()[..count * config.extent_bytes]
                },
                config.extent_bytes,
            )?;
            let mut acks = Vec::with_capacity(count * ZC_WAL_ACK_HEADER_LEN);
            for header in headers {
                acks.extend_from_slice(&raid_mirror_client_wal::encode_durable_ack(header)?);
            }
            upstream.write_all(&acks)?;
        }
        Ok(())
    })();
    drop(jobs_tx);
    // At most `slots` distinct credits can be outstanding, so this queue has
    // enough capacity to finish every queued job even after ingress stops.
    let forwarded = forward
        .join()
        .map_err(|_| io::Error::other("serial forwarder panicked"))??;
    drop(done_rx);
    result?;
    println!(
        "zcraid-serial-complete: lane={lane} forwarded_bytes={forwarded} payload_userspace_copy_bytes=0 pool_slots={slots} receipt_gates=independent-local-and-final payload_reuse_gate=both-consumers-finished"
    );
    Ok(())
}

pub(super) fn cli(mut args: impl Iterator<Item = String>) -> io::Result<()> {
    let path = args
        .next()
        .ok_or_else(|| io::Error::other("usage: zcraid-mirror-hop CONFIG.json"))?;
    if args.next().is_some() {
        return Err(io::Error::other("usage: zcraid-mirror-hop CONFIG.json"));
    }
    let config: Config = serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
    if config.lanes == 0
        || config.extents_per_lane == 0
        || config.window == 0
        || config.queue_windows == 0
        || config.queue_windows > 4096
        || config.extent_bytes == 0
        || config.extent_bytes % 4096 != 0
    {
        return Err(io::Error::other(
            "invalid serial mirror shape/credit limits",
        ));
    }
    let bytes = config
        .extents_per_lane
        .checked_mul(config.extent_bytes)
        .and_then(|bytes| bytes.checked_mul(config.lanes))
        .ok_or_else(|| io::Error::other("serial terminal size overflow"))?;
    let pool_bytes = config
        .lanes
        .checked_mul(config.queue_windows + 1)
        .and_then(|v| v.checked_mul(config.window.min(config.extents_per_lane)))
        .and_then(|v| v.checked_mul(config.extent_bytes))
        .ok_or_else(|| io::Error::other("serial aggregate pool size overflow"))?;
    raid_mirror_client_wal::topology_preflight(
        "zcraid-serial",
        config.lanes,
        config.lanes * 2,
        pool_bytes,
    )?;
    if let Some(rdma) = &config.rdma {
        zcofi_perf_contract_warnings(
            "zcraid-serial",
            &rdma.provider,
            config.lanes,
            2,
            config.extent_bytes,
            pool_bytes,
            rdma.domain.as_deref().is_some_and(|d| !d.trim().is_empty()),
        )?;
    }
    let terminal =
        ZcRaidMirrorTerminal::open(&config.terminal_target, config.extent_bytes, bytes as u64)?;
    terminal.validate_persistent_receipts()?;
    let mut workers = Vec::new();
    for index in 0..config.lanes {
        let data = TcpListener::bind((
            config.bind.as_str(),
            tcp_bench_port(config.data_base_port, index)?,
        ))?;
        let receipts = TcpListener::bind((
            config.bind.as_str(),
            tcp_bench_port(config.receipt_base_port, index)?,
        ))?;
        let cfg = config.clone();
        let terminal = terminal.clone();
        workers.push(thread::spawn(move || {
            lane(cfg, index, data, receipts, terminal)
        }));
    }
    for worker in workers {
        worker
            .join()
            .map_err(|_| io::Error::other("serial mirror worker panicked"))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_wal_commitment::{CommitScope, Policy, Replica};

    fn terminal(directory: &Path, name: &str, bytes: usize) -> ZcRaidMirrorTerminal {
        let store = persistent_wal::PersistentWalRuntime::open_with_integrity(
            directory.join(format!("{name}.journal")),
            directory.join(format!("{name}.base")),
            bytes as u64,
            8 * 1024 * 1024,
            persistent_wal::IntegrityMode::Frame,
        )
        .unwrap();
        ZcRaidMirrorTerminal {
            backend: Arc::new(ZcnblkWalLeafBackend::PersistentJournal {
                label: name.into(),
                store,
                device_bytes: bytes as u64,
            }),
            io_mode: ZcnblkWalLeafIoMode::Blocking,
            allow_volatile_sync: false,
        }
    }

    #[test]
    fn serial_tcp_commits_middle_without_waiting_for_tail_and_preserves_payload() {
        serial_test(None, 16, 1);
    }

    #[cfg(zc_has_libfabric)]
    #[test]
    fn serial_rma_slots_survive_early_ack_and_wrap() {
        // Software libfabric RMA is a protocol/lifetime check, not EFA
        // performance evidence. The cloud run must use the real EFA provider.
        for window in [1, 4, 16] {
            serial_test(
                Some(rdma_custody::Config {
                    provider: "sockets".into(),
                    domain: Some("lo".into()),
                }),
                16 * window,
                window,
            );
        }
    }

    fn serial_test(rdma: Option<rdma_custody::Config>, total: usize, window: usize) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = env::var_os("ZC_CLIENT_WAL_TEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/tmp"));
        let path = root.join(format!(
            "zc-serial-custody-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let data = TcpListener::bind("127.0.0.1:0").unwrap();
        let receipts = TcpListener::bind("127.0.0.1:0").unwrap();
        let tail = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = Config {
            bind: "127.0.0.1".into(),
            data_base_port: data.local_addr().unwrap().port(),
            receipt_base_port: receipts.local_addr().unwrap().port(),
            remote: "127.0.0.1".into(),
            remote_base_port: tail.local_addr().unwrap().port(),
            lanes: 1,
            extents_per_lane: total,
            extent_bytes: 4096,
            window,
            terminal_target: String::new(),
            queue_windows: 8,
            rdma: rdma.clone(),
        };
        let middle_terminal = terminal(&path, "middle", total * 4096);
        let tail_terminal = terminal(&path, "tail", total * 4096);
        let tail_reader = tail_terminal.clone();
        let tail_rdma = rdma.clone();
        let (release_tx, release_rx) = mpsc::channel();
        let tail_worker = thread::spawn(move || {
            let (stream, peer_addr) = tail.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            zcraid_mirror_tcp_recv_worker_with_payload(
                0,
                1,
                vec![ZcRaidMirrorTcpLane {
                    lane: 0,
                    port: tail.local_addr().unwrap().port(),
                    peer_addr,
                    stream,
                }],
                1,
                total,
                4096,
                true,
                window,
                ZcRaidZlaneCoordMode::LaneOwner,
                Some(0),
                Some(tail_terminal),
                tail_rdma,
                true,
            )
        });
        let middle_reader = middle_terminal.clone();
        let hop_config = config.clone();
        let middle = thread::spawn(move || lane(hop_config, 0, data, receipts, middle_terminal));
        let branches = Arc::new(
            [config.data_base_port, config.receipt_base_port]
                .into_iter()
                .enumerate()
                .map(|(branch, base_port)| ZcRaidMirrorBranchTarget {
                    plan: ZcRaidMirrorBranchPlan {
                        branch,
                        fabric_domain: None,
                        lanes: vec![0],
                        leader_cpus: vec![0],
                        workers: vec![0],
                    },
                    addr: "127.0.0.1".into(),
                    base_port,
                    source_ip: None,
                })
                .collect(),
        );
        let client_config = raid_mirror_client_wal::Config {
            policy: Policy {
                scope: CommitScope {
                    volume: 1,
                    log: 2,
                    writer_epoch: 3,
                    lane: 0,
                },
                count_client_local_wal: true,
                local: Replica {
                    id: 1,
                    incarnation: 1,
                    failure_domain: 1,
                },
                remote: [
                    Replica {
                        id: 2,
                        incarnation: 1,
                        failure_domain: 2,
                    },
                    Replica {
                        id: 3,
                        incarnation: 1,
                        failure_domain: 3,
                    },
                ],
            },
            journal_directory: path.join("client"),
            capacity_bytes_per_lane: 1024 * 1024,
            direct: true,
            serial_hop: true,
            repair: None,
            rdma,
        };
        let client = thread::spawn(move || {
            raid_mirror_client_wal::tcp_send_with_pattern(
                0,
                vec![0],
                branches,
                1,
                total,
                4096,
                window,
                client_config,
                SendPayloadPattern::Offset { seed: 0xa918_cbad },
            )
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut reached = false;
        while Instant::now() < deadline {
            if let ZcnblkWalLeafBackend::PersistentJournal { store, .. } =
                middle_reader.backend.as_ref()
            {
                // PersistentWal sequences are append batches, not 4K pages.
                if store.stats().durable_sequence >= 8 {
                    reached = true;
                    break;
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        release_tx.send(()).unwrap();
        let result = client.join().unwrap();
        let middle_result = middle.join().unwrap();
        let tail_result = tail_worker.join().unwrap();
        assert!(
            reached,
            "middle failed to durably admit eight windows while tail was gated: client={:?} middle={:?} tail={:?}",
            result.as_ref().err(),
            middle_result.as_ref().err(),
            tail_result.as_ref().err()
        );
        assert_eq!(result.unwrap().acks, total);
        middle_result.unwrap();
        tail_result.unwrap();
        let mut expected = vec![0; 4096];
        for offset in (0..total as u64 * 4096).step_by(4096) {
            SendPayloadPattern::Offset { seed: 0xa918_cbad }.fill_for_zcnblk(
                &mut expected,
                0,
                offset,
            );
            assert_eq!(
                middle_reader.backend.read_at(offset, 4096).unwrap(),
                expected
            );
            assert_eq!(tail_reader.backend.read_at(offset, 4096).unwrap(), expected);
        }
        drop(middle_reader);
        drop(tail_reader);
        fs::remove_dir_all(path).unwrap();
    }
}
