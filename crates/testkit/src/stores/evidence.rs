use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use contracts::ids::{EvidenceId, RunId};
use contracts::types::Evidence;
use contracts::{Error, EvidenceStore};

use super::{conflict, not_found};

#[derive(Default)]
pub struct InMemoryEvidenceStore {
    records: Mutex<BTreeMap<String, Evidence>>,
}

impl InMemoryEvidenceStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl EvidenceStore for InMemoryEvidenceStore {
    async fn put(&self, evidence: &Evidence) -> Result<(), Error> {
        let mut records = self.records.lock().unwrap();
        match records.get(evidence.id.as_str()) {
            Some(existing) if existing == evidence => Ok(()),
            Some(_) => Err(conflict(format!(
                "evidence {} exists with different content",
                evidence.id.as_str()
            ))),
            None => {
                records.insert(evidence.id.as_str().to_owned(), evidence.clone());
                Ok(())
            }
        }
    }

    async fn get(&self, id: &EvidenceId) -> Result<Evidence, Error> {
        self.records
            .lock()
            .unwrap()
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| not_found("evidence", id.as_str()))
    }

    async fn list_for_run(&self, run_id: &RunId) -> Result<Vec<Evidence>, Error> {
        let mut found: Vec<Evidence> = self
            .records
            .lock()
            .unwrap()
            .values()
            .filter(|e| &e.run_id == run_id)
            .cloned()
            .collect();
        found.sort_by(|a, b| (a.created_at.0, a.id.as_str()).cmp(&(b.created_at.0, b.id.as_str())));
        Ok(found)
    }
}
