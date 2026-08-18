use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;

const HEADER_LEN: usize = 128;
const MAGIC: &[u8; 8] = b"ZCFANW1\0";
const OP_HELLO: u16 = 1;
const OP_WRITE_DESC: u16 = 2;
const OP_READ_DESC: u16 = 3;
const OP_RESULT: u16 = 4;
const OP_SYNC: u16 = 5;
const OP_EOF: u16 = 6;
const OP_WRITE_BATCH: u16 = 7;
const OP_RESULT_BATCH: u16 = 8;
const OP_RESULT_RANGE_BATCH: u16 = 9;
const OP_REQUEST_BATCH: u16 = 10;
const OP_WRITE_EXTENT_BATCH: u16 = 11;
const OP_HELLO_ACK: u16 = 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Header {
    op: u16,
    payload_len: usize,
    sequence: u64,
    request_id: u64,
}

impl Header {
    fn decode(bytes: &[u8; HEADER_LEN]) -> io::Result<Self> {
        if &bytes[..8] != MAGIC || u16::from_le_bytes(bytes[10..12].try_into().unwrap()) != 128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid WAL frame header",
            ));
        }
        let op = u16::from_le_bytes(bytes[12..14].try_into().unwrap());
        if !matches!(
            op,
            OP_HELLO
                | OP_WRITE_DESC
                | OP_READ_DESC
                | OP_RESULT
                | OP_SYNC
                | OP_EOF
                | OP_WRITE_BATCH
                | OP_RESULT_BATCH
                | OP_RESULT_RANGE_BATCH
                | OP_REQUEST_BATCH
                | OP_WRITE_EXTENT_BATCH
                | OP_HELLO_ACK
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown WAL operation {op}"),
            ));
        }
        Ok(Self {
            op,
            payload_len: u32::from_le_bytes(bytes[44..48].try_into().unwrap()) as usize,
            sequence: u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
            request_id: u64::from_le_bytes(bytes[56..64].try_into().unwrap()),
        })
    }

    fn wire_payload_len(self) -> usize {
        match self.op {
            OP_WRITE_DESC
            | OP_RESULT
            | OP_WRITE_BATCH
            | OP_RESULT_BATCH
            | OP_REQUEST_BATCH
            | OP_WRITE_EXTENT_BATCH => self.payload_len,
            _ => 0,
        }
    }
}

#[derive(Debug)]
struct Frame {
    header_bytes: [u8; HEADER_LEN],
    header: Header,
    payload: Vec<u8>,
}

impl Frame {
    fn read(stream: &mut TcpStream) -> io::Result<Self> {
        let mut header_bytes = [0u8; HEADER_LEN];
        stream.read_exact(&mut header_bytes)?;
        let header = Header::decode(&header_bytes)?;
        let mut payload = vec![0u8; header.wire_payload_len()];
        stream.read_exact(&mut payload)?;
        Ok(Self {
            header_bytes,
            header,
            payload,
        })
    }

    fn write(&self, stream: &mut TcpStream) -> io::Result<()> {
        stream.write_all(&self.header_bytes)?;
        stream.write_all(&self.payload)
    }

    fn identical(&self, other: &Self) -> bool {
        self.header_bytes == other.header_bytes && self.payload == other.payload
    }
}

fn control_server(
    listen: &str,
    active: Arc<AtomicUsize>,
    generation: Arc<AtomicU64>,
    completed_hwm: Arc<AtomicU64>,
) -> io::Result<()> {
    let listener = TcpListener::bind(listen)?;
    eprintln!("ZCWAL_ROUTE_CONTROL_READY listen={listen}");
    for accepted in listener.incoming() {
        let mut stream = accepted?;
        let mut line = String::new();
        BufReader::new(stream.try_clone()?).read_line(&mut line)?;
        let command = line.trim();
        match command {
            "primary" | "secondary" => {
                let next = usize::from(command == "secondary");
                let previous = active.swap(next, Ordering::AcqRel);
                let next_generation = generation.fetch_add(1, Ordering::AcqRel) + 1;
                writeln!(
                    stream,
                    "OK active={} previous={} generation={} completed_hwm={}",
                    command,
                    if previous == 0 {
                        "primary"
                    } else {
                        "secondary"
                    },
                    next_generation,
                    completed_hwm.load(Ordering::Acquire)
                )?;
            }
            "status" => {
                writeln!(
                    stream,
                    "OK active={} generation={} completed_hwm={}",
                    if active.load(Ordering::Acquire) == 0 {
                        "primary"
                    } else {
                        "secondary"
                    },
                    generation.load(Ordering::Acquire),
                    completed_hwm.load(Ordering::Acquire)
                )?;
            }
            _ => writeln!(stream, "ERR expected primary, secondary, or status")?,
        }
    }
    Ok(())
}

fn run_data(
    listen: &str,
    primary_addr: &str,
    secondary_addr: &str,
    active: Arc<AtomicUsize>,
    completed_hwm: Arc<AtomicU64>,
) -> io::Result<()> {
    let listener = TcpListener::bind(listen)?;
    eprintln!(
        "ZCWAL_ROUTE_READY listen={listen} primary={primary_addr} secondary={secondary_addr} placement_owner=userspace-gateway block_client_placement=no"
    );
    let (mut client, peer) = listener.accept()?;
    client.set_nodelay(true)?;
    let mut primary = TcpStream::connect(primary_addr)?;
    let mut secondary = TcpStream::connect(secondary_addr)?;
    primary.set_nodelay(true)?;
    secondary.set_nodelay(true)?;
    let mut requests = 0u64;
    loop {
        let request = Frame::read(&mut client)?;
        request.write(&mut primary)?;
        request.write(&mut secondary)?;
        if request.header.op == OP_EOF {
            break;
        }
        let primary_result = Frame::read(&mut primary)?;
        let secondary_result = Frame::read(&mut secondary)?;
        if !primary_result.identical(&secondary_result) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "mirror results diverged primary={:?} secondary={:?}",
                    primary_result.header, secondary_result.header
                ),
            ));
        }
        if primary_result.header.request_id != request.header.request_id
            && request.header.op != OP_HELLO
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "leaf result request id does not match the request",
            ));
        }
        let selected = if active.load(Ordering::Acquire) == 0 {
            &primary_result
        } else {
            &secondary_result
        };
        selected.write(&mut client)?;
        requests += u64::from(request.header.op != OP_HELLO);
        completed_hwm.store(requests, Ordering::Release);
    }
    println!(
        "ZCWAL_ROUTE_PASS peer={peer} requests={requests} completed_hwm={} final_active={} client_connections=1 downstream_legs=2 cutover_boundary=dual-result-hwm",
        completed_hwm.load(Ordering::Acquire),
        if active.load(Ordering::Acquire) == 0 {
            "primary"
        } else {
            "secondary"
        }
    );
    Ok(())
}

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let listen = args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: zcwal-route-gateway LISTEN PRIMARY SECONDARY CONTROL_LISTEN",
        )
    })?;
    let primary = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing primary"))?;
    let secondary = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing secondary"))?;
    let control = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing control listen"))?;
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many arguments",
        ));
    }
    let active = Arc::new(AtomicUsize::new(0));
    let generation = Arc::new(AtomicU64::new(0));
    let completed_hwm = Arc::new(AtomicU64::new(0));
    let control_active = Arc::clone(&active);
    let control_generation = Arc::clone(&generation);
    let control_hwm = Arc::clone(&completed_hwm);
    thread::Builder::new()
        .name("zcwal-route-control".to_string())
        .spawn(move || {
            if let Err(error) =
                control_server(&control, control_active, control_generation, control_hwm)
            {
                eprintln!("zcwal route control failed: {error}");
            }
        })?;
    run_data(&listen, &primary, &secondary, active, completed_hwm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::mpsc;
    use std::time::Duration;

    fn free_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    fn test_frame(op: u16, sequence: u64, request_id: u64, payload: Vec<u8>) -> Frame {
        let mut header_bytes = [0u8; HEADER_LEN];
        header_bytes[..8].copy_from_slice(MAGIC);
        header_bytes[8..10].copy_from_slice(&5u16.to_le_bytes());
        header_bytes[10..12].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
        header_bytes[12..14].copy_from_slice(&op.to_le_bytes());
        header_bytes[44..48].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        header_bytes[48..56].copy_from_slice(&sequence.to_le_bytes());
        header_bytes[56..64].copy_from_slice(&request_id.to_le_bytes());
        Frame {
            header: Header::decode(&header_bytes).unwrap(),
            header_bytes,
            payload,
        }
    }

    fn fake_leaf(addr: SocketAddr, observed: mpsc::Sender<Vec<Vec<u8>>>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let listener = TcpListener::bind(addr).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut payloads = Vec::new();
            loop {
                let request = Frame::read(&mut stream).unwrap();
                match request.header.op {
                    OP_HELLO => test_frame(OP_HELLO_ACK, 0, 0, Vec::new())
                        .write(&mut stream)
                        .unwrap(),
                    OP_WRITE_DESC => {
                        payloads.push(request.payload);
                        test_frame(
                            OP_RESULT,
                            request.header.sequence,
                            request.header.request_id,
                            Vec::new(),
                        )
                        .write(&mut stream)
                        .unwrap();
                    }
                    OP_EOF => break,
                    other => panic!("unexpected fake-leaf operation {other}"),
                }
            }
            observed.send(payloads).unwrap();
        })
    }

    #[test]
    fn one_client_connection_survives_atomic_route_switch() {
        let primary_addr = free_addr();
        let secondary_addr = free_addr();
        let gateway_addr = free_addr();
        let (observed_tx, observed_rx) = mpsc::channel();
        let primary = fake_leaf(primary_addr, observed_tx.clone());
        let secondary = fake_leaf(secondary_addr, observed_tx);
        thread::sleep(Duration::from_millis(20));

        let active = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicU64::new(0));
        let gateway_active = Arc::clone(&active);
        let gateway_completed = Arc::clone(&completed);
        let gateway = thread::spawn(move || {
            run_data(
                &gateway_addr.to_string(),
                &primary_addr.to_string(),
                &secondary_addr.to_string(),
                gateway_active,
                gateway_completed,
            )
            .unwrap();
        });
        thread::sleep(Duration::from_millis(20));
        let mut client = TcpStream::connect(gateway_addr).unwrap();
        let client_local = client.local_addr().unwrap();
        test_frame(OP_HELLO, 0, 0, Vec::new())
            .write(&mut client)
            .unwrap();
        assert_eq!(Frame::read(&mut client).unwrap().header.op, OP_HELLO_ACK);

        let first = vec![0x11; 4096];
        test_frame(OP_WRITE_DESC, 0, 41, first.clone())
            .write(&mut client)
            .unwrap();
        assert_eq!(Frame::read(&mut client).unwrap().header.request_id, 41);
        active.store(1, Ordering::Release);
        let second = vec![0x22; 4096];
        test_frame(OP_WRITE_DESC, 1, 42, second.clone())
            .write(&mut client)
            .unwrap();
        assert_eq!(Frame::read(&mut client).unwrap().header.request_id, 42);
        assert_eq!(client.local_addr().unwrap(), client_local);
        test_frame(OP_EOF, 2, 0, Vec::new())
            .write(&mut client)
            .unwrap();
        drop(client);

        gateway.join().unwrap();
        primary.join().unwrap();
        secondary.join().unwrap();
        assert_eq!(completed.load(Ordering::Acquire), 2);
        for payloads in [observed_rx.recv().unwrap(), observed_rx.recv().unwrap()] {
            assert_eq!(payloads, vec![first.clone(), second.clone()]);
        }
    }
}
