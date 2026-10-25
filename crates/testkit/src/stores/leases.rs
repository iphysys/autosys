use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use contracts::types::{Job, JobState};
use contracts::{Clock, Error, FenceToken, FoundLease, StaleFence, SystemClock, lease_lost};

/// A job as the in-memory job store keeps it, with its lease epoch.
pub(crate) struct Leased {
    pub(crate) job: Job,
    pub(crate) epoch: u64,
}

pub(crate) type Jobs = BTreeMap<String, Leased>;

struct LeaseTable {
    clock: Arc<dyn Clock>,
    jobs: Mutex<Jobs>,
}

/// The fence state the in-memory stores share (ADR-0032): the job store's
/// jobs and their epochs, on the job store's clock, which stands in for the
/// database clock (ADR-0023). Cloning shares the state.
///
/// A store checks a fence and makes its write while holding this table's
/// lock, so no claim comes between the two. The lock order is always this
/// table, then the store's own data.
#[derive(Clone)]
pub struct Leases {
    table: Arc<LeaseTable>,
}

impl Leases {
    /// Empty fence state on `clock`. Until a job store on it claims a job,
    /// every fence is refused.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            table: Arc::new(LeaseTable {
                clock,
                jobs: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Fence state no job store uses: unfenced reasons still work, and every
    /// fence is refused.
    pub fn detached() -> Self {
        Self::new(Arc::new(SystemClock))
    }

    pub(crate) fn clock(&self) -> &dyn Clock {
        self.table.clock.as_ref()
    }

    pub(crate) fn jobs(&self) -> MutexGuard<'_, Jobs> {
        self.table.jobs.lock().unwrap()
    }

    /// Runs `write` with the table locked, if `fence` is current; refuses it
    /// with `lease_lost` otherwise. Without a fence, because an unfenced
    /// reason permitted the write, it just runs `write`.
    pub(crate) fn guard<T>(
        &self,
        fence: Option<&FenceToken>,
        write: impl FnOnce(&mut Jobs) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut jobs = self.jobs();
        if let Some(fence) = fence {
            check(&jobs, fence, self.clock().now())?;
        }
        write(&mut jobs)
    }
}

impl Default for Leases {
    fn default() -> Self {
        Self::detached()
    }
}

/// The Postgres predicate: the job is running under the fence's holder and
/// epoch, and its lease has not expired at `now`.
pub(crate) fn check(jobs: &Jobs, fence: &FenceToken, now: DateTime<Utc>) -> Result<(), Error> {
    let found = jobs.get(fence.job_id().as_str());
    let current = found.is_some_and(|leased| {
        leased.epoch == fence.epoch()
            && leased.job.state == JobState::Running
            && leased
                .job
                .lease
                .as_ref()
                .is_some_and(|l| &l.holder == fence.holder() && l.expires_at.0 > now)
    });
    if current {
        return Ok(());
    }
    Err(lease_lost(StaleFence::new(
        fence,
        found.map(|leased| FoundLease {
            epoch: leased.epoch,
            holder: leased.job.lease.as_ref().map(|l| l.holder.clone()),
            running: leased.job.state == JobState::Running,
        }),
    )))
}
