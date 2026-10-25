//! Fenced contexts for tests (ADR-0032). Outside testkit only the scheduler
//! gives a context its write authority (arch-check), so the tests of other
//! crates get theirs here, as they get SDR storage from
//! [`crate::stores::sdr_storage`].

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use contracts::ids::{CorrelationId, Identifier};
use contracts::{Claimed, Clock, ExecutionContext, UnfencedReason, WriteAuthority};

use crate::builders::{id, job};
use crate::stores::{InMemoryJobStore, Leases};

/// The holder a [`LeasedJob`] claims for.
pub const HOLDER: &str = "worker-1";

/// A year: long enough that a test advancing its clock never loses the lease
/// by accident.
pub const LONG_LEASE: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// An in-memory job store with one job, claimed. Build the stores a test
/// writes to with `fenced_by(leased.leases())`, and write with
/// [`LeasedJob::ctx`].
pub struct LeasedJob {
    jobs: Arc<InMemoryJobStore>,
    claimed: Claimed,
}

impl LeasedJob {
    /// Job `job-1` of `run_id`, claimed by [`HOLDER`] for [`LONG_LEASE`].
    pub fn new(clock: Arc<dyn Clock>, run_id: &str) -> Self {
        Self::with_lease(clock, run_id, &id(HOLDER), LONG_LEASE)
    }

    /// Job `job-1` of `run_id`, claimed by `holder` for `lease_for`: a short
    /// lease lets a test expire it and claim again as another worker.
    pub fn with_lease(
        clock: Arc<dyn Clock>,
        run_id: &str,
        holder: &Identifier,
        lease_for: Duration,
    ) -> Self {
        let created: DateTime<Utc> = clock.now();
        let jobs = Arc::new(InMemoryJobStore::new(clock));
        jobs.insert_now(
            &job("job-1", run_id, created),
            Some(&WriteAuthority::Unfenced(UnfencedReason::JobInsert)),
        )
        .expect("insert the leased job");
        let claimed = jobs
            .claim_now(holder, lease_for)
            .expect("claim the leased job")
            .expect("the leased job is claimable");
        Self { jobs, claimed }
    }

    pub fn jobs(&self) -> &Arc<InMemoryJobStore> {
        &self.jobs
    }

    /// The fence state the stores of the test share.
    pub fn leases(&self) -> Leases {
        self.jobs.leases()
    }

    pub fn claimed(&self) -> &Claimed {
        &self.claimed
    }

    /// A root context fenced by this claim.
    pub fn ctx(&self, correlation_id: &str) -> ExecutionContext {
        fenced_ctx(&self.claimed, id(correlation_id))
    }
}

/// A root context fenced by `claimed`.
pub fn fenced_ctx(claimed: &Claimed, correlation_id: CorrelationId) -> ExecutionContext {
    ExecutionContext::builder(correlation_id)
        .authorized_by(WriteAuthority::Fenced(claimed.fence.clone()))
        .build()
}

/// A root context for a write from outside any job, such as a client's
/// cancel or the creation of a project.
pub fn unfenced_ctx(reason: UnfencedReason, correlation_id: CorrelationId) -> ExecutionContext {
    ExecutionContext::builder(correlation_id)
        .authorized_by(WriteAuthority::Unfenced(reason))
        .build()
}
