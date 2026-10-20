//! Blocking, placement-free client for the plain TCP zcnblk onramp.
//!
//! Write acknowledgements and SYNC acknowledgements are separate operations.
//! This client makes no durability claim beyond the endpoint's SYNC contract.
use super::zcnblk::*;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::Duration;

pub const MAX_IO_BYTES: usize = 16 * 1024 * 1024;
const BLOCK_BYTES: usize = 4096;

pub struct ZcnblkClient {
    stream: TcpStream,
    next_request_id: u64,
    poisoned: bool,
}

impl ZcnblkClient {
    /// Connect to one lane of a plain TCP endpoint. TLS/encrypted endpoints
    /// require their own transport and are not negotiated by this client.
    pub fn connect(address: SocketAddr, timeout: Duration) -> io::Result<Self> {
        let stream = TcpStream::connect_timeout(&address, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            next_request_id: 1,
            poisoned: false,
        })
    }

    pub fn read_at(&mut self, offset: u64, data: &mut [u8]) -> io::Result<()> {
        validate_range(offset, data.len())?;
        self.exchange(ZCNBLK_OP_READ, offset, &[], data.len(), data)
    }

    /// Return after a remote WRITE_ACK. Call sync for the endpoint's drain.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        validate_range(offset, data.len())?;
        self.exchange(ZCNBLK_OP_WRITE, offset, data, data.len(), &mut [])
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.exchange(ZCNBLK_OP_SYNC, 0, &[], 0, &mut [])
    }

    fn exchange(
        &mut self,
        op: u16,
        offset: u64,
        payload: &[u8],
        len: usize,
        output: &mut [u8],
    ) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "zcnblk connection failed; reconnect before retrying",
            ));
        }
        let request_id = self.next_request_id;
        self.next_request_id = request_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("request ID exhausted; reconnect"))?;
        let frame = ZcnblkFrameHeader::with_topology(
            op,
            0,
            0,
            len,
            offset,
            ZcnblkFrameTopology {
                lane_count: 1,
                request_id,
                topology_flags: ZCNBLK_TOPOLOGY_VALID | ZCNBLK_TOPOLOGY_PORT_LANE,
                ..Default::default()
            },
        )?;
        let result = (|| {
            self.stream.write_all(&frame.encode())?;
            self.stream.write_all(payload)?;
            let mut header = [0; ZCNBLK_FRAME_HEADER_LEN];
            self.stream.read_exact(&mut header)?;
            let response = ZcnblkFrameHeader::decode(&header)?;
            let mut expected = frame;
            expected.op = match op {
                ZCNBLK_OP_READ => ZCNBLK_OP_READ_RESP,
                ZCNBLK_OP_WRITE => ZCNBLK_OP_WRITE_ACK,
                ZCNBLK_OP_SYNC => ZCNBLK_OP_SYNC_ACK,
                _ => unreachable!(),
            };
            if response.encode() != expected.encode() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "zcnblk response does not match the request",
                ));
            }
            self.stream.read_exact(output)
        })();
        if result.is_err() {
            self.poisoned = true;
            let _ = self.stream.shutdown(Shutdown::Both);
        }
        result
    }
}

fn validate_range(offset: u64, len: usize) -> io::Result<()> {
    if len == 0
        || len > MAX_IO_BYTES
        || len % BLOCK_BYTES != 0
        || offset % BLOCK_BYTES as u64 != 0
        || offset.checked_add(len as u64).is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "I/O must be 4096-byte aligned, nonempty, at most 16 MiB, and within u64 offsets",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn stale_ack_poisoned_connection_cannot_be_reused() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut header = [0; ZCNBLK_FRAME_HEADER_LEN];
            stream.read_exact(&mut header).unwrap();
            let mut frame = ZcnblkFrameHeader::decode(&header).unwrap();
            frame.op = ZCNBLK_OP_SYNC_ACK;
            frame.topology.request_id += 1;
            stream.write_all(&frame.encode()).unwrap();
            assert_eq!(stream.read(&mut header).unwrap(), 0);
        });
        let mut client = ZcnblkClient::connect(address, Duration::from_secs(5)).unwrap();
        assert_eq!(
            client.sync().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            client.sync().unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        server.join().unwrap();
    }

    #[test]
    fn rejects_unaligned_oversized_and_overflowing_ranges() {
        for (offset, len) in [
            (1, 4096),
            (0, 0),
            (0, 1),
            (0, MAX_IO_BYTES + 4096),
            (u64::MAX - 4095, 4096),
        ] {
            assert!(validate_range(offset, len).is_err());
        }
        assert!(validate_range(4096, 8192).is_ok());
    }
}
