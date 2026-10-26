use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use contracts::ids::RunId;
use contracts::replay::{ReplayBlob, verify};
use contracts::types::{ModelCallRecord, Sha256};
use contracts::{Error, ReplayStore};
use serde_json::Value;

use super::{conflict, not_found};

#[derive(Default)]
struct Stored {
    /// By (run, context seq).
    calls: BTreeMap<(String, u64), ModelCallRecord>,
    blobs: BTreeMap<String, Value>,
}

#[derive(Default)]
pub struct InMemoryReplayStore {
    stored: Mutex<Stored>,
}

impl InMemoryReplayStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ReplayStore for InMemoryReplayStore {
    async fn put_call(&self, record: &ModelCallRecord, blobs: &[ReplayBlob]) -> Result<(), Error> {
        let mut stored = self.stored.lock().unwrap();
        verify(record, blobs, |hash| {
            stored.blobs.contains_key(hash.as_str())
        })?;
        let key = (record.run_id.as_str().to_owned(), record.context_seq.get());
        match stored.calls.get(&key) {
            Some(existing) if existing == record => {}
            Some(_) => {
                return Err(conflict(format!(
                    "run {} already has a different call at context seq {}",
                    key.0, key.1
                )));
            }
            None => {
                stored.calls.insert(key, record.clone());
            }
        }
        for blob in blobs {
            stored
                .blobs
                .entry(blob.sha256.as_str().to_owned())
                .or_insert_with(|| blob.content.clone());
        }
        Ok(())
    }

    async fn call(&self, run_id: &RunId, context_seq: u64) -> Result<ModelCallRecord, Error> {
        self.stored
            .lock()
            .unwrap()
            .calls
            .get(&(run_id.as_str().to_owned(), context_seq))
            .cloned()
            .ok_or_else(|| not_found("model call", &format!("{}@{context_seq}", run_id.as_str())))
    }

    async fn calls_for_run(&self, run_id: &RunId) -> Result<Vec<ModelCallRecord>, Error> {
        Ok(self
            .stored
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|((run, _), _)| run == run_id.as_str())
            .map(|(_, record)| record.clone())
            .collect())
    }

    async fn blob(&self, sha256: &Sha256) -> Result<ReplayBlob, Error> {
        self.stored
            .lock()
            .unwrap()
            .blobs
            .get(sha256.as_str())
            .map(|content| ReplayBlob {
                sha256: sha256.clone(),
                content: content.clone(),
            })
            .ok_or_else(|| not_found("replay blob", sha256.as_str()))
    }
}
