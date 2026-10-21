//! Portable, bounded, TCP storage probe. This is not a kernel-block benchmark.
//! Can also build with rustc directly to isolate transport from optional crates.
#[path = "../block/zcnblk.rs"]
mod wire;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use wire::*;
const TIMEOUT: Duration = Duration::from_secs(45);
fn bad(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, s)
}
fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let s = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(TIMEOUT))?;
    s.set_write_timeout(Some(TIMEOUT))?;
    Ok(s)
}
fn frame(
    op: u16,
    id: u64,
    offset: u64,
    bytes: usize,
    lane: usize,
    lanes: usize,
) -> io::Result<ZcnblkFrameHeader> {
    ZcnblkFrameHeader::with_topology(
        op,
        0,
        0,
        bytes,
        offset,
        ZcnblkFrameTopology {
            lane_id: lane as u32,
            lane_count: lanes as u32,
            request_id: id,
            topology_flags: ZCNBLK_TOPOLOGY_VALID | ZCNBLK_TOPOLOGY_PORT_LANE,
            ..Default::default()
        },
    )
}
fn offset(index: u64, slots: u64, bytes: usize, lane: usize, random: bool) -> u64 {
    let n = if random {
        let mut x = index.wrapping_add(0x9e3779b97f4a7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
        x ^ (x >> 31)
    } else {
        index
    };
    (slots * lane as u64 + n % slots) * bytes as u64
}
fn pattern(data: &mut [u8], offset: u64) {
    for (i, p) in data.chunks_exact_mut(8).enumerate() {
        p.copy_from_slice(&((offset / 8).wrapping_add(i as u64) ^ 0x9e3779b97f4a7c15).to_le_bytes())
    }
}
struct Stats {
    elapsed: f64,
    latency_sum: f64,
    latency_max: f64,
    drain: f64,
    ops: u64,
}
#[derive(Clone)]
struct Config {
    addr: SocketAddr,
    write: bool,
    sync_each: bool,
    verify: bool,
    random: bool,
    ops: u64,
    workers: usize,
    qd: usize,
    bytes: usize,
    slots: u64,
}
fn worker(
    c: Config,
    lane: usize,
    barrier: Arc<Barrier>,
    mut reader: TcpStream,
    mut writer: TcpStream,
) -> io::Result<Stats> {
    let (credit_tx, credit_rx) = mpsc::sync_channel(c.qd);
    let (stamp_tx, stamp_rx) = mpsc::sync_channel(c.qd);
    for _ in 0..c.qd {
        credit_tx.send(()).map_err(io::Error::other)?;
    }
    barrier.wait();
    let start = Instant::now();
    let send = c.clone();
    let sender = thread::spawn(move || -> io::Result<()> {
        let result = (|| {
            let mut payload = vec![0; send.bytes];
            for index in 0..send.ops {
                credit_rx.recv_timeout(TIMEOUT).map_err(io::Error::other)?;
                let pos = offset(index, send.slots, send.bytes, lane, send.random);
                let f = frame(
                    if send.write {
                        ZCNBLK_OP_WRITE
                    } else {
                        ZCNBLK_OP_READ
                    },
                    index * (1 + u64::from(send.sync_each)) + 1,
                    pos,
                    send.bytes,
                    lane,
                    send.workers,
                )?;
                if send.write {
                    pattern(&mut payload, pos)
                }
                stamp_tx.send(Instant::now()).map_err(io::Error::other)?;
                writer.write_all(&f.encode())?;
                if send.write {
                    writer.write_all(&payload)?;
                }
                if send.sync_each {
                    writer.write_all(
                        &frame(ZCNBLK_OP_SYNC, index * 2 + 2, 0, 0, lane, send.workers)?.encode(),
                    )?;
                }
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = writer.shutdown(Shutdown::Both);
        }
        result
    });
    let result = (|| {
        let mut header = [0; ZCNBLK_FRAME_HEADER_LEN];
        let mut payload = vec![0; c.bytes];
        let mut expected_payload = vec![0; c.bytes];
        let mut sum = 0f64;
        let mut max = 0f64;
        for index in 0..c.ops {
            let stamp = stamp_rx.recv_timeout(TIMEOUT).map_err(io::Error::other)?;
            reader.read_exact(&mut header)?;
            let expected = frame(
                if c.write {
                    ZCNBLK_OP_WRITE_ACK
                } else {
                    ZCNBLK_OP_READ_RESP
                },
                index * (1 + u64::from(c.sync_each)) + 1,
                offset(index, c.slots, c.bytes, lane, c.random),
                c.bytes,
                lane,
                c.workers,
            )?;
            if header != expected.encode() {
                return Err(bad("response header/request/topology mismatch"));
            }
            if !c.write {
                reader.read_exact(&mut payload)?;
                if c.verify {
                    pattern(&mut expected_payload, expected.offset);
                    if payload != expected_payload {
                        return Err(bad("read data verification failed"));
                    }
                }
            }
            if c.sync_each {
                reader.read_exact(&mut header)?;
                let sync_expected =
                    frame(ZCNBLK_OP_SYNC_ACK, index * 2 + 2, 0, 0, lane, c.workers)?;
                if header != sync_expected.encode() {
                    return Err(bad("per-write SYNC acknowledgement mismatch"));
                }
            }
            let elapsed = stamp.elapsed().as_secs_f64();
            sum += elapsed;
            max = max.max(elapsed);
            // Returning credit after completion bounds aggregate outstanding depth.
            if index + 1 < c.ops {
                let _ = credit_tx.send(());
            }
        }
        let elapsed = start.elapsed().as_secs_f64();
        let sync = frame(
            ZCNBLK_OP_SYNC,
            c.ops * (1 + u64::from(c.sync_each)) + 1,
            0,
            0,
            lane,
            c.workers,
        )?;
        let drain = Instant::now();
        reader.write_all(&sync.encode())?;
        reader.read_exact(&mut header)?;
        let mut expected = sync;
        expected.op = ZCNBLK_OP_SYNC_ACK;
        if header != expected.encode() {
            return Err(bad("drain acknowledgement mismatch"));
        }
        Ok(Stats {
            elapsed,
            latency_sum: sum,
            latency_max: max,
            drain: drain.elapsed().as_secs_f64(),
            ops: c.ops,
        })
    })();
    if result.is_err() {
        let _ = reader.shutdown(Shutdown::Both);
    }
    drop(credit_tx);
    drop(stamp_rx);
    let sent = sender
        .join()
        .map_err(|_| io::Error::other("writer thread panicked"))?;
    match result {
        Err(e) => Err(e),
        Ok(stats) => {
            sent?;
            Ok(stats)
        }
    }
}
fn run() -> io::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 3 && args[1] == "echo-server" {
        let l = TcpListener::bind(&args[2])?;
        for s in l.incoming() {
            thread::spawn(move || -> io::Result<()> {
                let mut s = s?;
                s.set_nodelay(true)?;
                let mut b = [0; 64];
                loop {
                    if let Err(e) = s.read_exact(&mut b) {
                        if e.kind() == io::ErrorKind::UnexpectedEof {
                            return Ok(());
                        }
                        return Err(e);
                    }
                    s.write_all(&b)?;
                }
            });
        }
        return Ok(());
    }
    if args.len() == 4 && matches!(args[1].as_str(), "rtt" | "sync-rtt") {
        let mut s = connect(args[2].parse().map_err(io::Error::other)?)?;
        let n: usize = args[3].parse().map_err(io::Error::other)?;
        if !(3..=1_000_000).contains(&n) {
            return Err(bad("RTT needs at least 3 samples"));
        }
        let mut samples = Vec::with_capacity(n);
        let sync_probe = args[1] == "sync-rtt";
        let mut payload = if sync_probe {
            frame(ZCNBLK_OP_SYNC, 1, 0, 0, 0, 1)?.encode()
        } else {
            [0xa5; 64]
        };
        let mut expected = if sync_probe {
            frame(ZCNBLK_OP_SYNC_ACK, 1, 0, 0, 0, 1)?.encode()
        } else {
            payload
        };
        let mut b = [0; 64];
        let warmups = if sync_probe { 32 } else { 200 };
        for index in 0..warmups {
            if sync_probe {
                payload = frame(ZCNBLK_OP_SYNC, index + 1, 0, 0, 0, 1)?.encode();
                expected = frame(ZCNBLK_OP_SYNC_ACK, index + 1, 0, 0, 0, 1)?.encode();
            }
            s.write_all(&payload)?;
            s.read_exact(&mut b)?;
            if b != expected {
                return Err(bad("echo warmup mismatch"));
            }
        }
        for index in 0..n {
            if sync_probe {
                let id = warmups + index as u64 + 1;
                payload = frame(ZCNBLK_OP_SYNC, id, 0, 0, 0, 1)?.encode();
                expected = frame(ZCNBLK_OP_SYNC_ACK, id, 0, 0, 0, 1)?.encode();
            }
            let t = Instant::now();
            s.write_all(&payload)?;
            s.read_exact(&mut b)?;
            if b != expected {
                return Err(bad("echo mismatch"));
            }
            samples.push(t.elapsed().as_secs_f64() * 1e6)
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "{{\"kind\":{:?},\"samples\":{n},\"median_us\":{},\"min_us\":{},\"max_us\":{}}}",
            if sync_probe {
                "endpoint_sync_roundtrip"
            } else {
                "raw_tcp_echo_rtt"
            },
            samples[n / 2],
            samples[0],
            samples[n - 1]
        );
        return Ok(());
    }
    if args.len() != 12 && args.len() != 13 {
        return Err(bad(
            "usage: zcnblk-net-bench run ADDRESS read|read-verify|write|write-sync OPS_PER_WORKER WORKERS PER_WORKER_QD IO_BYTES CAPACITY_BYTES RAW_RTT_US sequential|random volatile-ram|file-write-ack|wal-append-ack|file-durable-sync|wal-durable-sync [MIN_SYNC_ROUNDTRIP_US (required for write-sync)]\nOther modes: echo-server IP:PORT; rtt ADDRESS SAMPLES; sync-rtt ADDRESS SAMPLES",
        ));
    }
    let parse = |i: usize| args[i].parse::<u64>().map_err(io::Error::other);
    let (ops, workers, qd, bytes, cap) = (parse(4)?, parse(5)?, parse(6)?, parse(7)?, parse(8)?);
    let rtt = args[9].parse::<f64>().map_err(io::Error::other)?;
    let sync_each = args[3] == "write-sync";
    let sync_reference = if sync_each && args.len() == 13 {
        args[12].parse::<f64>().map_err(io::Error::other)?
    } else {
        0.0
    };
    let durable_label = matches!(args[11].as_str(), "file-durable-sync" | "wal-durable-sync");
    if args[1] != "run"
        || !matches!(
            args[3].as_str(),
            "read" | "read-verify" | "write" | "write-sync"
        )
        || !matches!(args[10].as_str(), "sequential" | "random")
        || !matches!(
            args[11].as_str(),
            "volatile-ram"
                | "file-write-ack"
                | "wal-append-ack"
                | "file-durable-sync"
                | "wal-durable-sync"
        )
        || ops == 0
        || ops > u64::MAX / 128
        || (sync_each
            && (args.len() != 13
                || !sync_reference.is_finite()
                || sync_reference <= 0.0
                || (!durable_label && args[11] != "volatile-ram")))
        || (!sync_each && (args.len() != 12 || durable_label))
        || !(1..=64).contains(&workers)
        || !(1..=4096).contains(&qd)
        || bytes == 0
        || bytes % 4096 != 0
        || bytes > 16 * 1024 * 1024
        || cap % 4096 != 0
        || cap / bytes < workers
        || qd * workers * bytes > 1024 * 1024 * 1024
        || !rtt.is_finite()
        || rtt <= 0.0
    {
        return Err(bad("invalid or excessive benchmark dimensions"));
    }
    eprintln!(
        "TOPOLOGY WARNING: exploratory shared-system TCP userspace probe; NOT a representative high-IOPS block/mux benchmark. hugetlb=false memlock=not-reserved cpu_pinning=false hctx=not-applicable io_uring=false batching=one-frame-per-request; target topology must be recorded separately"
    );
    for lane in 0..workers {
        eprintln!(
            "lane={lane} worker={lane} sender_thread=dedicated receiver_thread=dedicated cpu=os-scheduled per_worker_qd={qd}"
        )
    }
    if ["URING_PLAY_TOPOLOGY_STRICT", "URING_PLAY_TOPOLOGY_FATAL"]
        .iter()
        .any(|k| std::env::var(k).as_deref() == Ok("1"))
    {
        return Err(bad(
            "strict topology: this portable probe has no CPU pinning or reserved fast-path resources",
        ));
    }
    let c = Config {
        addr: args[2].parse().map_err(io::Error::other)?,
        write: matches!(args[3].as_str(), "write" | "write-sync"),
        sync_each,
        verify: args[3] == "read-verify",
        random: args[10] == "random",
        ops,
        workers: workers as usize,
        qd: qd as usize,
        bytes: bytes as usize,
        slots: cap / bytes / workers,
    };
    // Establish and clone every socket before creating workers. A failed
    // connection therefore cannot strand another worker at the start barrier.
    let mut streams = Vec::new();
    for _ in 0..workers {
        let reader = connect(c.addr)?;
        let writer = reader.try_clone()?;
        streams.push((reader, writer));
    }
    let barrier = Arc::new(Barrier::new(workers as usize));
    let mut handles = vec![];
    for (lane, (reader, writer)) in streams.into_iter().enumerate() {
        let c = c.clone();
        let b = barrier.clone();
        handles.push(thread::spawn(move || worker(c, lane, b, reader, writer)));
    }
    let mut elapsed = 0f64;
    let mut sum = 0f64;
    let mut max = 0f64;
    let mut drain = 0f64;
    let mut completed = 0;
    for h in handles {
        let s = h
            .join()
            .map_err(|_| io::Error::other("worker panicked"))??;
        elapsed = elapsed.max(s.elapsed);
        sum += s.latency_sum;
        max = max.max(s.latency_max);
        drain = drain.max(s.drain);
        completed += s.ops;
    }
    let iops = completed as f64 / elapsed;
    let completion_reference_us = if sync_each {
        rtt.max(sync_reference)
    } else {
        rtt
    };
    let ceiling = (workers * qd) as f64 / (completion_reference_us / 1e6);
    let ceiling_model = if sync_each {
        "optimistic_same_endpoint_sync_roundtrip_floor"
    } else {
        "raw_tcp_echo_rtt_floor"
    };
    let completion_semantics = if sync_each {
        if durable_label {
            "remote_write_plus_durable_sync_ack"
        } else {
            "remote_write_plus_volatile_visibility_sync_ack"
        }
    } else if c.write {
        "remote_write_ack_not_durable"
    } else {
        "remote_read_response"
    };
    // A clean SYNC probe does not model dirty-media drain and WAL commit
    // service time. Keep its optimistic reference separate from a matching
    // theoretical completion ceiling; do not claim network-RTT efficiency.
    let matching_ceiling = if sync_each {
        "null".to_owned()
    } else {
        ceiling.to_string()
    };
    let matching_efficiency = if sync_each {
        "null".to_owned()
    } else {
        (iops / ceiling).to_string()
    };
    let sync_floor = if sync_each {
        ceiling.to_string()
    } else {
        "null".to_owned()
    };
    println!(
        "{{\"kind\":\"zcnblk_tcp_probe\",\"representative\":false,\"operation\":{:?},\"backend_semantics\":{:?},\"pattern\":{:?},\"workers\":{workers},\"lanes\":{workers},\"per_worker_qd\":{qd},\"aggregate_outstanding_limit\":{},\"cpu_mapping\":\"os-scheduled\",\"io_bytes\":{bytes},\"operations\":{completed},\"elapsed_s\":{elapsed},\"iops\":{iops},\"mib_per_s\":{},\"mean_completion_us\":{},\"max_completion_us\":{},\"raw_tcp_echo_rtt_us\":{rtt},\"rtt_iops_ceiling\":{matching_ceiling},\"optimistic_sync_reference_iops_ceiling\":{sync_floor},\"ceiling_model\":{ceiling_model:?},\"completion_reference_us\":{completion_reference_us},\"measured_sync_reference_us\":{sync_reference},\"completion_semantics\":{completion_semantics:?},\"aggregate_wire_request_limit\":{},\"actual_theoretical_efficiency\":{matching_efficiency},\"durable_ceiling_unavailable\":{sync_each},\"final_sync_max_us\":{},\"sync_included_in_iops\":{sync_each},\"final_sync_included_in_iops\":false}}",
        args[3],
        args[11],
        args[10],
        workers * qd,
        iops * bytes as f64 / 1048576.0,
        sum / completed as f64 * 1e6,
        max * 1e6,
        workers * qd * (1 + u64::from(sync_each)),
        drain * 1e6
    );
    Ok(())
}
fn main() -> io::Result<()> {
    run()
}
