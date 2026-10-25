//! `GET /v0/runs/{run_id}/events`: the run's session log as server-sent
//! events (blueprint section 3.5).
//!
//! Each frame's id is the event's sequence number, so a client that
//! reconnects with `Last-Event-ID` gets every later event exactly once:
//! the log numbers a run's events without gaps, and the stream reads it in
//! order from that number. The stream ends once the run has stopped for
//! good and the log is read to its end; a worker records a run's last
//! event before the save that stops it, so a read after seeing the stopped
//! job finds everything. A suspended run's stream stays open.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State as Shared};
use axum::http::HeaderMap;
use axum::response::sse::{Event as Frame, KeepAlive, Sse};
use contracts::ids::RunId;
use contracts::types::Event;
use contracts::{EventKind, ExecutionContext};
use futures_util::Stream;

use crate::State;
use crate::error::ApiError;
use crate::request::{Call, is_final, job_of, parse_id};

const LAST_EVENT_ID: &str = "last-event-id";

pub(crate) async fn stream(
    Shared(state): Shared<Arc<State>>,
    Call(ctx): Call,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Frame, Infallible>>>, ApiError> {
    let run: RunId = parse_id(&run_id, "run id", &ctx)?;
    job_of(state.services.jobs.as_ref(), &run, &ctx).await?;
    let after = match headers.get(LAST_EVENT_ID) {
        None => 0,
        Some(value) => value
            .to_str()
            .ok()
            .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| {
                ApiError::invalid(
                    "Last-Event-ID must be the sequence number of an event",
                    ctx.correlation_id(),
                )
            })?,
    };
    let keep_alive = state.services.limits.keep_alive;
    let cursor = Cursor {
        state,
        run,
        after,
        buffer: VecDeque::new(),
        stopped: false,
        ctx,
    };
    let frames = futures_util::stream::unfold(cursor, Cursor::next);
    Ok(Sse::new(frames).keep_alive(KeepAlive::new().interval(keep_alive)))
}

/// Where a stream is in its run's log.
struct Cursor {
    state: Arc<State>,
    run: RunId,
    /// The last sequence number read.
    after: u64,
    /// Read, not yet sent.
    buffer: VecDeque<Event>,
    /// The job was seen stopped for good: once the log is read to its end
    /// again, the stream ends.
    stopped: bool,
    ctx: ExecutionContext,
}

impl Cursor {
    /// The next frame, or `None` when the stream ends.
    async fn next(mut self) -> Option<(Result<Frame, Infallible>, Self)> {
        let s = &self.state.services;
        loop {
            if let Some(event) = self.buffer.pop_front() {
                let frame = self.frame(&event)?;
                return Some((Ok(frame), self));
            }
            if self.state.shutdown.is_cancelled() {
                return None;
            }
            match s
                .events
                .read(&self.run, self.after, s.limits.page_size.max(1))
                .await
            {
                Ok(page) if !page.is_empty() => {
                    if let Some(last) = page.last() {
                        self.after = last.seq.get();
                    }
                    self.buffer.extend(page);
                    continue;
                }
                Ok(_) => {}
                Err(error) => return self.end(&error),
            }
            if self.stopped {
                return None;
            }
            match s.jobs.find_by_run(&self.run).await {
                Ok(Some(job)) if is_final(&job) => {
                    self.stopped = true;
                    continue;
                }
                Ok(_) => {}
                Err(error) => return self.end(&error),
            }
            tokio::select! {
                () = self.state.shutdown.cancelled() => return None,
                waited = s.events.wait_for(&self.run, self.after, &self.ctx) => {
                    if let Err(error) = waited {
                        return self.end(&error);
                    }
                }
            }
        }
    }

    fn frame(&self, event: &Event) -> Option<Frame> {
        match Frame::default()
            .id(event.seq.get().to_string())
            .event(EventKind::of(&event.payload).as_str())
            .json_data(event)
        {
            Ok(frame) => Some(frame),
            Err(error) => {
                tracing::error!(
                    correlation_id = self.ctx.correlation_id().as_str(),
                    %error,
                    "an event cannot be sent; ending the stream"
                );
                None
            }
        }
    }

    /// Ends the stream after a store failure; the client reconnects from
    /// its last id.
    fn end<T>(&self, error: &contracts::Error) -> Option<T> {
        tracing::warn!(
            correlation_id = self.ctx.correlation_id().as_str(),
            run = self.run.as_str(),
            %error,
            "reading the run's events failed; ending the stream"
        );
        None
    }
}
