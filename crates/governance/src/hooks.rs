//! Hooks observe; they never enforce (blueprint section 15.2).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use contracts::ids::RunId;
use contracts::types::Event;
use contracts::{
    Error, ErrorCode, EventKind, EventLog, EventSubscriber, ExecutionContext, NewEvent,
    WriteAuthority,
};

/// An [`EventLog`] that shows every appended event to the registered
/// subscribers. Give every component this log, and hooks see every event
/// without wrapping the loop.
///
/// Subscribers run after the append, each within `timeout`. A subscriber that
/// fails or times out is logged and skipped: it can never change or stop the
/// run. Its context carries no write authority: hooks observe, they never
/// write (blueprint section 15.2).
pub struct ObservedEventLog {
    inner: Arc<dyn EventLog>,
    subscribers: Vec<Arc<dyn EventSubscriber>>,
    timeout: Duration,
}

impl ObservedEventLog {
    /// InvalidInput if two subscribers share an id.
    pub fn new(
        inner: Arc<dyn EventLog>,
        subscribers: Vec<Arc<dyn EventSubscriber>>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let mut seen = HashSet::new();
        for subscriber in &subscribers {
            if !seen.insert(subscriber.id().as_str().to_owned()) {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "subscriber {} is registered twice",
                        subscriber.id().as_str()
                    ),
                ));
            }
        }
        Ok(Self {
            inner,
            subscribers,
            timeout,
        })
    }
}

#[async_trait]
impl EventLog for ObservedEventLog {
    async fn append(
        &self,
        event: NewEvent,
        authority: Option<&WriteAuthority>,
    ) -> Result<Event, Error> {
        let stored = self.inner.append(event, authority).await?;
        let ctx =
            ExecutionContext::new(stored.correlation_id.clone()).with_run(stored.run_id.clone());
        for subscriber in &self.subscribers {
            let id = subscriber.id().as_str();
            match tokio::time::timeout(self.timeout, subscriber.on_event(&stored, &ctx)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(subscriber = id, seq = stored.seq.get(), %error, "hook failed");
                }
                Err(_) => {
                    tracing::warn!(subscriber = id, seq = stored.seq.get(), "hook timed out");
                }
            }
        }
        Ok(stored)
    }

    async fn read(&self, run_id: &RunId, after: u64, limit: usize) -> Result<Vec<Event>, Error> {
        self.inner.read(run_id, after, limit).await
    }

    async fn read_kinds(
        &self,
        run_id: &RunId,
        after: u64,
        kinds: &[EventKind],
        limit: usize,
    ) -> Result<Vec<Event>, Error> {
        self.inner.read_kinds(run_id, after, kinds, limit).await
    }

    async fn last_seq(&self, run_id: &RunId) -> Result<u64, Error> {
        self.inner.last_seq(run_id).await
    }

    async fn wait_for(
        &self,
        run_id: &RunId,
        after: u64,
        ctx: &ExecutionContext,
    ) -> Result<(), Error> {
        self.inner.wait_for(run_id, after, ctx).await
    }
}
