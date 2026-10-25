//! A job this worker holds: its fence, where its checkpoints go, and how it
//! leaves running.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use contracts::ids::CorrelationId;
use contracts::types::{
    CompletedStopReason, ContinuationRecord, Job, JobFailure, JobState, JobStatus, StopReason,
    WakeCondition,
};
use contracts::{
    Clock, Error, ErrorCode, EventLog, FenceToken, JobStore, JobWrite, RecordWrite, WriteAuthority,
};

use contracts::ids::ApprovalId;

use super::transition::{self, RELEASED, RETRY};
use crate::continuation::checkpoint_ref;
use crate::query_loop::CheckpointSink;

/// Where a job goes when it leaves running.
#[derive(Clone, Debug)]
pub(crate) enum Leave {
    /// Waits for the human answer to `approval_id`.
    Suspended(ApprovalId),
    /// The run stopped for good.
    Stopped(StopReason),
    Failed(JobFailure),
    /// Back to the queue after a failed attempt, claimable after `delay`.
    Retry(Duration),
    /// Back to the queue because this worker is shutting down; not a failed
    /// attempt.
    Released,
}

impl Leave {
    fn reason(&self) -> String {
        match self {
            Leave::Suspended(_) => "the run waits for an approval".to_owned(),
            Leave::Stopped(reason) => format!("the run stopped: {reason}"),
            Leave::Failed(failure) => format!("the job failed: {}", failure.error.message.as_str()),
            Leave::Retry(_) => RETRY.to_owned(),
            Leave::Released => RELEASED.to_owned(),
        }
    }
}

/// A claimed job: every write under its fence (ADR-0032).
pub(crate) struct HeldJob {
    jobs: Arc<dyn JobStore>,
    events: Arc<dyn EventLog>,
    clock: Arc<dyn Clock>,
    fence: FenceToken,
    authority: WriteAuthority,
    correlation_id: CorrelationId,
    /// The job's `progress.last_event_seq` when it was claimed: a
    /// checkpoint past it means the run moved on, which clears the failed
    /// attempts.
    claimed_at_seq: u64,
    last: Mutex<Option<Job>>,
}

impl HeldJob {
    pub(crate) fn new(
        jobs: Arc<dyn JobStore>,
        events: Arc<dyn EventLog>,
        clock: Arc<dyn Clock>,
        fence: FenceToken,
        correlation_id: CorrelationId,
        claimed_at_seq: u64,
    ) -> Self {
        Self {
            jobs,
            events,
            clock,
            authority: WriteAuthority::Fenced(fence.clone()),
            fence,
            correlation_id,
            claimed_at_seq,
            last: Mutex::new(None),
        }
    }

    pub(crate) fn authority(&self) -> &WriteAuthority {
        &self.authority
    }

    pub(crate) fn correlation_id(&self) -> &CorrelationId {
        &self.correlation_id
    }

    /// The job as its last checkpoint saved it, if one was saved.
    pub(crate) fn last_saved(&self) -> Option<Job> {
        self.last.lock().ok().and_then(|last| last.clone())
    }

    fn running(&self, job: &Job) -> Job {
        let mut job = job.clone();
        job.status = JobStatus::Running;
        if job.progress.last_event_seq > self.claimed_at_seq {
            job.failed_attempts = 0;
        }
        job
    }

    /// Leaves running for `to` (ADR-0031): saves `record` as the job's
    /// checkpoint, if one is given, while the fence still holds; appends
    /// `job_state_changed` naming the checkpoint the job rests on; then
    /// saves the new status, which ends the fence. With no `record`, the
    /// stored one is named. Returns the job as saved.
    pub(crate) async fn leave(
        &self,
        job: &Job,
        record: Option<&ContinuationRecord>,
        to: Leave,
    ) -> Result<Job, Error> {
        let named = match record {
            Some(record) => {
                self.checkpoint(job, record).await?;
                Some(record.clone())
            }
            None => self.jobs.continuation(&job.id).await?,
        };
        let checkpoint = named.as_ref().map(checkpoint_ref).transpose()?;
        let needs = |what: &str| {
            Error::new(
                ErrorCode::Internal,
                format!("{what} needs a checkpoint, and the job has none"),
            )
        };
        let (status, retry_after) = match &to {
            Leave::Suspended(approval_id) => (
                JobStatus::Suspended {
                    wake_condition: WakeCondition::ApprovalAnswered(approval_id.clone()),
                    checkpoint: checkpoint.ok_or_else(|| needs("suspending"))?,
                },
                None,
            ),
            Leave::Stopped(StopReason::Cancelled) => (
                JobStatus::Cancelled {
                    checkpoint: checkpoint.ok_or_else(|| needs("cancelling"))?,
                },
                None,
            ),
            Leave::Stopped(reason) => (
                JobStatus::Completed {
                    stop_reason: completed(*reason)?,
                    checkpoint: checkpoint.ok_or_else(|| needs("completing"))?,
                },
                None,
            ),
            Leave::Failed(failure) => (
                JobStatus::Failed {
                    failure: failure.clone(),
                    checkpoint: checkpoint.ok_or_else(|| needs("failing"))?,
                },
                None,
            ),
            Leave::Retry(delay) => (JobStatus::Queued { checkpoint }, Some(*delay)),
            Leave::Released => (JobStatus::Queued { checkpoint }, None),
        };
        let event = transition::record(
            self.events.as_ref(),
            self.clock.as_ref(),
            &job.run_id,
            &self.correlation_id,
            transition::changed(Some(JobState::Running), status.clone(), &to.reason()),
            &self.authority,
        )
        .await?;
        let mut left = self.running(job);
        left.status = status;
        left.progress.last_event_seq = event.seq.get();
        self.finish(&left, retry_after).await?;
        Ok(left)
    }

    /// Saves `job`, already in its new status, keeping the stored record:
    /// the last step of leaving running, also for a transition a dead
    /// worker decided.
    pub(crate) async fn finish(
        &self,
        job: &Job,
        retry_after: Option<Duration>,
    ) -> Result<(), Error> {
        self.jobs
            .save(
                &JobWrite {
                    job,
                    continuation: RecordWrite::Keep,
                    retry_after,
                },
                &self.fence,
            )
            .await
    }
}

#[async_trait]
impl CheckpointSink for HeldJob {
    async fn checkpoint(&self, job: &Job, record: &ContinuationRecord) -> Result<(), Error> {
        let running = self.running(job);
        self.jobs
            .save(
                &JobWrite {
                    job: &running,
                    continuation: RecordWrite::Replace(record),
                    retry_after: None,
                },
                &self.fence,
            )
            .await?;
        if let Ok(mut last) = self.last.lock() {
            *last = Some(running);
        }
        Ok(())
    }
}

fn completed(reason: StopReason) -> Result<CompletedStopReason, Error> {
    match reason {
        StopReason::Success => Ok(CompletedStopReason::Success),
        StopReason::Failure => Ok(CompletedStopReason::Failure),
        StopReason::BudgetExhausted => Ok(CompletedStopReason::BudgetExhausted),
        StopReason::Cancelled | StopReason::ApprovalRequired => Err(Error::new(
            ErrorCode::Internal,
            format!("{reason} is not a completed job's stop reason"),
        )),
    }
}
