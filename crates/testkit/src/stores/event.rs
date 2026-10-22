use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Mutex;

use async_trait::async_trait;
use contracts::ids::RunId;
use contracts::types::{Event, EventSchemaVersion};
use contracts::{Error, ErrorCode, EventLog, ExecutionContext, NewEvent};
use tokio::sync::Notify;

#[derive(Default)]
pub struct InMemoryEventLog {
    runs: Mutex<HashMap<String, Vec<Event>>>,
    appended: Notify,
}

impl InMemoryEventLog {
    pub fn new() -> Self {
        Self::default()
    }

    fn last_seq_now(&self, run_id: &RunId) -> u64 {
        self.runs
            .lock()
            .unwrap()
            .get(run_id.as_str())
            .map_or(0, |events| events.len() as u64)
    }
}

#[async_trait]
impl EventLog for InMemoryEventLog {
    async fn append(&self, event: NewEvent) -> Result<Event, Error> {
        let stored = {
            let mut runs = self.runs.lock().unwrap();
            let events = runs.entry(event.run_id.as_str().to_owned()).or_default();
            let seq = NonZeroU64::new(events.len() as u64 + 1)
                .ok_or_else(|| Error::new(ErrorCode::Internal, "sequence overflow"))?;
            let stored = Event {
                schema_version: EventSchemaVersion::X01,
                run_id: event.run_id,
                seq,
                occurred_at: event.occurred_at,
                correlation_id: event.correlation_id,
                actor: event.actor,
                payload: event.payload,
            };
            events.push(stored.clone());
            stored
        };
        self.appended.notify_waiters();
        Ok(stored)
    }

    async fn read(&self, run_id: &RunId, after: u64, limit: usize) -> Result<Vec<Event>, Error> {
        Ok(self
            .runs
            .lock()
            .unwrap()
            .get(run_id.as_str())
            .map(|events| {
                events
                    .iter()
                    .filter(|e| e.seq.get() > after)
                    .take(limit)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn last_seq(&self, run_id: &RunId) -> Result<u64, Error> {
        Ok(self.last_seq_now(run_id))
    }

    async fn wait_for(
        &self,
        run_id: &RunId,
        after: u64,
        ctx: &ExecutionContext,
    ) -> Result<(), Error> {
        loop {
            // Register for the notification before checking, so an append
            // between the check and the wait is not missed.
            let notified = self.appended.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.last_seq_now(run_id) > after {
                return Ok(());
            }
            ctx.ensure_not_cancelled()?;
            tokio::select! {
                () = &mut notified => {}
                () = ctx.cancelled() => return ctx.ensure_not_cancelled(),
            }
        }
    }
}
