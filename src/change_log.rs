//! Durable atomic change batches for control-plane state.
//!
//! One committed batch is one checksummed `FileLogStream` frame and one
//! `sync_data()`. Component state machines stage and validate their complete
//! transition before persistence, then publish it and notify subscribers.

use crate::block::logstream::{DurableLogStream, FileLogStream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

pub const CHANGE_LOG_STREAM: &str = "zccusan.changes.v1";
pub const CHANGE_BATCH_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComponentChange {
    pub component_id: String,
    pub entity_id: String,
    pub operation: String,
    pub schema_hash: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangeBatch {
    pub schema_version: u16,
    pub stream_id: String,
    pub transaction_id: String,
    pub expected_revision: u64,
    pub new_revision: u64,
    pub topology_epoch: u64,
    pub changes: Vec<ComponentChange>,
    pub referenced_object_hashes: Vec<String>,
    pub resulting_state_hash: String,
}

impl ChangeBatch {
    pub fn validate_shape(&self) -> io::Result<()> {
        if self.schema_version != CHANGE_BATCH_SCHEMA_VERSION {
            return Err(invalid(format!(
                "unsupported change batch schema {}",
                self.schema_version
            )));
        }
        validate_id(&self.stream_id, "stream_id")?;
        validate_id(&self.transaction_id, "transaction_id")?;
        if self.new_revision != self.expected_revision.saturating_add(1) {
            return Err(invalid(format!(
                "change revision must advance by one expected={} new={}",
                self.expected_revision, self.new_revision
            )));
        }
        if self.changes.is_empty() {
            return Err(invalid("atomic change batch must not be empty"));
        }
        for change in &self.changes {
            validate_id(&change.component_id, "component_id")?;
            validate_id(&change.entity_id, "entity_id")?;
            validate_id(&change.operation, "operation")?;
            validate_hash(&change.schema_hash, "schema_hash")?;
        }
        for hash in &self.referenced_object_hashes {
            validate_hash(hash, "referenced_object_hash")?;
        }
        validate_hash(&self.resulting_state_hash, "resulting_state_hash")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommittedChangeBatch {
    pub raft_term: u64,
    pub raft_index: u64,
    pub batch: ChangeBatch,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StreamCursor {
    revision: u64,
    raft_term: u64,
    raft_index: u64,
}

pub struct ChangeLogStore {
    log: Arc<FileLogStream>,
    committed: Mutex<Vec<Arc<CommittedChangeBatch>>>,
    cursors: Mutex<BTreeMap<String, StreamCursor>>,
    subscribers: Mutex<Vec<Sender<Arc<CommittedChangeBatch>>>>,
}

impl ChangeLogStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let log = Arc::new(FileLogStream::open(path)?);
        let mut committed = Vec::new();
        let mut cursors = BTreeMap::new();
        for record in log.replay()? {
            if record.stream != CHANGE_LOG_STREAM {
                continue;
            }
            let envelope: CommittedChangeBatch = serde_json::from_value(record.value)
                .map_err(|error| invalid(format!("decode committed change batch: {error}")))?;
            if record.term != envelope.raft_term {
                return Err(invalid(format!(
                    "change envelope term {} differs from record term {}",
                    envelope.raft_term, record.term
                )));
            }
            validate_next(&mut cursors, &envelope)?;
            committed.push(Arc::new(envelope));
        }
        Ok(Self {
            log,
            committed: Mutex::new(committed),
            cursors: Mutex::new(cursors),
            subscribers: Mutex::new(Vec::new()),
        })
    }

    /// Persist a complete transaction as one durable frame. Call `publish()`
    /// only after the component state machine has installed its staged state.
    pub fn persist(&self, envelope: &CommittedChangeBatch) -> io::Result<()> {
        envelope.batch.validate_shape()?;
        let mut cursors = self.cursors.lock().expect("change cursor mutex poisoned");
        let mut staged = cursors.clone();
        validate_next(&mut staged, envelope)?;
        self.log.append_at_term(
            CHANGE_LOG_STREAM,
            "change.batch.committed",
            &envelope.batch.stream_id,
            envelope.raft_term,
            envelope,
        )?;
        *cursors = staged;
        self.committed
            .lock()
            .expect("committed changes mutex poisoned")
            .push(Arc::new(envelope.clone()));
        Ok(())
    }

    pub fn publish(&self, envelope: Arc<CommittedChangeBatch>) {
        self.subscribers
            .lock()
            .expect("change subscriber mutex poisoned")
            .retain(|subscriber| subscriber.send(Arc::clone(&envelope)).is_ok());
    }

    pub fn subscribe(&self) -> Receiver<Arc<CommittedChangeBatch>> {
        let (sender, receiver) = mpsc::channel();
        self.subscribers
            .lock()
            .expect("change subscriber mutex poisoned")
            .push(sender);
        receiver
    }

    pub fn replay_from(
        &self,
        stream_id: &str,
        after_revision: u64,
    ) -> Vec<Arc<CommittedChangeBatch>> {
        self.committed
            .lock()
            .expect("committed changes mutex poisoned")
            .iter()
            .filter(|entry| {
                entry.batch.stream_id == stream_id && entry.batch.new_revision > after_revision
            })
            .cloned()
            .collect()
    }

    pub fn revision(&self, stream_id: &str) -> u64 {
        self.cursors
            .lock()
            .expect("change cursor mutex poisoned")
            .get(stream_id)
            .map_or(0, |cursor| cursor.revision)
    }
}

fn validate_next(
    cursors: &mut BTreeMap<String, StreamCursor>,
    envelope: &CommittedChangeBatch,
) -> io::Result<()> {
    envelope.batch.validate_shape()?;
    if envelope.raft_term == 0 || envelope.raft_index == 0 {
        return Err(invalid(
            "committed change Raft term and index must be nonzero",
        ));
    }
    let cursor = cursors.entry(envelope.batch.stream_id.clone()).or_default();
    if envelope.batch.expected_revision != cursor.revision {
        return Err(invalid(format!(
            "change stream {} revision conflict expected={} actual={}",
            envelope.batch.stream_id, envelope.batch.expected_revision, cursor.revision
        )));
    }
    if envelope.raft_index <= cursor.raft_index {
        return Err(invalid(format!(
            "change stream {} Raft index did not advance",
            envelope.batch.stream_id
        )));
    }
    if envelope.raft_term < cursor.raft_term {
        return Err(invalid(format!(
            "change stream {} Raft term regressed",
            envelope.batch.stream_id
        )));
    }
    cursor.revision = envelope.batch.new_revision;
    cursor.raft_term = envelope.raft_term;
    cursor.raft_index = envelope.raft_index;
    Ok(())
}

pub fn content_hash<T: Serialize>(value: &T) -> io::Result<String> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| invalid(format!("serialize change hash input: {error}")))?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
}

fn validate_id(value: &str, label: &str) -> io::Result<()> {
    if value.is_empty() || value.contains('\0') || value.contains('\n') {
        return Err(invalid(format!("invalid {label}")));
    }
    Ok(())
}

fn validate_hash(value: &str, label: &str) -> io::Result<()> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(invalid(format!("{label} must use sha256:<hex>")));
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(format!("invalid {label}")));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn envelope(revision: u64, changes: usize) -> CommittedChangeBatch {
        let payload = serde_json::json!({"revision": revision});
        let change = ComponentChange {
            component_id: "test.component".into(),
            entity_id: "entity-a".into(),
            operation: "update".into(),
            schema_hash: content_hash(&"test-schema-v1").unwrap(),
            payload,
        };
        CommittedChangeBatch {
            raft_term: 3,
            raft_index: revision,
            batch: ChangeBatch {
                schema_version: CHANGE_BATCH_SCHEMA_VERSION,
                stream_id: "test-stream".into(),
                transaction_id: format!("tx-{revision}"),
                expected_revision: revision - 1,
                new_revision: revision,
                topology_epoch: 7,
                changes: vec![change; changes],
                referenced_object_hashes: Vec::new(),
                resulting_state_hash: content_hash(&revision).unwrap(),
            },
        }
    }

    #[test]
    fn atomic_batch_is_one_replay_record_and_one_notification() {
        let path = temp_path("atomic");
        let store = ChangeLogStore::open(&path).unwrap();
        let receiver = store.subscribe();
        let committed = envelope(1, 3);
        store.persist(&committed).unwrap();
        assert!(receiver.try_recv().is_err());
        store.publish(Arc::new(committed.clone()));
        assert_eq!(receiver.recv().unwrap().batch.changes.len(), 3);
        drop(store);
        let reopened = ChangeLogStore::open(&path).unwrap();
        let replay = reopened.replay_from("test-stream", 0);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].batch, committed.batch);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn conflicting_revision_is_rejected_without_partial_append() {
        let path = temp_path("conflict");
        let store = ChangeLogStore::open(&path).unwrap();
        store.persist(&envelope(1, 2)).unwrap();
        assert!(store.persist(&envelope(1, 4)).is_err());
        assert_eq!(store.revision("test-stream"), 1);
        assert_eq!(store.replay_from("test-stream", 0).len(), 1);
        fs::remove_file(path).unwrap();
    }

    fn temp_path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "zc-change-{label}-{}-{nonce}.log",
            std::process::id()
        ))
    }
}
