use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const LOGSTREAM_MAGIC: &[u8; 8] = b"ZCLS0001";
const LOGSTREAM_HEADER_LEN: usize = 48;
const LOGSTREAM_MAX_PAYLOAD_BYTES: u64 = 16 * 1024 * 1024;
pub const ZCCUSAN_STATE_STREAM: &str = "zccusan.state";

pub trait DurableLogStream: Send + Sync {
    fn append_value(
        &self,
        stream: &str,
        kind: &str,
        key: &str,
        value: serde_json::Value,
    ) -> io::Result<ZccusanLogEntry>;

    fn replay(&self) -> io::Result<Vec<ZccusanLogEntry>>;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ZccusanLogEntry {
    pub schema_version: u16,
    pub stream: String,
    pub sequence: u64,
    pub term: u64,
    pub offset: u64,
    pub timestamp_secs: u64,
    pub kind: String,
    pub key: String,
    pub value: serde_json::Value,
}

#[derive(Debug)]
pub struct FileLogStream {
    path: PathBuf,
    state: Mutex<FileLogState>,
}

#[derive(Debug)]
struct FileLogState {
    file: File,
    next_sequence: u64,
}

impl FileLogStream {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)?;
        let recovery = read_entries(&mut file)?;
        if recovery.valid_len != file.metadata()?.len() {
            file.set_len(recovery.valid_len)?;
            file.sync_data()?;
        }
        file.seek(SeekFrom::End(0))?;

        Ok(Self {
            path,
            state: Mutex::new(FileLogState {
                file,
                next_sequence: recovery.next_sequence,
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append<T: Serialize>(
        &self,
        stream: &str,
        kind: &str,
        key: &str,
        value: &T,
    ) -> io::Result<ZccusanLogEntry> {
        let value = serde_json::to_value(value).map_err(invalid_data)?;
        self.append_value(stream, kind, key, value)
    }

    pub fn append_state<T: Serialize>(
        &self,
        kind: &str,
        key: &str,
        value: &T,
    ) -> io::Result<ZccusanLogEntry> {
        self.append(ZCCUSAN_STATE_STREAM, kind, key, value)
    }
}

impl DurableLogStream for FileLogStream {
    fn append_value(
        &self,
        stream: &str,
        kind: &str,
        key: &str,
        value: serde_json::Value,
    ) -> io::Result<ZccusanLogEntry> {
        validate_record_field(stream, "stream")?;
        validate_record_field(kind, "kind")?;
        validate_record_field(key, "key")?;

        let mut state = self.state.lock().expect("logstream mutex poisoned");
        let offset = state.file.metadata()?.len();
        let entry = ZccusanLogEntry {
            schema_version: 1,
            stream: stream.to_string(),
            sequence: state.next_sequence,
            term: 0,
            offset,
            timestamp_secs: unix_now_secs(),
            kind: kind.to_string(),
            key: key.to_string(),
            value,
        };
        let payload = serde_json::to_vec(&entry).map_err(invalid_data)?;
        if payload.len() as u64 > LOGSTREAM_MAX_PAYLOAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "logstream payload is too large",
            ));
        }

        let mut header = [0_u8; LOGSTREAM_HEADER_LEN];
        header[..LOGSTREAM_MAGIC.len()].copy_from_slice(LOGSTREAM_MAGIC);
        header[8..16].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        header[16..48].copy_from_slice(&Sha256::digest(&payload));

        let write_result = (|| -> io::Result<()> {
            state.file.write_all(&header)?;
            state.file.write_all(&payload)?;
            state.file.sync_data()
        })();
        if let Err(error) = write_result {
            let _ = state.file.set_len(offset);
            let _ = state.file.seek(SeekFrom::End(0));
            return Err(error);
        }
        state.next_sequence = state.next_sequence.saturating_add(1);
        Ok(entry)
    }

    fn replay(&self) -> io::Result<Vec<ZccusanLogEntry>> {
        let mut file = OpenOptions::new().read(true).open(&self.path)?;
        Ok(read_entries(&mut file)?.entries)
    }
}

#[derive(Debug)]
struct ReplayResult {
    entries: Vec<ZccusanLogEntry>,
    valid_len: u64,
    next_sequence: u64,
}

fn read_entries(file: &mut File) -> io::Result<ReplayResult> {
    file.seek(SeekFrom::Start(0))?;
    let mut entries = Vec::new();
    let mut valid_len = 0_u64;
    let mut next_sequence = 1_u64;

    loop {
        let frame_offset = valid_len;
        let mut header = [0_u8; LOGSTREAM_HEADER_LEN];
        match file.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        if &header[..LOGSTREAM_MAGIC.len()] != LOGSTREAM_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad logstream magic at offset {frame_offset}"),
            ));
        }

        let payload_len = u64::from_le_bytes(header[8..16].try_into().expect("u64 header length"));
        if payload_len > LOGSTREAM_MAX_PAYLOAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("logstream payload at offset {frame_offset} is too large"),
            ));
        }

        let mut payload = vec![0_u8; payload_len as usize];
        match file.read_exact(&mut payload) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }

        let checksum = Sha256::digest(&payload);
        if checksum.as_slice() != &header[16..48] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad logstream checksum at offset {frame_offset}"),
            ));
        }

        let entry: ZccusanLogEntry = serde_json::from_slice(&payload).map_err(invalid_data)?;
        if entry.schema_version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported logstream schema {} at offset {frame_offset}",
                    entry.schema_version
                ),
            ));
        }
        if entry.offset != frame_offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "logstream entry offset {} does not match frame offset {frame_offset}",
                    entry.offset
                ),
            ));
        }

        next_sequence = next_sequence.max(entry.sequence.saturating_add(1));
        entries.push(entry);
        valid_len = valid_len
            .saturating_add(LOGSTREAM_HEADER_LEN as u64)
            .saturating_add(payload_len);
    }

    Ok(ReplayResult {
        entries,
        valid_len,
        next_sequence,
    })
}

fn validate_record_field(value: &str, label: &str) -> io::Result<()> {
    if value.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} must not be empty"),
        ));
    }
    if value.contains('\n') || value.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} contains an unsupported control character"),
        ));
    }
    Ok(())
}

fn invalid_data(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_logstream_appends_and_replays_entries() {
        let path = temp_log_path("append-replay");
        let log = FileLogStream::open(&path).expect("open logstream");

        let first = log
            .append_state(
                "snapshot.created",
                "snap-a",
                &serde_json::json!({"snapshot_id":"snap-a"}),
            )
            .expect("append first");
        let second = log
            .append_state(
                "snapshot.deleted",
                "snap-a",
                &serde_json::json!({"deleted":true}),
            )
            .expect("append second");

        let entries = log.replay().expect("replay logstream");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], first);
        assert_eq!(entries[1], second);
        assert_eq!(entries[0].sequence, 1);
        assert_eq!(entries[1].sequence, 2);

        let reopened = FileLogStream::open(&path).expect("reopen logstream");
        let third = reopened
            .append_state(
                "stream.started",
                "repl-a",
                &serde_json::json!({"repl_id":"repl-a"}),
            )
            .expect("append third");
        assert_eq!(third.sequence, 3);
        assert_eq!(reopened.replay().expect("replay reopened").len(), 3);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_logstream_truncates_partial_tail_on_open() {
        let path = temp_log_path("partial-tail");
        {
            let log = FileLogStream::open(&path).expect("open logstream");
            log.append_state(
                "snapshot.created",
                "snap-a",
                &serde_json::json!({"ok":true}),
            )
            .expect("append");
        }
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open append");
            file.write_all(b"partial").expect("write partial tail");
            file.sync_data().expect("sync partial tail");
        }

        let log = FileLogStream::open(&path).expect("reopen with partial tail");
        let entries = log.replay().expect("replay after truncation");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "snapshot.created");

        let _ = fs::remove_file(path);
    }

    fn temp_log_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "zcutils-logstream-{name}-{}-{nanos}.log",
            std::process::id()
        ))
    }
}
