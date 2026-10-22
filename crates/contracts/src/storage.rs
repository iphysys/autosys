//! Repository traits implemented by `synth-infra` (Postgres, ADR-0016) and by
//! testkit's in-memory stores.
//!
//! Lease expiry follows a single clock per store (ADR-0023). The Postgres
//! stores read the database clock. The in-memory stores take a
//! [`crate::Clock`] at construction that stands in for it, so tests control
//! time.

use std::time::Duration;

use async_trait::async_trait;
use schema_types::{
    Actor, ArtifactMetadata, Event, EventPayload, Evidence, Job, SdrDocument, SdrVersionRecord,
    Timestamp,
};

use crate::context::ExecutionContext;
use crate::error::Error;
use crate::ids::{
    ApprovalId, ArtifactId, CorrelationId, EvidenceId, Identifier, JobId, ProjectId, RunId,
    SdrVersion,
};

/// Why a waiting or suspended job should be looked at again.
#[derive(Clone, Debug, PartialEq)]
pub enum WakeReason {
    ApprovalAnswered(ApprovalId),
    /// Wakes any waiting or suspended job, whatever its wake condition.
    CancelRequested,
}

#[async_trait]
pub trait JobStore: Send + Sync {
    /// Stores a new job. InvalidInput unless it is queued without a lease;
    /// Conflict if the id exists.
    async fn insert(&self, job: &Job) -> Result<(), Error>;

    /// NotFound if there is no such job.
    async fn get(&self, id: &JobId) -> Result<Job, Error>;

    async fn find_by_run(&self, run_id: &RunId) -> Result<Option<Job>, Error>;

    /// Atomically claims the oldest job (by `created_at`, then id) that is
    /// queued, or running with an expired lease. The claimed job is running,
    /// leased to `holder` for `lease_for`, with `attempt` incremented.
    /// Concurrent callers never receive the same job.
    async fn claim(&self, holder: &Identifier, lease_for: Duration) -> Result<Option<Job>, Error>;

    /// Extends the lease. Conflict if `holder` does not hold an unexpired lease.
    async fn renew_lease(
        &self,
        id: &JobId,
        holder: &Identifier,
        lease_for: Duration,
    ) -> Result<Job, Error>;

    /// Writes the job while `holder` holds an unexpired lease; Conflict
    /// otherwise. The store keeps its own lease and attempt values; a state
    /// other than running clears the lease.
    async fn save(&self, job: &Job, holder: &Identifier) -> Result<(), Error>;

    /// Moves a waiting or suspended job back to queued if `reason` matches its
    /// wake condition. Returns whether it woke. NotFound if there is no such job.
    async fn wake(&self, id: &JobId, reason: &WakeReason) -> Result<bool, Error>;
}

/// An event before the log assigns its sequence number.
#[derive(Clone, Debug, PartialEq)]
pub struct NewEvent {
    pub run_id: RunId,
    pub occurred_at: Timestamp,
    pub correlation_id: CorrelationId,
    pub actor: Actor,
    pub payload: EventPayload,
}

#[async_trait]
pub trait EventLog: Send + Sync {
    /// Appends with the run's next sequence number (the first is 1, with no
    /// gaps, even under concurrent appends) and returns the stored event.
    async fn append(&self, event: NewEvent) -> Result<Event, Error>;

    /// Events with `seq > after`, in ascending order, at most `limit`.
    async fn read(&self, run_id: &RunId, after: u64, limit: usize) -> Result<Vec<Event>, Error>;

    /// The last sequence number, or 0 if the run has no events.
    async fn last_seq(&self, run_id: &RunId) -> Result<u64, Error>;

    /// Resolves once the run has an event with `seq > after`. Returns
    /// `Cancelled` if `ctx` is cancelled first.
    async fn wait_for(
        &self,
        run_id: &RunId,
        after: u64,
        ctx: &ExecutionContext,
    ) -> Result<(), Error>;
}

/// Storage behind `SdrManager`. Only `bootstrap` constructs an implementation
/// and only `SdrManager` receives it (blueprint section 7.2).
#[async_trait]
pub trait SdrRepository: Send + Sync {
    /// The latest version, or `None` before version 0 exists.
    async fn head(&self, project_id: &ProjectId) -> Result<Option<SdrVersion>, Error>;

    async fn document(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
    ) -> Result<SdrDocument, Error>;

    async fn version_record(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
    ) -> Result<SdrVersionRecord, Error>;

    /// Stores a new version atomically. InvalidInput if the document and record
    /// disagree on project, version or parent. Conflict unless the version is
    /// head + 1 (0 when empty) and its parent is the current head.
    async fn append_version(
        &self,
        document: &SdrDocument,
        record: &SdrVersionRecord,
    ) -> Result<(), Error>;
}

#[async_trait]
pub trait EvidenceStore: Send + Sync {
    /// Evidence is immutable: storing identical content again is Ok; different
    /// content under an existing id is Conflict.
    async fn put(&self, evidence: &Evidence) -> Result<(), Error>;

    async fn get(&self, id: &EvidenceId) -> Result<Evidence, Error>;

    /// Ordered by `created_at`, then id.
    async fn list_for_run(&self, run_id: &RunId) -> Result<Vec<Evidence>, Error>;
}

#[async_trait]
pub trait ArtifactStore: Send + Sync {
    /// InvalidInput if the content's sha256 or size disagree with the
    /// metadata. Storing an identical artifact again is Ok; a different one
    /// under an existing id is Conflict.
    async fn put(&self, metadata: &ArtifactMetadata, content: &[u8]) -> Result<(), Error>;

    async fn metadata(&self, id: &ArtifactId) -> Result<ArtifactMetadata, Error>;

    async fn content(&self, id: &ArtifactId) -> Result<Vec<u8>, Error>;

    /// Ordered by `created_at`, then id.
    async fn list_for_run(&self, run_id: &RunId) -> Result<Vec<ArtifactMetadata>, Error>;
}
