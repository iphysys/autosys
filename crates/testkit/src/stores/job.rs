use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::TimeDelta;
use contracts::ids::{Identifier, JobId, RunId};
use contracts::types::{
    ContinuationRecord, Job, JobState, JobStatus, Lease, Timestamp, WakeCondition,
};
use contracts::{
    Claimed, Clock, Error, FenceToken, JobStore, JobWrite, Parked, RecordWrite, StoreWrite,
    WakeReason, WriteAuthority, authorize, job_state,
};

use super::leases::{Leased, Leases, check};
use super::{conflict, invalid, not_found};

/// [`JobStore`] in memory. Its jobs live in its [`Leases`], which the other
/// in-memory stores share to check fences.
pub struct InMemoryJobStore {
    leases: Leases,
}

impl InMemoryJobStore {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            leases: Leases::new(clock),
        }
    }

    /// The fence state, for the stores this one fences
    /// (`InMemoryEventLog::fenced_by` and the others).
    pub fn leases(&self) -> Leases {
        self.leases.clone()
    }

    /// [`JobStore::insert`], without awaiting.
    pub(crate) fn insert_now(
        &self,
        job: &Job,
        authority: Option<&WriteAuthority>,
    ) -> Result<(), Error> {
        let fence = authorize(authority, StoreWrite::JobInsert)?;
        if job.status != (JobStatus::Queued { checkpoint: None })
            || job.lease.is_some()
            || job.not_before.is_some()
        {
            return Err(invalid(
                "a new job must be queued, without a checkpoint, a delay or a lease",
            ));
        }
        self.leases.guard(fence, |jobs| {
            if jobs.contains_key(job.id.as_str()) {
                return Err(conflict(format!("job {} already exists", job.id.as_str())));
            }
            jobs.insert(
                job.id.as_str().to_owned(),
                Leased {
                    job: job.clone(),
                    epoch: 0,
                    continuation: None,
                    checked_seq: 0,
                },
            );
            Ok(())
        })
    }

    /// [`JobStore::claim`], without awaiting.
    pub(crate) fn claim_now(
        &self,
        holder: &Identifier,
        lease_for: Duration,
    ) -> Result<Option<Claimed>, Error> {
        let delta = lease_delta(lease_for)?;
        let now = self.leases.clock().now();
        let mut jobs = self.leases.jobs();
        let claimable = jobs
            .values_mut()
            .filter(|l| match job_state(&l.job.status) {
                JobState::Queued => l.job.not_before.as_ref().is_none_or(|t| t.0 <= now),
                JobState::Running => l.job.lease.as_ref().is_none_or(|l| l.expires_at.0 <= now),
                _ => false,
            })
            .min_by(|a, b| {
                (a.job.created_at.0, a.job.id.as_str())
                    .cmp(&(b.job.created_at.0, b.job.id.as_str()))
            });
        let Some(leased) = claimable else {
            return Ok(None);
        };
        let job = &mut leased.job;
        let interrupted = job_state(&job.status) == JobState::Running;
        job.status = JobStatus::Running;
        job.attempt += 1;
        job.updated_at = Timestamp(now);
        job.not_before = None;
        job.lease = Some(Lease {
            holder: holder.clone(),
            acquired_at: Timestamp(now),
            expires_at: Timestamp(now + delta),
            heartbeat_at: Timestamp(now),
        });
        leased.epoch += 1;
        let fence = FenceToken::issue_for_claim(job.id.clone(), holder.clone(), leased.epoch);
        Ok(Some(Claimed::new(job.clone(), fence, interrupted)))
    }
}

fn lease_delta(lease_for: Duration) -> Result<TimeDelta, Error> {
    TimeDelta::from_std(lease_for).map_err(|_| invalid("lease duration is too long"))
}

fn is_parked(job: &Job) -> bool {
    matches!(
        job_state(&job.status),
        JobState::Waiting | JobState::Suspended
    )
}

#[async_trait]
impl JobStore for InMemoryJobStore {
    async fn insert(&self, job: &Job, authority: Option<&WriteAuthority>) -> Result<(), Error> {
        self.insert_now(job, authority)
    }

    async fn get(&self, id: &JobId) -> Result<Job, Error> {
        self.leases
            .jobs()
            .get(id.as_str())
            .map(|l| l.job.clone())
            .ok_or_else(|| not_found("job", id.as_str()))
    }

    async fn find_by_run(&self, run_id: &RunId) -> Result<Option<Job>, Error> {
        Ok(self
            .leases
            .jobs()
            .values()
            .find(|l| &l.job.run_id == run_id)
            .map(|l| l.job.clone()))
    }

    async fn continuation(&self, id: &JobId) -> Result<Option<ContinuationRecord>, Error> {
        self.leases
            .jobs()
            .get(id.as_str())
            .map(|l| l.continuation.clone())
            .ok_or_else(|| not_found("job", id.as_str()))
    }

    async fn parked(&self, limit: usize) -> Result<Vec<Parked>, Error> {
        let jobs = self.leases.jobs();
        let mut parked: Vec<&Leased> = jobs.values().filter(|l| is_parked(&l.job)).collect();
        parked.sort_by(|a, b| {
            (a.job.updated_at.0, a.job.id.as_str()).cmp(&(b.job.updated_at.0, b.job.id.as_str()))
        });
        Ok(parked
            .into_iter()
            .take(limit)
            .map(|l| Parked {
                job: l.job.clone(),
                checked_seq: l.checked_seq,
            })
            .collect())
    }

    async fn claim(
        &self,
        holder: &Identifier,
        lease_for: Duration,
    ) -> Result<Option<Claimed>, Error> {
        self.claim_now(holder, lease_for)
    }

    async fn renew_lease(&self, fence: &FenceToken, lease_for: Duration) -> Result<Job, Error> {
        let delta = lease_delta(lease_for)?;
        let now = self.leases.clock().now();
        let mut jobs = self.leases.jobs();
        check(&jobs, fence, now)?;
        let leased = jobs
            .get_mut(fence.job_id().as_str())
            .ok_or_else(|| not_found("job", fence.job_id().as_str()))?;
        if let Some(lease) = leased.job.lease.as_mut() {
            lease.expires_at = Timestamp(now + delta);
            lease.heartbeat_at = Timestamp(now);
        }
        leased.job.updated_at = Timestamp(now);
        Ok(leased.job.clone())
    }

    async fn save(&self, write: &JobWrite<'_>, fence: &FenceToken) -> Result<(), Error> {
        let job = write.job;
        if fence.job_id() != &job.id {
            return Err(invalid(format!(
                "the fence is for {}, not {}",
                fence.job_id().as_str(),
                job.id.as_str()
            )));
        }
        let state = job_state(&job.status);
        if write.retry_after.is_some() && state != JobState::Queued {
            return Err(invalid(format!(
                "a retry delay needs a queued job, not {state}"
            )));
        }
        let now = self.leases.clock().now();
        let not_before = write
            .retry_after
            .map(|delay| {
                TimeDelta::from_std(delay)
                    .map(|delta| Timestamp(now + delta))
                    .map_err(|_| invalid("retry delay is too long"))
            })
            .transpose()?;
        let mut jobs = self.leases.jobs();
        check(&jobs, fence, now)?;
        let leased = jobs
            .get_mut(job.id.as_str())
            .ok_or_else(|| not_found("job", job.id.as_str()))?;
        let stored = &mut leased.job;
        let lease = stored.lease.clone();
        let attempt = stored.attempt;
        *stored = job.clone();
        stored.attempt = attempt;
        stored.updated_at = Timestamp(now);
        stored.not_before = not_before;
        stored.lease = if state == JobState::Running {
            lease
        } else {
            None
        };
        if let RecordWrite::Replace(record) = write.continuation {
            leased.continuation = Some(record.clone());
        }
        if is_parked(job) {
            leased.checked_seq = job.progress.last_event_seq;
        }
        Ok(())
    }

    async fn wake(
        &self,
        id: &JobId,
        reason: &WakeReason,
        authority: Option<&WriteAuthority>,
    ) -> Result<bool, Error> {
        let fence = authorize(authority, StoreWrite::JobWake)?;
        let now = self.leases.clock().now();
        self.leases.guard(fence, |jobs| {
            let job = &mut jobs
                .get_mut(id.as_str())
                .ok_or_else(|| not_found("job", id.as_str()))?
                .job;
            let (wake_condition, checkpoint) = match &job.status {
                JobStatus::Waiting {
                    wake_condition,
                    checkpoint,
                }
                | JobStatus::Suspended {
                    wake_condition,
                    checkpoint,
                } => (wake_condition, checkpoint),
                _ => return Ok(false),
            };
            let matches = match (reason, wake_condition) {
                (WakeReason::CancelRequested, _) => true,
                (
                    WakeReason::ApprovalAnswered(answered),
                    WakeCondition::ApprovalAnswered(waiting_for),
                ) => answered == waiting_for,
            };
            if matches {
                job.status = JobStatus::Queued {
                    checkpoint: Some(checkpoint.clone()),
                };
                job.not_before = None;
                job.updated_at = Timestamp(now);
            }
            Ok(matches)
        })
    }
}
