//! Write authority for tests (ADR-0032). Outside testkit only the scheduler
//! gives a context its write authority, and only contracts, testkit,
//! bootstrap and the scheduler name `UnfencedReason` (arch-check). So the
//! tests of other crates get fenced contexts and each unfenced authority
//! here, as they get SDR storage from [`crate::stores::sdr_storage`].

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
        jobs.insert_now(&job("job-1", run_id, created), Some(&job_insert()))
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

/// A root context with `authority`, normally one of the unfenced ones
/// below, for a write from outside any job.
pub fn unfenced_ctx(authority: WriteAuthority, correlation_id: CorrelationId) -> ExecutionContext {
    ExecutionContext::builder(correlation_id)
        .authorized_by(authority)
        .build()
}

/// Submitting a job: `JobStore::insert`.
pub fn job_insert() -> WriteAuthority {
    WriteAuthority::Unfenced(UnfencedReason::JobInsert)
}

/// Waking a waiting or suspended job: `JobStore::wake`.
pub fn job_wake() -> WriteAuthority {
    WriteAuthority::Unfenced(UnfencedReason::JobWake)
}

/// A client's command: appending `approval_answered` or `cancel_requested`.
pub fn api_command() -> WriteAuthority {
    WriteAuthority::Unfenced(UnfencedReason::ApiCommand)
}

/// Creating a project: appending SDR version 0.
pub fn project_create() -> WriteAuthority {
    WriteAuthority::Unfenced(UnfencedReason::ProjectCreate)
}
