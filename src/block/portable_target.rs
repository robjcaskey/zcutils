//! Portable terminal RAM/file leaf for the plain TCP zcnblk protocol.
//! Placement belongs in a separate userspace stage; this leaf owns one medium.
use super::client::MAX_IO_BYTES;
use super::zcnblk::*;
use crate::persistent_wal::{PersistentWalRuntime, MAX_APPEND_RECORDS};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
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
        let mut data = vec![0; frame.len as usize];
        if frame.op == ZCNBLK_OP_WRITE {
            stream.read_exact(&mut data)?;
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
                    file.read_exact(&mut data)?;
                }
                (Medium::File(file), ZCNBLK_OP_WRITE) => {
                    file.seek(SeekFrom::Start(frame.offset))?;
                    file.write_all(&data)?;
                }
                (Medium::File(file), ZCNBLK_OP_SYNC) => crate::persistent_wal::sync_backing(file)?,
                (Medium::Wal(wal), ZCNBLK_OP_READ) => wal.read_at(frame.offset, &mut data)?,
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
        stream.write_all(&frame.encode())?;
        if read {
            stream.write_all(&data)?;
        }
        Ok(())
    }

    fn connection(&self, mut stream: TcpStream) -> io::Result<()> {
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        loop {
            let mut header = [0; ZCNBLK_FRAME_HEADER_LEN];
            // Distinguish a clean disconnect from a truncated request.
            match stream.read(&mut header[..1])? {
                0 => return Ok(()),
                _ => (),
            }
            stream.read_exact(&mut header[1..])?;
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
}
