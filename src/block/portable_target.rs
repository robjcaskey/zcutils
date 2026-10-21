//! Portable terminal RAM/file leaf for the plain TCP zcnblk protocol.
//! Placement belongs in a separate userspace stage; this leaf owns one medium.
use super::client::MAX_IO_BYTES;
use super::zcnblk::*;
use crate::persistent_wal::{MAX_APPEND_RECORDS, PersistentWalRuntime};
use std::fs::{self, File, OpenOptions};
use std::io::{self, IoSlice, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const MAX_BATCH: usize = 128;
const MAX_CONNECTIONS: usize = 64;

enum Medium {
    Ram(Vec<u8>),
    File(File),
    Wal(PersistentWalRuntime),
}

pub struct Target {
    medium: Mutex<Medium>,
    capacity: u64,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

// Keep one response's header and payload in the same socket write when
// possible. Handle partial vectored writes without repeating framing bytes.
fn write_read_response(writer: &mut impl Write, header: &[u8], payload: &[u8]) -> io::Result<()> {
    let mut slices = [IoSlice::new(header), IoSlice::new(payload)];
    let mut pending = &mut slices[..];
    while !pending.is_empty() {
        match writer.write_vectored(pending) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "read response write made no progress",
                ));
            }
            Ok(written) => IoSlice::advance_slices(&mut pending, written),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

impl Target {
    /// The same durable WAL/reducer as Linux, with platform file admission.
    /// Placement stays in userspace; the native kernel edge never opens media.
    pub fn wal(journal: &Path, base: &Path, bytes: u64, journal_bytes: u64) -> io::Result<Self> {
        validate_capacity(bytes)?;
        if journal_bytes < ((MAX_APPEND_RECORDS + 3) * 4096) as u64 {
            return Err(invalid(
                "portable WAL journal must hold at least one maximum frame (use >= 3 MiB)",
            ));
        }
        let wal = PersistentWalRuntime::open(journal, base, bytes, journal_bytes)?;
        wal.validate_persistent_backings()?;
        Ok(Self {
            medium: Mutex::new(Medium::Wal(wal)),
            capacity: bytes,
        })
    }

    pub fn ram(bytes: usize) -> io::Result<Self> {
        validate_capacity(bytes as u64)?;
        let mut data = Vec::new();
        data.try_reserve_exact(bytes).map_err(io::Error::other)?;
        data.resize(bytes, 0);
        Ok(Self {
            medium: Mutex::new(Medium::Ram(data)),
            capacity: bytes as u64,
        })
    }

    /// Create a new regular file or open an existing regular file of exactly
    /// the requested size. Existing media is never truncated or resized.
    pub fn file(path: &Path, bytes: u64, create: bool) -> io::Result<Self> {
        validate_capacity(bytes)?;
        if !create && !fs::symlink_metadata(path)?.file_type().is_file() {
            return Err(invalid("backing path must be an ordinary file"));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(create)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(invalid("backing handle must be an ordinary file"));
        }
        if create {
            file.set_len(bytes)?;
            crate::persistent_wal::sync_backing(&file)?;
        }
        if file.metadata()?.len() != bytes {
            return Err(invalid("backing file size differs from requested capacity"));
        }
        Ok(Self {
            medium: Mutex::new(Medium::File(file)),
            capacity: bytes,
        })
    }

    fn validate(&self, frame: ZcnblkFrameHeader) -> io::Result<()> {
        if frame.shard != 0 {
            return Err(invalid("terminal leaf supports only shard zero"));
        }
        if frame.op == ZCNBLK_OP_SYNC {
            return if frame.len == 0 {
                Ok(())
            } else {
                Err(invalid("SYNC has no payload"))
            };
        }
        if !matches!(frame.op, ZCNBLK_OP_READ | ZCNBLK_OP_WRITE) {
            return Err(invalid("unsupported request operation"));
        }
        let len = frame.len as usize;
        if len == 0
            || len > MAX_IO_BYTES
            || len % 4096 != 0
            || frame.offset % 4096 != 0
            || frame
                .offset
                .checked_add(len as u64)
                .is_none_or(|end| end > self.capacity)
        {
            return Err(invalid(
                "I/O is unaligned, oversized, or outside target capacity",
            ));
        }
        Ok(())
    }

    fn process(&self, stream: &mut TcpStream, mut frame: ZcnblkFrameHeader) -> io::Result<()> {
        self.validate(frame)?;
        // Keep payload storage local to this request. Retaining it across
        // requests regressed large reads in the native Mac measurements.
        let mut buffer = vec![0u8; frame.len as usize];
        let data = &mut buffer[..];
        if frame.op == ZCNBLK_OP_WRITE {
            stream.read_exact(data)?;
        }
        {
            let mut medium = self
                .medium
                .lock()
                .map_err(|_| io::Error::other("target medium lock poisoned"))?;
            match (&mut *medium, frame.op) {
                (Medium::Ram(memory), ZCNBLK_OP_READ) => data.copy_from_slice(
                    &memory[frame.offset as usize..frame.offset as usize + frame.len as usize],
                ),
                (Medium::Ram(memory), ZCNBLK_OP_WRITE) => memory
                    [frame.offset as usize..frame.offset as usize + data.len()]
                    .copy_from_slice(&data),
                (Medium::File(file), ZCNBLK_OP_READ) => {
                    file.seek(SeekFrom::Start(frame.offset))?;
                    file.read_exact(data)?;
                }
                (Medium::File(file), ZCNBLK_OP_WRITE) => {
                    file.seek(SeekFrom::Start(frame.offset))?;
                    file.write_all(&data)?;
                }
                (Medium::File(file), ZCNBLK_OP_SYNC) => crate::persistent_wal::sync_backing(file)?,
                (Medium::Wal(wal), ZCNBLK_OP_READ) => wal.read_at(frame.offset, data)?,
                (Medium::Wal(wal), ZCNBLK_OP_WRITE) => {
                    // Split only to respect the WAL's bounded page table.
                    // Full journals apply backpressure while the reducer drains.
                    for (index, chunk) in data.chunks(MAX_APPEND_RECORDS * 4096).enumerate() {
                        let offset = frame.offset + (index * MAX_APPEND_RECORDS * 4096) as u64;
                        let deadline = std::time::Instant::now() + Duration::from_secs(20);
                        loop {
                            match wal.append_contiguous(offset, chunk) {
                                Ok(_) => break,
                                Err(error)
                                    if error.kind() == io::ErrorKind::WouldBlock
                                        && std::time::Instant::now() < deadline =>
                                {
                                    wal.sync()?;
                                    thread::sleep(Duration::from_millis(1));
                                }
                                Err(error) => return Err(error),
                            }
                        }
                    }
                }
                (Medium::Wal(wal), ZCNBLK_OP_SYNC) => {
                    wal.sync()?;
                }
                (Medium::Ram(_), ZCNBLK_OP_SYNC) => (), // Visibility drain only; RAM remains volatile.
                _ => unreachable!(),
            }
        }
        let read = frame.op == ZCNBLK_OP_READ;
        frame.op = match frame.op {
            ZCNBLK_OP_READ => ZCNBLK_OP_READ_RESP,
            ZCNBLK_OP_WRITE => ZCNBLK_OP_WRITE_ACK,
            ZCNBLK_OP_SYNC => ZCNBLK_OP_SYNC_ACK,
            _ => unreachable!(),
        };
        let response = frame.encode();
        if read {
            write_read_response(stream, &response, data)?;
        } else {
            stream.write_all(&response)?;
        }
        Ok(())
    }

    fn connection(&self, mut stream: TcpStream) -> io::Result<()> {
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        loop {
            let mut header = [0; ZCNBLK_FRAME_HEADER_LEN];
            // Accept a complete header in one receive, while retaining clean
            // EOF versus truncated-header detection for fragmented TCP input.
            let received = stream.read(&mut header)?;
            if received == 0 {
                return Ok(());
            }
            stream.read_exact(&mut header[received..])?;
            let frame = ZcnblkFrameHeader::decode(&header)?;
            if frame.op != ZCNBLK_OP_BATCH {
                self.process(&mut stream, frame)?;
                continue;
            }
            if frame.len == 0 || frame.len as usize > MAX_BATCH {
                return Err(invalid("batch must contain 1..=128 requests"));
            }
            let mut frames = Vec::with_capacity(frame.len as usize);
            let mut bytes = 0usize;
            for _ in 0..frame.len {
                stream.read_exact(&mut header)?;
                let member = ZcnblkFrameHeader::decode(&header)?;
                self.validate(member)?;
                if !matches!(member.op, ZCNBLK_OP_READ | ZCNBLK_OP_WRITE)
                    || frames
                        .first()
                        .is_some_and(|first: &ZcnblkFrameHeader| first.op != member.op)
                {
                    return Err(invalid("batch members must all be reads or all writes"));
                }
                bytes += member.len as usize;
                if bytes > MAX_IO_BYTES {
                    return Err(invalid("batch payload exceeds 16 MiB"));
                }
                frames.push(member);
            }
            for member in frames {
                self.process(&mut stream, member)?;
            }
        }
    }

    pub fn serve(self: Arc<Self>, listener: TcpListener) -> io::Result<()> {
        let connections = Arc::new(AtomicUsize::new(0));
        for stream in listener.incoming() {
            let stream = stream?;
            if connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                connections.fetch_sub(1, Ordering::AcqRel);
                drop(stream);
                continue;
            }
            let target = Arc::clone(&self);
            let count = Arc::clone(&connections);
            thread::spawn(move || {
                if let Err(error) = target.connection(stream) {
                    eprintln!("zcnblk-portable-target: connection closed: {error}");
                }
                count.fetch_sub(1, Ordering::AcqRel);
            });
        }
        Ok(())
    }
}

fn validate_capacity(bytes: u64) -> io::Result<()> {
    if bytes == 0 || bytes % 4096 != 0 {
        return Err(invalid("capacity must be a nonzero multiple of 4096"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::client::ZcnblkClient;
    use super::*;

    #[test]
    fn wal_roundtrip_drains_full_journal_and_recovers() {
        let root = std::env::var_os("ZCUTILS_TEST_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"))
            .join(format!(
                "zcutils-wal-leaf-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir_all(&root).unwrap();
        let expected = vec![0x3d; 4 * 1024 * 1024];
        for first in [true, false] {
            let target = Target::wal(
                &root.join("journal"),
                &root.join("base"),
                8 * 1024 * 1024,
                3 * 1024 * 1024,
            )
            .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = thread::spawn(move || target.connection(listener.accept().unwrap().0));
            let mut client = ZcnblkClient::connect(address, Duration::from_secs(25)).unwrap();
            if first {
                client.write_at(4096, &expected).unwrap();
                client.sync().unwrap();
            }
            let mut actual = vec![0; expected.len()];
            client.read_at(4096, &mut actual).unwrap();
            assert_eq!(actual, expected);
            drop(client);
            server.join().unwrap().unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ram_roundtrip_and_range_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let target = Target::ram(8192).unwrap();
        let server = thread::spawn(move || target.connection(listener.accept().unwrap().0));
        let mut client = ZcnblkClient::connect(address, Duration::from_secs(5)).unwrap();
        let expected = vec![0xa5; 4096];
        client.write_at(4096, &expected).unwrap();
        client.sync().unwrap();
        let mut data = vec![0; 4096];
        client.read_at(4096, &mut data).unwrap();
        assert_eq!(data, expected);
        assert!(client.write_at(8192, &expected).is_err());
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn file_survives_sync_close_and_reopen_without_overwrite() {
        let path = std::env::temp_dir().join(format!(
            "zcutils-portable-leaf-{}-{}.img",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let expected = vec![0x5a; 4096];
        for create in [true, false] {
            let target = Target::file(&path, 8192, create).unwrap();
            assert!(Target::file(&path, 8192, true).is_err());
            assert!(Target::file(&path, 16384, false).is_err());
            assert_eq!(fs::metadata(&path).unwrap().len(), 8192);
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = thread::spawn(move || target.connection(listener.accept().unwrap().0));
            let mut client = ZcnblkClient::connect(address, Duration::from_secs(5)).unwrap();
            if create {
                client.write_at(4096, &expected).unwrap();
                client.sync().unwrap();
            }
            let mut actual = vec![0; 4096];
            client.read_at(4096, &mut actual).unwrap();
            assert_eq!(actual, expected);
            drop(client);
            server.join().unwrap().unwrap();
        }
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn fragmented_header_and_truncated_header_keep_stream_boundaries() {
        for truncated in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let target = Target::ram(8192).unwrap();
            let server = thread::spawn(move || target.connection(listener.accept().unwrap().0));
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let header = ZcnblkFrameHeader::with_flags(ZCNBLK_OP_READ, 0, 0, 4096, 0)
                .unwrap()
                .encode();
            stream.write_all(&header[..7]).unwrap();
            if truncated {
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                assert_eq!(
                    server.join().unwrap().unwrap_err().kind(),
                    io::ErrorKind::UnexpectedEof
                );
            } else {
                stream.write_all(&header[7..]).unwrap();
                let mut response = [0; 64];
                stream.read_exact(&mut response).unwrap();
                assert_eq!(
                    ZcnblkFrameHeader::decode(&response).unwrap().op,
                    ZCNBLK_OP_READ_RESP
                );
                let mut payload = [0xff; 4096];
                stream.read_exact(&mut payload).unwrap();
                assert_eq!(payload, [0; 4096]);
                drop(stream);
                server.join().unwrap().unwrap();
            }
        }
    }

    #[test]
    fn request_payloads_do_not_leak_between_sizes_or_sync() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let target = Target::ram(65536).unwrap();
        let server = thread::spawn(move || target.connection(listener.accept().unwrap().0));
        let mut client = ZcnblkClient::connect(address, Duration::from_secs(5)).unwrap();
        client.write_at(0, &vec![0xa5; 32768]).unwrap();
        client.sync().unwrap();
        client.write_at(0, &[0x31; 4096]).unwrap();
        client.sync().unwrap();
        let mut small = [0; 4096];
        client.read_at(0, &mut small).unwrap();
        assert_eq!(small, [0x31; 4096]);
        client.read_at(32768, &mut small).unwrap();
        assert_eq!(small, [0; 4096]);
        let mut large = vec![0; 32768];
        client.read_at(0, &mut large).unwrap();
        assert!(large[..4096].iter().all(|&v| v == 0x31));
        assert!(large[4096..].iter().all(|&v| v == 0xa5));
        drop(client);
        server.join().unwrap().unwrap();
    }
    #[test]
    fn vectored_reply_handles_interruption_and_cross_slice_partial_writes() {
        struct PartialWriter {
            bytes: Vec<u8>,
            interrupted: bool,
        }
        impl Write for PartialWriter {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                self.write_vectored(&[IoSlice::new(data)])
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
            fn write_vectored(&mut self, slices: &[IoSlice<'_>]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                let mut written = 0;
                for slice in slices {
                    let count = slice.len().min(7 - written);
                    self.bytes.extend_from_slice(&slice[..count]);
                    written += count;
                    if written == 7 {
                        break;
                    }
                }
                Ok(written)
            }
        }
        let mut writer = PartialWriter {
            bytes: Vec::new(),
            interrupted: false,
        };
        write_read_response(&mut writer, b"hdr", b"payload123456789").unwrap();
        assert_eq!(writer.bytes, b"hdrpayload123456789");
    }

    #[test]
    fn vectored_reply_rejects_zero_progress() {
        struct ClosedWriter;
        impl Write for ClosedWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Ok(0)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            write_read_response(&mut ClosedWriter, b"hdr", b"payload")
                .unwrap_err()
                .kind(),
            io::ErrorKind::WriteZero
        );
    }
}
