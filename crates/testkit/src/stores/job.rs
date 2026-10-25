use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::TimeDelta;
use contracts::ids::{Identifier, JobId, RunId};
use contracts::types::{Job, JobState, JobStatus, Lease, Timestamp, WakeCondition};
use contracts::{
    Claimed, Clock, Error, FenceToken, JobStore, StoreWrite, WakeReason, WriteAuthority, authorize,
    job_state,
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
        if job.status != (JobStatus::Queued { checkpoint: None }) || job.lease.is_some() {
            return Err(invalid(
                "a new job must be queued, without a checkpoint or a lease",
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
                JobState::Queued => true,
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
        job.status = JobStatus::Running;
        job.attempt += 1;
        job.updated_at = Timestamp(now);
        job.lease = Some(Lease {
            holder: holder.clone(),
            acquired_at: Timestamp(now),
            expires_at: Timestamp(now + delta),
            heartbeat_at: Timestamp(now),
        });
        leased.epoch += 1;
        let fence = FenceToken::issue_for_claim(job.id.clone(), holder.clone(), leased.epoch);
        Ok(Some(Claimed::new(job.clone(), fence)))
    }
}

fn lease_delta(lease_for: Duration) -> Result<TimeDelta, Error> {
    TimeDelta::from_std(lease_for).map_err(|_| invalid("lease duration is too long"))
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

    async fn save(&self, job: &Job, fence: &FenceToken) -> Result<(), Error> {
        if fence.job_id() != &job.id {
            return Err(invalid(format!(
                "the fence is for {}, not {}",
                fence.job_id().as_str(),
                job.id.as_str()
            )));
        }
        let now = self.leases.clock().now();
        let mut jobs = self.leases.jobs();
        check(&jobs, fence, now)?;
        let stored = &mut jobs
            .get_mut(job.id.as_str())
            .ok_or_else(|| not_found("job", job.id.as_str()))?
            .job;
        let lease = stored.lease.clone();
        let attempt = stored.attempt;
        *stored = job.clone();
        stored.attempt = attempt;
        stored.updated_at = Timestamp(now);
        stored.lease = if job_state(&job.status) == JobState::Running {
            lease
        } else {
            None
        };
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
                job.updated_at = Timestamp(now);
            }
            Ok(matches)
        })
    }
}
