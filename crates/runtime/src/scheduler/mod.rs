//! The scheduler (blueprint section 6, ADR-0031): claims jobs, keeps them
//! leased, runs them through the orchestrator from their last checkpoint,
//! records every change of job state, and wakes parked jobs when their
//! answer or a cancel arrives.
//!
//! Every write a claimed job makes carries the claim's fence (ADR-0032).
//! This module is the only product code that gives a context its write
//! authority, and the only one that names an unfenced reason: `JobSubmit`
//! for a submit, `ProjectCreate` for a new project's version 0, `JobWake`
//! for a wake-up. [`Submissions`] starts the runs clients ask for, through
//! the same submit.
//!
//! A job leaves running by checkpoint, event, save: the fence ends with the
//! save, so the event that records the change comes first, naming the
//! checkpoint the job rests on. A claim that finds such an event without
//! its save finishes it (recovery step 0). Submits and wake-ups act first
//! and record after; a claim adds a record that is missing.

mod heartbeat;
mod held;
mod submit;
mod transition;

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use contracts::ids::{ApprovalId, CorrelationId, Identifier, JobId, RunId};
use contracts::types::{
    AttemptRecord, ContinuationRecord, Event, EventPayload, Job, JobFailure, JobFailureKind,
    JobState, JobStatus, StopReason, WorkerSettings,
};
use contracts::{
    Claimed, Clock, Error, ErrorCode, EventKind, EventLog, ExecutionContext, JobStore, Spawner,
    UnfencedReason, WakeReason, WriteAuthority, job_state,
};
use sdr::{InitialVersion, SdrManager};
use tokio_util::sync::CancellationToken;

pub use self::submit::Submissions;

use self::held::{HeldJob, Leave};
use self::transition::{WOKEN_BY_ANSWER, WOKEN_BY_CANCEL};
use crate::continuation::{self, checkpoint_ref};
use crate::orchestrator::Orchestrator;
use crate::query_loop::{RunEvent, Start};

/// How many parked jobs one wake-up scan looks at.
const PARKED_PER_SCAN: usize = 100;
/// How many events one read returns at most.
const PAGE: usize = 256;

/// How a worker leases, polls and retries, from `config/worker.yaml`.
#[derive(Clone, Debug)]
pub struct SchedulerSettings {
    /// The worker's id, recorded as the holder of its leases.
    pub holder: Identifier,
    pub lease: Duration,
    pub heartbeat: Duration,
    pub poll: Duration,
    pub poll_jitter: Duration,
    pub max_attempts: u64,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
}

impl SchedulerSettings {
    pub fn new(holder: Identifier, worker: &WorkerSettings) -> Self {
        Self {
            holder,
            lease: Duration::from_millis(worker.lease_ms.get()),
            heartbeat: Duration::from_millis(worker.heartbeat_ms.get()),
            poll: Duration::from_millis(worker.poll_interval_ms.get()),
            poll_jitter: Duration::from_millis(worker.poll_jitter_ms),
            max_attempts: worker.max_attempts.get(),
            backoff_min: Duration::from_millis(worker.retry_backoff_min_ms),
            backoff_max: Duration::from_millis(worker.retry_backoff_max_ms),
        }
    }

    /// The delay before the retry that follows failed attempt number
    /// `failed` (from 1): the minimum, doubling, up to the maximum.
    pub fn backoff(&self, failed: u64) -> Duration {
        let doublings = u32::try_from(failed.saturating_sub(1)).unwrap_or(u32::MAX);
        let factor = 2u32.checked_pow(doublings).unwrap_or(u32::MAX);
        self.backoff_min
            .checked_mul(factor)
            .unwrap_or(self.backoff_max)
            .min(self.backoff_max)
    }
}

/// How a claimed job's turn on this worker ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It left running for this state.
    Left(JobState),
    /// Its lease was lost; another worker has it, or will.
    LeaseLost,
    /// This worker is shutting down and gave it back.
    Released,
}

/// What one turn of the idle loop did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tick {
    /// Claimed `job` and ran it until `ended`.
    Ran { job: JobId, ended: Ended },
    /// Claimed nothing, and woke this many parked jobs.
    Scanned { woken: usize },
}

/// Runs jobs, one at a time. See the module docs.
pub struct Scheduler {
    jobs: Arc<dyn JobStore>,
    events: Arc<dyn EventLog>,
    sdr: Arc<SdrManager>,
    orchestrator: Arc<Orchestrator>,
    spawner: Arc<dyn Spawner>,
    clock: Arc<dyn Clock>,
    settings: SchedulerSettings,
    /// The last seq of each parked job's run that a scan read: a cache, so
    /// a scan reads only what is new. Losing it is safe.
    seen: Mutex<HashMap<String, u64>>,
    jitter: Mutex<Jitter>,
}

/// Spreads a worker's polls around their interval, so workers started
/// together do not poll in step: a small xorshift generator seeded by the
/// worker id.
struct Jitter(u64);

impl Jitter {
    fn seeded(holder: &Identifier) -> Self {
        let seed = holder
            .as_str()
            .bytes()
            .fold(0x9e37_79b9_7f4a_7c15_u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
            });
        Self(seed | 1)
    }

    /// `poll`, give or take up to `spread`.
    fn next(&mut self, poll: Duration, spread: Duration) -> Duration {
        let spread = u64::try_from(spread.as_millis()).unwrap_or(u64::MAX);
        if spread == 0 {
            return poll;
        }
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        let offset = self.0 % spread.saturating_mul(2).saturating_add(1);
        let poll = u64::try_from(poll.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(poll.saturating_add(offset).saturating_sub(spread))
    }
}

impl Scheduler {
    pub fn new(
        jobs: Arc<dyn JobStore>,
        events: Arc<dyn EventLog>,
        sdr: Arc<SdrManager>,
        orchestrator: Arc<Orchestrator>,
        spawner: Arc<dyn Spawner>,
        clock: Arc<dyn Clock>,
        settings: SchedulerSettings,
    ) -> Self {
        let jitter = Jitter::seeded(&settings.holder);
        Self {
            jobs,
            events,
            sdr,
            orchestrator,
            spawner,
            clock,
            settings,
            seen: Mutex::new(HashMap::new()),
            jitter: Mutex::new(jitter),
        }
    }

    /// Submits a top-level job: creates its project's version 0 from
    /// `initial` if given, inserts the job and records it queued. A child
    /// job is not submitted here: its parent inserts it under its own fence.
    pub async fn submit(&self, job: &Job, initial: Option<InitialVersion>) -> Result<(), Error> {
        if let Some(initial) = initial {
            submit::create_project(&self.sdr, job, initial).await?;
        }
        submit::insert_and_record(
            self.jobs.as_ref(),
            self.events.as_ref(),
            self.clock.as_ref(),
            job,
        )
        .await
    }

    /// Claims and runs work until `shutdown`, polling with jitter when there
    /// is none. Store failures are logged and retried at the next poll.
    pub async fn run(&self, shutdown: CancellationToken) {
        while !shutdown.is_cancelled() {
            match self.tick(&shutdown).await {
                Ok(Tick::Ran { .. }) => continue,
                Ok(Tick::Scanned { .. }) => {}
                Err(error) => tracing::warn!(%error, "the scheduler's turn failed; trying again"),
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(self.next_poll()) => {}
            }
        }
    }

    /// One turn: claims a job and runs it, or else scans parked jobs.
    pub async fn tick(&self, shutdown: &CancellationToken) -> Result<Tick, Error> {
        match self
            .jobs
            .claim(&self.settings.holder, self.settings.lease)
            .await?
        {
            Some(claimed) => {
                let job = claimed.job.id.clone();
                let ended = self.hold(claimed, shutdown).await?;
                Ok(Tick::Ran { job, ended })
            }
            None => Ok(Tick::Scanned {
                woken: self.wake_parked().await?,
            }),
        }
    }

    fn next_poll(&self) -> Duration {
        match self.jitter.lock() {
            Ok(mut jitter) => jitter.next(self.settings.poll, self.settings.poll_jitter),
            Err(_) => self.settings.poll,
        }
    }

    /// Runs a claimed job until it leaves running, its lease is lost, or
    /// `shutdown` gives it back.
    async fn hold(&self, claimed: Claimed, shutdown: &CancellationToken) -> Result<Ended, Error> {
        let Claimed {
            mut job,
            fence,
            interrupted,
            ..
        } = claimed;
        let held = HeldJob::new(
            Arc::clone(&self.jobs),
            Arc::clone(&self.events),
            Arc::clone(&self.clock),
            fence.clone(),
            correlation(&job.id, job.attempt)?,
            job.progress.last_event_seq,
        );
        let lost = CancellationToken::new();
        let _heartbeat = self.spawner.spawn(
            "heartbeat",
            heartbeat::heartbeat(
                Arc::clone(&self.jobs),
                fence.clone(),
                self.settings.lease,
                self.settings.heartbeat,
                lost.clone(),
            ),
        );
        let outcome = self
            .recover(&held, &mut job, interrupted, &lost, shutdown)
            .await;
        match outcome {
            Err(error) if error.code() == ErrorCode::LeaseLost => {
                tracing::warn!(%error, "the job's lease was lost; dropping it");
                Ok(Ended::LeaseLost)
            }
            other => other,
        }
    }

    /// The claim's next move (ADR-0031, recovery steps 0 to 4).
    async fn recover(
        &self,
        held: &HeldJob,
        job: &mut Job,
        interrupted: bool,
        lost: &CancellationToken,
        shutdown: &CancellationToken,
    ) -> Result<Ended, Error> {
        let last = transition::last_change(self.events.as_ref(), &job.run_id).await?;

        // 0. A transition a dead worker decided, without its save.
        if let Some((seq, Some(JobState::Running), to)) = &last
            && interrupted
            && job_state(to) != JobState::Running
            && job.progress.last_event_seq < *seq
        {
            return self.finish_decided(held, job, *seq, to).await;
        }
        let build = self.orchestrator.harness_build();
        for payload in transition::missing(last.as_ref().map(|(_, _, to)| to), interrupted, build) {
            self.record(held, &job.run_id, payload).await?;
        }
        let stored = self.jobs.continuation(&job.id).await?;

        if interrupted {
            let error = Error::new(
                ErrorCode::Unavailable,
                "the attempt was interrupted: its lease expired",
            );
            if let Some(ended) = self
                .failed_attempt(held, job, stored.as_ref(), &error)
                .await?
            {
                return Ok(ended);
            }
        }

        // 1. The previous holder's run stopped, and nothing recorded it: a
        // stop after the log's last state change. The final checkpoint is
        // written after the stop, so the job's own mark may be past it.
        let since = last.as_ref().map_or(0, |(seq, _, _)| *seq);
        let stops = self
            .read_kinds(&job.run_id, since, &[EventKind::RUN_STOPPED])
            .await?;
        if let Some(EventPayload::RunStopped { stop_reason, .. }) = stops.last().map(|e| &e.payload)
        {
            return self.record_stop(held, job, stored, *stop_reason).await;
        }

        // 2. A cancel after the last stop.
        let marks = self
            .read_kinds(
                &job.run_id,
                0,
                &[EventKind::RUN_STOPPED, EventKind::CANCEL_REQUESTED],
            )
            .await?;
        let cancelled = marks
            .iter()
            .rev()
            .take_while(|e| !matches!(e.payload, EventPayload::RunStopped { .. }))
            .any(|e| matches!(e.payload, EventPayload::CancelRequested { .. }));

        // 3. Waiting for an answer.
        let waiting = stored
            .as_ref()
            .and_then(|record| record.approval_id.clone())
            .filter(|_| !cancelled);
        let start = match (stored, waiting) {
            (Some(record), Some(approval_id)) => {
                match self.answer(&job.run_id, &approval_id).await? {
                    Some(answer) => Start::Answer {
                        job: Box::new(job.clone()),
                        record: Box::new(record),
                        answer,
                    },
                    None => {
                        let left = held.leave(job, None, Leave::Suspended(approval_id)).await?;
                        return Ok(Ended::Left(job_state(&left.status)));
                    }
                }
            }
            // 4. Go on from the checkpoint, or from the start.
            (record, _) => Start::Recover {
                job: Box::new(job.clone()),
                record: record.map(Box::new),
            },
        };
        self.drive(held, job, start, cancelled, lost, shutdown)
            .await
    }

    /// Recovery step 0: saves the status the job's last transition names,
    /// if the job holds the checkpoint it rests on. It is the save the dead
    /// worker did not make; nothing runs again and nothing is appended.
    async fn finish_decided(
        &self,
        held: &HeldJob,
        job: &Job,
        seq: u64,
        to: &JobStatus,
    ) -> Result<Ended, Error> {
        let stored = self.jobs.continuation(&job.id).await?;
        let named = named_checkpoint(to);
        let holds = match (named, &stored) {
            (None, _) => true,
            (Some(named), Some(record)) => checkpoint_ref(record)? == *named,
            (Some(_), None) => false,
        };
        if !holds {
            tracing::error!(
                job = job.id.as_str(),
                "the job's last transition names a checkpoint the job does not hold"
            );
            let error = Error::new(
                ErrorCode::Internal,
                "the job's last transition names a checkpoint the job does not hold",
            );
            return self
                .fail(
                    held,
                    job,
                    stored.as_ref(),
                    &error,
                    JobFailureKind::HarnessError,
                )
                .await;
        }
        let reason = transition_reason(self.events.as_ref(), &job.run_id, seq).await?;
        let retry_after = (job_state(to) == JobState::Queued
            && reason.as_deref() == Some(transition::RETRY))
        .then(|| self.settings.backoff(job.failed_attempts));
        let mut done = job.clone();
        done.status = to.clone();
        done.progress.last_event_seq = seq;
        held.finish(&done, retry_after).await?;
        Ok(Ended::Left(job_state(to)))
    }

    /// Recovery step 1: the run recorded its stop; the job follows it.
    async fn record_stop(
        &self,
        held: &HeldJob,
        job: &Job,
        stored: Option<ContinuationRecord>,
        stop_reason: StopReason,
    ) -> Result<Ended, Error> {
        let left = if stop_reason == StopReason::ApprovalRequired {
            let approval_id = self.last_approval(&job.run_id).await?.ok_or_else(|| {
                Error::new(
                    ErrorCode::Internal,
                    "a run waits for an approval nobody asked for",
                )
            })?;
            let mut record = match stored {
                Some(record) => record,
                None => initial(job, 0)?,
            };
            record.approval_id = Some(approval_id.clone());
            record.checkpoint = next(record.checkpoint);
            held.leave(job, Some(&record), Leave::Suspended(approval_id))
                .await?
        } else {
            let record = match stored {
                Some(_) => None,
                None => Some(initial(job, 1)?),
            };
            held.leave(job, record.as_ref(), Leave::Stopped(stop_reason))
                .await?
        };
        Ok(Ended::Left(job_state(&left.status)))
    }

    /// Runs the job from `start` until it stops, and records how.
    async fn drive(
        &self,
        held: &HeldJob,
        job: &mut Job,
        start: Start,
        cancelled: bool,
        lost: &CancellationToken,
        shutdown: &CancellationToken,
    ) -> Result<Ended, Error> {
        let ctx = ExecutionContext::builder(held.correlation_id().clone())
            .project(job.project_id.clone())
            .run(job.run_id.clone())
            .task(job.task.id.clone())
            .agent(job.task.agent_id.clone())
            .job(job.id.clone())
            .authorized_by(held.authority().clone())
            .build();
        if cancelled {
            ctx.cancel();
        }
        let watched_from = self.events.last_seq(&job.run_id).await?;
        let mut stream = self.orchestrator.drive(start, held, ctx.clone());
        let ran = async {
            loop {
                match stream.next().await {
                    Some(Ok(RunEvent::Recorded(_))) => {}
                    Some(Ok(RunEvent::Stopped(outcome))) => return Ok(*outcome),
                    Some(Err(error)) => return Err(error),
                    None => {
                        return Err(crate::RuntimeError::Internal(
                            "the run's stream ended without a stop".to_owned(),
                        ));
                    }
                }
            }
        };
        let finished = tokio::select! {
            biased;
            () = lost.cancelled() => return Ok(Ended::LeaseLost),
            () = shutdown.cancelled() => None,
            ran = async {
                tokio::select! {
                    ran = ran => ran,
                    () = self.watch_for_cancel(&job.run_id, watched_from, &ctx) => {
                        Err(crate::RuntimeError::Internal("the cancel watcher stopped".to_owned()))
                    }
                }
            } => Some(ran),
        };
        let Some(ran) = finished else {
            drop(stream);
            let saved = held.last_saved().unwrap_or_else(|| job.clone());
            held.leave(&saved, None, Leave::Released).await?;
            return Ok(Ended::Released);
        };
        drop(stream);
        let left = match ran {
            Ok(outcome) => {
                let to = match (&outcome.stop_reason, &outcome.record.approval_id) {
                    (StopReason::ApprovalRequired, Some(approval_id)) => {
                        Leave::Suspended(approval_id.clone())
                    }
                    (reason, _) => Leave::Stopped(*reason),
                };
                held.leave(&outcome.job, Some(&outcome.record), to).await?
            }
            Err(error) if error.code() == ErrorCode::LeaseLost => return Ok(Ended::LeaseLost),
            Err(error) => {
                let mut saved = held.last_saved().unwrap_or_else(|| job.clone());
                let stored = self.jobs.continuation(&saved.id).await?;
                let error = error.into_error(&ctx);
                if error.retryable() {
                    if let Some(ended) = self
                        .failed_attempt(held, &mut saved, stored.as_ref(), &error)
                        .await?
                    {
                        return Ok(ended);
                    }
                    let delay = self.settings.backoff(saved.failed_attempts);
                    held.leave(&saved, None, Leave::Retry(delay)).await?
                } else {
                    return self
                        .fail(
                            held,
                            &saved,
                            stored.as_ref(),
                            &error,
                            JobFailureKind::HarnessError,
                        )
                        .await;
                }
            }
        };
        Ok(Ended::Left(job_state(&left.status)))
    }

    /// Counts a failed attempt on `job`. Ends the job as failed, and
    /// returns how, if it was the last one allowed.
    async fn failed_attempt(
        &self,
        held: &HeldJob,
        job: &mut Job,
        stored: Option<&ContinuationRecord>,
        error: &Error,
    ) -> Result<Option<Ended>, Error> {
        job.failed_attempts += 1;
        let attempt = NonZeroU64::new(job.attempt.max(1)).unwrap_or(NonZeroU64::MIN);
        job.attempt_history.push(AttemptRecord {
            attempt,
            error: error.to_info(),
            last_event_seq: job.progress.last_event_seq,
        });
        if job.failed_attempts >= self.settings.max_attempts {
            return self
                .fail(held, job, stored, error, JobFailureKind::AttemptsExhausted)
                .await
                .map(Some);
        }
        Ok(None)
    }

    async fn fail(
        &self,
        held: &HeldJob,
        job: &Job,
        stored: Option<&ContinuationRecord>,
        error: &Error,
        kind: JobFailureKind,
    ) -> Result<Ended, Error> {
        let record = match stored {
            Some(_) => None,
            None => Some(initial(job, 1)?),
        };
        let failure = JobFailure {
            error: error.to_info(),
            kind,
        };
        let left = held
            .leave(job, record.as_ref(), Leave::Failed(failure))
            .await?;
        Ok(Ended::Left(job_state(&left.status)))
    }

    /// Cancels `ctx` when a `cancel_requested` reaches the run after
    /// `after`, then waits for ever: it ends only when the run does.
    async fn watch_for_cancel(&self, run: &RunId, mut after: u64, ctx: &ExecutionContext) {
        let waiting = ExecutionContext::new(ctx.correlation_id().clone());
        loop {
            if let Err(error) = self.events.wait_for(run, after, &waiting).await {
                tracing::warn!(%error, "waiting for a cancel failed; trying again");
                tokio::time::sleep(self.settings.poll).await;
                continue;
            }
            // Read the last seq before the cancels, so a cancel appended
            // between the two reads is above `upto` and read next time.
            let upto = self.events.last_seq(run).await.unwrap_or(after);
            match self
                .events
                .read_kinds(run, after, &[EventKind::CANCEL_REQUESTED], PAGE)
                .await
            {
                Ok(found) if !found.is_empty() => {
                    ctx.cancel();
                    std::future::pending::<()>().await;
                }
                Ok(_) => after = upto,
                Err(error) => {
                    tracing::warn!(%error, "reading for a cancel failed; trying again");
                    tokio::time::sleep(self.settings.poll).await;
                }
            }
        }
    }

    /// Wakes the parked jobs whose answer or cancel arrived (ADR-0031). A
    /// scan reads only those two kinds, after the last seq it has seen.
    async fn wake_parked(&self) -> Result<usize, Error> {
        let wake = WriteAuthority::Unfenced(UnfencedReason::JobWake);
        let mut woken = 0;
        for parked in self.jobs.parked(PARKED_PER_SCAN).await? {
            let job = &parked.job;
            let key = job.id.as_str().to_owned();
            let cached = self.seen.lock().ok().and_then(|s| s.get(&key).copied());
            let after = cached.unwrap_or(0).max(parked.checked_seq);
            let found = self
                .events
                .read_kinds(
                    &job.run_id,
                    after,
                    &[EventKind::APPROVAL_ANSWERED, EventKind::CANCEL_REQUESTED],
                    PAGE,
                )
                .await?;
            if let (Some(last), Ok(mut seen)) = (found.last(), self.seen.lock()) {
                seen.insert(key, last.seq.get());
            }
            for event in &found {
                let (reason, why) = match &event.payload {
                    EventPayload::CancelRequested { .. } => {
                        (WakeReason::CancelRequested, WOKEN_BY_CANCEL)
                    }
                    EventPayload::ApprovalAnswered { approval_id, .. } => (
                        WakeReason::ApprovalAnswered(approval_id.clone()),
                        WOKEN_BY_ANSWER,
                    ),
                    _ => continue,
                };
                if !self.jobs.wake(&job.id, &reason, Some(&wake)).await? {
                    continue;
                }
                let from = job_state(&job.status);
                let to = JobStatus::Queued {
                    checkpoint: named_checkpoint(&job.status).cloned(),
                };
                transition::record(
                    self.events.as_ref(),
                    self.clock.as_ref(),
                    &job.run_id,
                    &correlation(&job.id, job.attempt)?,
                    transition::changed(Some(from), to, why),
                    &wake,
                )
                .await?;
                woken += 1;
                break;
            }
        }
        Ok(woken)
    }

    async fn record(
        &self,
        held: &HeldJob,
        run: &RunId,
        payload: EventPayload,
    ) -> Result<Event, Error> {
        transition::record(
            self.events.as_ref(),
            self.clock.as_ref(),
            run,
            held.correlation_id(),
            payload,
            held.authority(),
        )
        .await
    }

    async fn read_kinds(
        &self,
        run: &RunId,
        after: u64,
        kinds: &[EventKind],
    ) -> Result<Vec<Event>, Error> {
        let mut all = Vec::new();
        let mut after = after;
        loop {
            let page = self.events.read_kinds(run, after, kinds, PAGE).await?;
            let Some(tail) = page.last() else {
                return Ok(all);
            };
            after = tail.seq.get();
            all.extend(page);
        }
    }

    /// The earliest answer to `approval_id`.
    async fn answer(
        &self,
        run: &RunId,
        approval_id: &ApprovalId,
    ) -> Result<Option<contracts::types::ApprovalAnswer>, Error> {
        Ok(self
            .read_kinds(run, 0, &[EventKind::APPROVAL_ANSWERED])
            .await?
            .into_iter()
            .find_map(|e| match e.payload {
                EventPayload::ApprovalAnswered {
                    approval_id: answered,
                    answer,
                    ..
                } if &answered == approval_id => Some(answer),
                _ => None,
            }))
    }

    async fn last_approval(&self, run: &RunId) -> Result<Option<ApprovalId>, Error> {
        Ok(self
            .read_kinds(run, 0, &[EventKind::APPROVAL_REQUESTED])
            .await?
            .into_iter()
            .rev()
            .find_map(|e| match e.payload {
                EventPayload::ApprovalRequested { approval_id, .. } => Some(approval_id),
                _ => None,
            }))
    }
}

/// The reason of the `job_state_changed` at `seq`.
async fn transition_reason(
    events: &dyn EventLog,
    run: &RunId,
    seq: u64,
) -> Result<Option<String>, Error> {
    let found = events.read(run, seq.saturating_sub(1), 1).await?;
    Ok(found.into_iter().find_map(|e| match e.payload {
        EventPayload::JobStateChanged { reason, .. } => reason.map(|r| r.as_str().to_owned()),
        _ => None,
    }))
}

/// The checkpoint a status names, if it names one.
fn named_checkpoint(status: &JobStatus) -> Option<&contracts::types::CheckpointRef> {
    match status {
        JobStatus::Queued { checkpoint } => checkpoint.as_ref(),
        JobStatus::Running => None,
        JobStatus::Waiting { checkpoint, .. }
        | JobStatus::Suspended { checkpoint, .. }
        | JobStatus::Completed { checkpoint, .. }
        | JobStatus::Cancelled { checkpoint }
        | JobStatus::Failed { checkpoint, .. } => Some(checkpoint),
    }
}

fn initial(job: &Job, checkpoint: u64) -> Result<ContinuationRecord, Error> {
    continuation::initial(job, checkpoint.max(1))
        .map_err(|e| Error::new(ErrorCode::Internal, e.to_string()))
}

fn next(checkpoint: NonZeroU64) -> NonZeroU64 {
    checkpoint.checked_add(1).unwrap_or(checkpoint)
}

/// `corr-{job}-{attempt}`: the correlation id of one attempt's writes.
fn correlation(job: &JobId, attempt: u64) -> Result<CorrelationId, Error> {
    format!("corr-{}-{attempt}", job.as_str())
        .parse()
        .map_err(|_| Error::new(ErrorCode::Internal, "a generated correlation id is invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> SchedulerSettings {
        SchedulerSettings {
            holder: "worker-1".parse().expect("id"),
            lease: Duration::from_secs(30),
            heartbeat: Duration::from_secs(10),
            poll: Duration::from_millis(1000),
            poll_jitter: Duration::from_millis(100),
            max_attempts: 3,
            backoff_min: Duration::from_secs(5),
            backoff_max: Duration::from_secs(60),
        }
    }

    #[test]
    fn the_backoff_doubles_up_to_its_maximum() {
        let s = settings();
        let delays: Vec<u64> = (1..=6).map(|n| s.backoff(n).as_secs()).collect();
        assert_eq!(delays, [5, 10, 20, 40, 60, 60]);
        assert_eq!(s.backoff(u64::MAX), Duration::from_secs(60));
    }

    #[test]
    fn polls_stay_within_the_jitter_and_out_of_step() {
        let draws = |holder: &str| -> Vec<u128> {
            let mut jitter = Jitter::seeded(&holder.parse().expect("id"));
            (0..200)
                .map(|_| {
                    jitter
                        .next(Duration::from_millis(1000), Duration::from_millis(100))
                        .as_millis()
                })
                .collect()
        };
        let a = draws("worker-1");
        assert!(a.iter().all(|ms| (900..=1100).contains(ms)), "{a:?}");
        assert!(a.iter().any(|ms| *ms != 1000), "no jitter");
        assert_ne!(a, draws("worker-2"), "two workers poll in step");
        let mut none = Jitter::seeded(&"worker-1".parse().expect("id"));
        assert_eq!(
            none.next(Duration::from_millis(1000), Duration::ZERO),
            Duration::from_millis(1000)
        );
    }
}
