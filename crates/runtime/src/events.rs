//! The events the runtime appends itself (ADR-0030): `run_started`,
//! `model_call_completed`, `sdr_transition`, `error` and `run_stopped`,
//! with actor `system/runtime`. Every other event comes from the component
//! that owns it: the context builder, the gates, the checks and the tool
//! gateway.

use std::time::Duration;

use contracts::types::{Actor, ErrorInfo, Event, EventPayload, Timestamp};
use contracts::{Clock, EventLog, ExecutionContext, NewEvent};

use crate::cancellation::timed;
use crate::error::RuntimeError;

/// The component recorded as the actor of the runtime's events.
const COMPONENT: &str = "runtime";

/// Appends the runtime's events for one run.
#[derive(Clone, Copy)]
pub(crate) struct Recorder<'a> {
    pub(crate) events: &'a dyn EventLog,
    pub(crate) clock: &'a dyn Clock,
    pub(crate) io_timeout: Duration,
}

impl Recorder<'_> {
    /// Appends `payload` to the run in `ctx`. The append has the I/O
    /// timeout but ignores cancellation: what the runtime records, it records
    /// even for a cancelled run.
    pub(crate) async fn append(
        &self,
        payload: EventPayload,
        ctx: &ExecutionContext,
    ) -> Result<Event, RuntimeError> {
        let run_id = ctx
            .run_id()
            .cloned()
            .ok_or_else(|| RuntimeError::Internal("an event without a run".to_owned()))?;
        let component = COMPONENT
            .parse()
            .map_err(|_| RuntimeError::Internal(format!("{COMPONENT} is not an identifier")))?;
        timed(
            self.io_timeout,
            "appending an event",
            self.events.append(NewEvent {
                run_id,
                occurred_at: Timestamp(self.clock.now()),
                correlation_id: ctx.correlation_id().clone(),
                actor: Actor::System { component },
                payload,
            }),
        )
        .await
    }

    /// Appends an `error` event.
    pub(crate) async fn error(
        &self,
        error: ErrorInfo,
        ctx: &ExecutionContext,
    ) -> Result<Event, RuntimeError> {
        self.append(EventPayload::Error { error }, ctx).await
    }
}
