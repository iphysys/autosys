use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use contracts::ids::Identifier;
use contracts::types::{StepOutcome, StepRecord, StepRecordSchemaVersion, Timestamp};
use contracts::{Clock, Error, NewStep, StepStart, StepStore};

use super::{conflict, not_found};

/// [`StepStore`] in memory. `started_at` and `completed_at` come from the
/// clock given at construction, standing in for the database clock.
pub struct InMemoryStepStore {
    clock: Arc<dyn Clock>,
    records: Mutex<BTreeMap<String, StepRecord>>,
}

impl InMemoryStepStore {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            records: Mutex::new(BTreeMap::new()),
        }
    }
}

/// Whether `record` was begun for exactly `step`.
fn same_request(record: &StepRecord, step: &NewStep) -> bool {
    record.run_id == step.run_id
        && record.step_id == step.step_id
        && record.tool == step.tool
        && record.request_sha256 == step.request_sha256
}

#[async_trait]
impl StepStore for InMemoryStepStore {
    async fn begin(&self, step: &NewStep) -> Result<StepStart, Error> {
        let mut records = self.records.lock().unwrap();
        let key = step.idempotency_key.as_str();
        if let Some(existing) = records.get(key) {
            return if same_request(existing, step) {
                Ok(StepStart::Existing(existing.clone()))
            } else {
                Err(conflict(format!(
                    "idempotency key {key} was used for a different request"
                )))
            };
        }
        let record = StepRecord {
            schema_version: StepRecordSchemaVersion::X01,
            idempotency_key: step.idempotency_key.clone(),
            run_id: step.run_id.clone(),
            step_id: step.step_id.clone(),
            tool: step.tool.clone(),
            request_sha256: step.request_sha256.clone(),
            started_at: Timestamp(self.clock.now()),
            completed_at: None,
            outcome: None,
        };
        records.insert(key.to_owned(), record.clone());
        Ok(StepStart::Started(record))
    }

    async fn complete(&self, key: &Identifier, outcome: &StepOutcome) -> Result<StepRecord, Error> {
        let mut records = self.records.lock().unwrap();
        let record = records
            .get_mut(key.as_str())
            .ok_or_else(|| not_found("step", key.as_str()))?;
        match &record.outcome {
            Some(stored) if stored == outcome => Ok(record.clone()),
            Some(_) => Err(conflict(format!(
                "step {} already completed with a different outcome",
                key.as_str()
            ))),
            None => {
                record.completed_at = Some(Timestamp(self.clock.now()));
                record.outcome = Some(outcome.clone());
                Ok(record.clone())
            }
        }
    }

    async fn get(&self, key: &Identifier) -> Result<StepRecord, Error> {
        self.records
            .lock()
            .unwrap()
            .get(key.as_str())
            .cloned()
            .ok_or_else(|| not_found("step", key.as_str()))
    }
}
