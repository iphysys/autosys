//! The stream a run returns (blueprint sections 3.5 and 13.1, ADR-0030).
//!
//! A [`RunStream`] yields every event of the run in sequence order, read
//! back from the event log, so events other components append are included
//! and nothing is reported that was not stored. Then it yields one
//! [`RunEvent::Stopped`] and ends. A harness failure ends it with an `Err`
//! instead, and no `run_stopped` is written.
//!
//! The stream drives the run as it is polled: no task is spawned. The run
//! sends events through a bounded channel, so a slow reader slows the run
//! down rather than letting events pile up. Dropping the stream drops the
//! run, as a crash would; PR 12's recovery picks it up from the stores.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use contracts::ids::{ApprovalId, EvidenceId, RunId, SdrVersion, TransitionId};
use contracts::types::{
    ApprovalAnswer, ContinuationRecord, Event, Evidence, Job, Requirement, RunHarness, StopReason,
    TaskResult, VersionedHash,
};
use contracts::{ContentBlock, Error, EventLog, Message};
use futures_core::Stream;
use tokio::sync::mpsc;

use crate::cancellation::timed;
use crate::capability_router::ToolCallBlock;
use crate::error::RuntimeError;

/// How many events the stream buffers before the run waits for its reader.
pub(crate) const CHANNEL_CAPACITY: usize = 64;

/// How many events one read of the log returns at most.
const PAGE: usize = 256;

/// One item of a run's stream.
#[allow(
    clippy::large_enum_variant,
    reason = "every item but the last is an event; boxing them would allocate per event"
)]
#[derive(Clone, Debug, PartialEq)]
pub enum RunEvent {
    /// An event of the run, as the event log stored it.
    Recorded(Event),
    /// The run stopped. Always the last item.
    Stopped(Box<RunOutcome>),
}

/// How a run stopped (blueprint section 5.4).
#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
    pub stop_reason: StopReason,
    /// Checked against the agent's result schema.
    pub result: TaskResult,
    /// The job with its usage, progress and pending work. Its status and
    /// lease are the scheduler's: the loop never changes them.
    pub job: Job,
    /// The run's state as it stopped, numbered after its last checkpoint:
    /// for `approval_required`, the open step and the approval it waits for
    /// (ADR-0031). The scheduler stores it before it records the stop.
    pub record: ContinuationRecord,
    /// Set when the run waits for an approval: what
    /// [`crate::Orchestrator::resume`] continues from.
    pub continuation: Option<Continuation>,
}

/// Where a run starts.
pub enum Start {
    /// From the beginning.
    New(Box<Job>),
    /// From an in-memory [`Continuation`], with the human's answer.
    Resume {
        continuation: Box<Continuation>,
        answer: ApprovalAnswer,
    },
    /// From the job as the job store holds it, and its last checkpoint, or
    /// from the beginning if it has none: after a crash, a lost lease or a
    /// requeue. Usage recorded after the checkpoint is counted again.
    Recover {
        job: Box<Job>,
        record: Option<Box<ContinuationRecord>>,
    },
    /// From a checkpoint that waits for an approval, with the human's
    /// answer: the call that asked goes through the gates again with it.
    Answer {
        job: Box<Job>,
        record: Box<ContinuationRecord>,
        answer: ApprovalAnswer,
    },
}

impl Start {
    /// The job the run starts with.
    pub fn job(&self) -> &Job {
        match self {
            Start::New(job) | Start::Recover { job, .. } | Start::Answer { job, .. } => job,
            Start::Resume { continuation, .. } => continuation.job(),
        }
    }
}

/// A run waiting for a human's answer: the open turn, the call that asked,
/// the calls not yet run, and the counters. Resuming puts the call through
/// the gates again with the answer, then continues the run. PR 12 persists
/// it; until then it lives in memory.
#[derive(Clone, Debug, PartialEq)]
pub struct Continuation {
    pub(crate) state: LoopState,
    pub(crate) turn: Vec<Message>,
    pub(crate) step: OpenStep,
    pub(crate) approval_id: ApprovalId,
}

impl Continuation {
    /// The approval the run waits for, as `approval_requested` named it.
    pub fn approval_id(&self) -> &ApprovalId {
        &self.approval_id
    }

    /// The job as it was when the run stopped.
    pub fn job(&self) -> &Job {
        &self.state.job
    }
}

/// What the loop carries from step to step, besides the open turn.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LoopState {
    /// Usage, progress (the last event forwarded included) and pending work.
    pub(crate) job: Job,
    /// Failures since the last commit, toward the repair bound.
    pub(crate) failures: u64,
    /// What shaped the run so far, for commit provenance (ADR-0033).
    pub(crate) harness: RunHarness,
    pub(crate) committed: Vec<TransitionId>,
    pub(crate) evidence_ids: Vec<EvidenceId>,
    /// The latest SDR version the run has seen or made.
    pub(crate) head: Option<SdrVersion>,
    pub(crate) last_commit: Option<CommitView>,
    /// The number of the last checkpoint taken; 0 before the first.
    pub(crate) checkpoint: u64,
}

/// The run's latest commit, as the success rule and `pending_work` see it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CommitView {
    pub(crate) transition_id: TransitionId,
    pub(crate) version: SdrVersion,
    pub(crate) final_: bool,
    pub(crate) requirements: Vec<Requirement>,
    pub(crate) evidence: Vec<Evidence>,
}

impl LoopState {
    pub(crate) fn new(job: Job) -> Self {
        Self {
            job,
            failures: 0,
            harness: RunHarness {
                harness_builds: Vec::new(),
                model_ids: Vec::new(),
                prompts: Vec::new(),
                skills: Vec::new(),
                agent_specs: Vec::new(),
            },
            committed: Vec::new(),
            evidence_ids: Vec::new(),
            head: None,
            last_commit: None,
            checkpoint: 0,
        }
    }

    pub(crate) fn add_evidence(&mut self, ids: &[EvidenceId]) {
        for id in ids {
            if !self.evidence_ids.contains(id) {
                self.evidence_ids.push(id.clone());
            }
        }
    }
}

/// The model step whose tool calls are being run.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpenStep {
    /// The step's number: `step-{number}`.
    pub(crate) number: u64,
    /// The SDR version of the step's context.
    pub(crate) sdr_version: SdrVersion,
    /// The prompts of the step's context, for commit provenance.
    pub(crate) prompts: Vec<VersionedHash>,
    /// The model's message, unchanged.
    pub(crate) assistant: Message,
    pub(crate) calls: Vec<ToolCallBlock>,
    /// Results of the calls run so far.
    pub(crate) results: Vec<ContentBlock>,
    /// The index of the next call to run.
    pub(crate) next: usize,
}

/// Where a run's checkpoints go (ADR-0031): after each model reply, after
/// each tool result, and when a turn closes or a commit lands. The scheduler
/// stores each with its job, under the job's fence; an error, such as a lost
/// lease, ends the run.
#[async_trait]
pub trait CheckpointSink: Send + Sync {
    async fn checkpoint(&self, job: &Job, record: &ContinuationRecord) -> Result<(), Error>;
}

/// Keeps no checkpoint: for a run whose state lives only in memory.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCheckpoints;

#[async_trait]
impl CheckpointSink for NoCheckpoints {
    async fn checkpoint(&self, _job: &Job, _record: &ContinuationRecord) -> Result<(), Error> {
        Ok(())
    }
}

/// Sends the run's events into its stream, read back from the log.
pub(crate) struct Forwarder<'a> {
    pub(crate) events: &'a dyn EventLog,
    pub(crate) run_id: RunId,
    pub(crate) sender: mpsc::Sender<Event>,
    pub(crate) io_timeout: Duration,
}

impl Forwarder<'_> {
    /// Sends every event after `job.progress.last_event_seq`, in order, and
    /// moves the mark. Reads with the I/O timeout alone, so a cancelled run
    /// still reports its last events.
    pub(crate) async fn forward(&self, job: &mut Job) -> Result<(), RuntimeError> {
        loop {
            let after = job.progress.last_event_seq;
            let page = timed(
                self.io_timeout,
                "reading the run's events",
                self.events.read(&self.run_id, after, PAGE),
            )
            .await?;
            if page.is_empty() {
                return Ok(());
            }
            for event in page {
                let seq = event.seq.get();
                if seq != job.progress.last_event_seq + 1 {
                    return Err(RuntimeError::Internal(format!(
                        "the event log went from seq {} to {seq}",
                        job.progress.last_event_seq
                    )));
                }
                job.progress.last_event_seq = seq;
                self.sender.send(event).await.map_err(|_| {
                    RuntimeError::Internal("the run's stream was dropped".to_owned())
                })?;
            }
        }
    }
}

type RunFuture<'a> = Pin<Box<dyn Future<Output = Result<RunOutcome, RuntimeError>> + Send + 'a>>;

/// The events of one run, then how it stopped. See the module docs.
pub struct RunStream<'a> {
    events: Option<mpsc::Receiver<Event>>,
    run: Option<RunFuture<'a>>,
    finished: Option<Result<RunOutcome, RuntimeError>>,
}

impl<'a> RunStream<'a> {
    /// A stream over the run `start` builds, given the channel's sender.
    pub(crate) fn new<F>(start: impl FnOnce(mpsc::Sender<Event>) -> F) -> Self
    where
        F: Future<Output = Result<RunOutcome, RuntimeError>> + Send + 'a,
    {
        let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        Self {
            events: Some(receiver),
            run: Some(Box::pin(start(sender))),
            finished: None,
        }
    }

    /// The next item; `None` after `Stopped` or an error.
    pub async fn next(&mut self) -> Option<Result<RunEvent, RuntimeError>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }
}

impl Stream for RunStream<'_> {
    type Item = Result<RunEvent, RuntimeError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            // Buffered events come first, so they are never overtaken by
            // the outcome.
            if let Some(receiver) = this.events.as_mut() {
                match receiver.poll_recv(cx) {
                    Poll::Ready(Some(event)) => {
                        return Poll::Ready(Some(Ok(RunEvent::Recorded(event))));
                    }
                    // The run is over and every event it sent is out.
                    Poll::Ready(None) => this.events = None,
                    Poll::Pending if this.run.is_none() => return Poll::Pending,
                    Poll::Pending => {}
                }
            }
            if let Some(run) = this.run.as_mut() {
                match run.as_mut().poll(cx) {
                    Poll::Ready(finished) => {
                        // Dropping the run drops its sender, which closes
                        // the channel once the rest is read.
                        this.run = None;
                        this.finished = Some(finished);
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
            if this.events.is_some() {
                continue;
            }
            return Poll::Ready(
                this.finished
                    .take()
                    .map(|finished| finished.map(|outcome| RunEvent::Stopped(Box::new(outcome)))),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use contracts::Clock;
    use testkit::ManualClock;
    use testkit::builders::every_event;

    use super::*;

    fn events(count: usize) -> Vec<Event> {
        let at = ManualClock::at_epoch().now();
        let mut all = Vec::new();
        while all.len() < count {
            all.extend(every_event("run-1", at));
        }
        all.truncate(count);
        all
    }

    fn assert_send<T: Send>(_: &T) {}

    #[tokio::test]
    async fn every_buffered_event_comes_before_the_end_even_beyond_the_channel() {
        let sent = events(CHANNEL_CAPACITY * 2 + 3);
        let to_send = sent.clone();
        let mut stream = RunStream::new(move |sender| async move {
            for event in to_send {
                sender
                    .send(event)
                    .await
                    .map_err(|_| RuntimeError::Internal("the stream was dropped".to_owned()))?;
            }
            Err(RuntimeError::Internal("a harness failure".to_owned()))
        });
        assert_send(&stream);
        let mut received = Vec::new();
        let mut ended_with = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(RunEvent::Recorded(event)) => received.push(event),
                Ok(RunEvent::Stopped(_)) => panic!("the run did not stop"),
                Err(error) => ended_with = Some(error.to_string()),
            }
        }
        assert_eq!(received, sent);
        assert_eq!(
            ended_with.as_deref(),
            Some("internal error: a harness failure")
        );
        assert!(stream.next().await.is_none(), "nothing after the end");
    }

    #[tokio::test]
    async fn a_run_waits_for_its_reader_when_the_channel_is_full() {
        let sent = events(CHANNEL_CAPACITY + 1);
        let progress = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = progress.clone();
        let mut stream = RunStream::new(move |sender| async move {
            for event in sent {
                sender
                    .send(event)
                    .await
                    .map_err(|_| RuntimeError::Internal("the stream was dropped".to_owned()))?;
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Err(RuntimeError::Internal("done".to_owned()))
        });
        // Reading one item lets the run fill the channel and no more.
        let first = stream.next().await;
        assert!(matches!(first, Some(Ok(RunEvent::Recorded(_)))));
        let sent_so_far = progress.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            sent_so_far <= CHANNEL_CAPACITY + 1,
            "{sent_so_far} events sent with one read"
        );
        drop(stream);
    }
}
