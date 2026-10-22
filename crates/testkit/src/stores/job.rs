use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use contracts::ids::{Identifier, JobId, RunId};
use contracts::types::{Job, JobState, Lease, Timestamp, WakeCondition};
use contracts::{Clock, Error, JobStore, WakeReason};

use super::{conflict, invalid, not_found};

pub struct InMemoryJobStore {
    clock: Arc<dyn Clock>,
    jobs: Mutex<BTreeMap<String, Job>>,
}

impl InMemoryJobStore {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            jobs: Mutex::new(BTreeMap::new()),
        }
    }
}

fn lease_delta(lease_for: Duration) -> Result<TimeDelta, Error> {
    TimeDelta::from_std(lease_for).map_err(|_| invalid("lease duration is too long"))
}

fn lease_is_live(lease: &Lease, now: DateTime<Utc>) -> bool {
    lease.expires_at.0 > now
}

/// The job's lease if `holder` holds it and it has not expired.
fn held_by<'a>(job: &'a Job, holder: &Identifier, now: DateTime<Utc>) -> Option<&'a Lease> {
    job.lease
        .as_ref()
        .filter(|l| job.state == JobState::Running && &l.holder == holder && lease_is_live(l, now))
}

#[async_trait]
impl JobStore for InMemoryJobStore {
    async fn insert(&self, job: &Job) -> Result<(), Error> {
        if job.state != JobState::Queued || job.lease.is_some() {
            return Err(invalid("a new job must be queued and have no lease"));
        }
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.contains_key(job.id.as_str()) {
            return Err(conflict(format!("job {} already exists", job.id.as_str())));
        }
        jobs.insert(job.id.as_str().to_owned(), job.clone());
        Ok(())
    }

    async fn get(&self, id: &JobId) -> Result<Job, Error> {
        self.jobs
            .lock()
            .unwrap()
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| not_found("job", id.as_str()))
    }

    async fn find_by_run(&self, run_id: &RunId) -> Result<Option<Job>, Error> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .values()
            .find(|j| &j.run_id == run_id)
            .cloned())
    }

    async fn claim(&self, holder: &Identifier, lease_for: Duration) -> Result<Option<Job>, Error> {
        let delta = lease_delta(lease_for)?;
        let now = self.clock.now();
        let mut jobs = self.jobs.lock().unwrap();
        let claimable = jobs
            .values_mut()
            .filter(|j| match j.state {
                JobState::Queued => true,
                JobState::Running => j.lease.as_ref().is_none_or(|l| !lease_is_live(l, now)),
                _ => false,
            })
            .min_by(|a, b| (a.created_at.0, a.id.as_str()).cmp(&(b.created_at.0, b.id.as_str())));
        let Some(job) = claimable else {
            return Ok(None);
        };
        job.state = JobState::Running;
        job.attempt += 1;
        job.updated_at = Timestamp(now);
        job.lease = Some(Lease {
            holder: holder.clone(),
            acquired_at: Timestamp(now),
            expires_at: Timestamp(now + delta),
            heartbeat_at: Timestamp(now),
        });
        Ok(Some(job.clone()))
    }

    async fn renew_lease(
        &self,
        id: &JobId,
        holder: &Identifier,
        lease_for: Duration,
    ) -> Result<Job, Error> {
        let delta = lease_delta(lease_for)?;
        let now = self.clock.now();
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs
            .get_mut(id.as_str())
            .ok_or_else(|| not_found("job", id.as_str()))?;
        if held_by(job, holder, now).is_none() {
            return Err(conflict(format!(
                "{} does not hold the lease on {}",
                holder.as_str(),
                id.as_str()
            )));
        }
        if let Some(lease) = job.lease.as_mut() {
            lease.expires_at = Timestamp(now + delta);
            lease.heartbeat_at = Timestamp(now);
        }
        job.updated_at = Timestamp(now);
        Ok(job.clone())
    }

    async fn save(&self, job: &Job, holder: &Identifier) -> Result<(), Error> {
        let now = self.clock.now();
        let mut jobs = self.jobs.lock().unwrap();
        let stored = jobs
            .get_mut(job.id.as_str())
            .ok_or_else(|| not_found("job", job.id.as_str()))?;
        let Some(lease) = held_by(stored, holder, now).cloned() else {
            return Err(conflict(format!(
                "{} does not hold the lease on {}",
                holder.as_str(),
                job.id.as_str()
            )));
        };
        let attempt = stored.attempt;
        *stored = job.clone();
        stored.attempt = attempt;
        stored.updated_at = Timestamp(now);
        stored.lease = (job.state == JobState::Running).then_some(lease);
        Ok(())
    }

    async fn wake(&self, id: &JobId, reason: &WakeReason) -> Result<bool, Error> {
        let now = self.clock.now();
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs
            .get_mut(id.as_str())
            .ok_or_else(|| not_found("job", id.as_str()))?;
        if !matches!(job.state, JobState::Waiting | JobState::Suspended) {
            return Ok(false);
        }
        let matches = match (reason, &job.wake_condition) {
            (WakeReason::CancelRequested, _) => true,
            (
                WakeReason::ApprovalAnswered(answered),
                Some(WakeCondition::ApprovalAnswered(waiting_for)),
            ) => answered == waiting_for,
            (WakeReason::ApprovalAnswered(_), None) => false,
        };
        if matches {
            job.state = JobState::Queued;
            job.wake_condition = None;
            job.updated_at = Timestamp(now);
        }
        Ok(matches)
    }
}
