//! Repository traits implemented by `synth-infra` (Postgres, ADR-0016) and by
//! testkit's in-memory stores.
//!
//! Lease expiry follows a single clock per store (ADR-0023). The Postgres
//! stores read the database clock. The in-memory stores take a
//! [`crate::Clock`] at construction that stands in for it, so tests control
//! time.
//!
//! Every write takes its authority (ADR-0032), normally
//! `ctx.write_authority()`. A store first applies [`crate::authorize`]; for a
//! fenced write it then checks, in the write's own transaction and before
//! any other condition, that the job is still running under the fence's
//! holder and epoch with an unexpired lease. A stale fence, a missing
//! authority and an unfenced reason that does not cover the write all fail
//! with `LeaseLost`. Reads take no authority.

use std::time::Duration;

use async_trait::async_trait;
use schema_types::{
    Actor, ArtifactMetadata, ContinuationRecord, Event, EventPayload, Evidence, Job, SdrDocument,
    SdrVersionRecord, Sha256, StepOutcome, StepRecord, Timestamp,
};

use crate::context::ExecutionContext;
use crate::error::Error;
use crate::events::EventKind;
use crate::fence::{FenceToken, WriteAuthority};
use crate::ids::{
    ApprovalId, ArtifactId, CorrelationId, EvidenceId, Identifier, JobId, ProjectId, RunId,
    SdrVersion, StepId, ToolName,
};

/// Why a waiting or suspended job should be looked at again.
#[derive(Clone, Debug, PartialEq)]
pub enum WakeReason {
    ApprovalAnswered(ApprovalId),
    /// Wakes any waiting or suspended job, whatever its wake condition.
    CancelRequested,
}

/// A job a claim leased, and the fence every write under that lease carries
/// (ADR-0032).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct Claimed {
    pub job: Job,
    pub fence: FenceToken,
    /// True when the job was claimed from running because its lease had
    /// expired: the attempt before this one was interrupted.
    pub interrupted: bool,
}

impl Claimed {
    pub fn new(job: Job, fence: FenceToken, interrupted: bool) -> Self {
        Self {
            job,
            fence,
            interrupted,
        }
    }
}

/// What [`JobStore::save`] does with the run's continuation record.
#[derive(Clone, Copy, Debug)]
pub enum RecordWrite<'a> {
    /// The stored record stays as it is.
    Keep,
    /// `record` replaces the stored one.
    Replace(&'a ContinuationRecord),
}

/// One fenced write of a job (ADR-0031): the job, its continuation record and
/// a requeue's delay, stored together.
#[derive(Clone, Copy, Debug)]
pub struct JobWrite<'a> {
    pub job: &'a Job,
    pub continuation: RecordWrite<'a>,
    /// A requeue's delay: the store sets `not_before` this far past its own
    /// clock. `None` clears it.
    pub retry_after: Option<Duration>,
}

impl<'a> JobWrite<'a> {
    /// `job` alone: the stored record stays, and there is no delay.
    pub fn keep(job: &'a Job) -> Self {
        Self {
            job,
            continuation: RecordWrite::Keep,
            retry_after: None,
        }
    }
}

/// A waiting or suspended job, and the last sequence number of its run that
/// the worker which parked it had seen: a wake-up scan reads only events
/// after it.
#[derive(Clone, Debug, PartialEq)]
pub struct Parked {
    pub job: Job,
    pub checked_seq: u64,
}

#[async_trait]
pub trait JobStore: Send + Sync {
    /// Stores a new job, with `UnfencedReason::JobSubmit` or a current fence.
    /// InvalidInput unless it is queued, without a checkpoint or a lease;
    /// Conflict if the id exists.
    async fn insert(&self, job: &Job, authority: Option<&WriteAuthority>) -> Result<(), Error>;

    /// NotFound if there is no such job.
    async fn get(&self, id: &JobId) -> Result<Job, Error>;

    async fn find_by_run(&self, run_id: &RunId) -> Result<Option<Job>, Error>;

    /// The job's continuation record, if a save stored one. NotFound if
    /// there is no such job.
    async fn continuation(&self, id: &JobId) -> Result<Option<ContinuationRecord>, Error>;

    /// Waiting and suspended jobs, least recently updated first, at most
    /// `limit`.
    async fn parked(&self, limit: usize) -> Result<Vec<Parked>, Error>;

    /// Atomically claims the oldest job (by `created_at`, then id) that is
    /// queued and past its `not_before`, or running with an expired lease.
    /// The claimed job is running, leased to `holder` for `lease_for`, with
    /// `attempt` incremented, and its lease epoch is one higher than before:
    /// the returned fence is the only current one. Concurrent callers never
    /// receive the same job.
    async fn claim(
        &self,
        holder: &Identifier,
        lease_for: Duration,
    ) -> Result<Option<Claimed>, Error>;

    /// Extends the lease of the fence's job. LeaseLost unless the fence is
    /// current.
    async fn renew_lease(&self, fence: &FenceToken, lease_for: Duration) -> Result<Job, Error>;

    /// Writes the job, its continuation record and its `not_before` while
    /// `fence` is current; LeaseLost otherwise. InvalidInput if the fence is
    /// for another job. The store keeps its own lease, epoch and attempt
    /// values; a status other than running clears the lease, and with it the
    /// fence. A save into waiting or suspended sets the job's checked
    /// sequence number ([`Parked::checked_seq`]) to its
    /// `progress.last_event_seq`.
    async fn save(&self, write: &JobWrite<'_>, fence: &FenceToken) -> Result<(), Error>;

    /// Moves a waiting or suspended job back to queued, on the checkpoint it
    /// was parked on, if `reason` matches its wake condition, with
    /// `UnfencedReason::JobWake` or a current fence. Returns whether it woke.
    /// NotFound if there is no such job.
    async fn wake(
        &self,
        id: &JobId,
        reason: &WakeReason,
        authority: Option<&WriteAuthority>,
    ) -> Result<bool, Error>;
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
    /// Needs a current fence, or an unfenced reason that permits the event.
    ///
    /// An event that completes something ([`crate::completion_key`]) is
    /// stored once per run: appending it again, even concurrently, returns
    /// the event already stored, unchanged, and takes no sequence number.
    async fn append(
        &self,
        event: NewEvent,
        authority: Option<&WriteAuthority>,
    ) -> Result<Event, Error>;

    /// Events with `seq > after`, in ascending order, at most `limit`.
    async fn read(&self, run_id: &RunId, after: u64, limit: usize) -> Result<Vec<Event>, Error>;

    /// Like [`EventLog::read`], but only events of the given kinds.
    async fn read_kinds(
        &self,
        run_id: &RunId,
        after: u64,
        kinds: &[EventKind],
        limit: usize,
    ) -> Result<Vec<Event>, Error>;

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

    /// Stores a new version atomically. Needs a current fence, or
    /// `UnfencedReason::ProjectCreate` for version 0. InvalidInput if the
    /// document and record disagree on project, version or parent. Conflict
    /// unless the version is head + 1 (0 when empty) and its parent is the
    /// current head; a stale fence is LeaseLost even then.
    async fn append_version(
        &self,
        document: &SdrDocument,
        record: &SdrVersionRecord,
        authority: Option<&WriteAuthority>,
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

/// A tool call about to run, before the store records it.
#[derive(Clone, Debug, PartialEq)]
pub struct NewStep {
    pub idempotency_key: Identifier,
    pub run_id: RunId,
    pub step_id: StepId,
    pub tool: ToolName,
    /// sha256 of the canonical JSON of the tool name and arguments.
    pub request_sha256: Sha256,
}

/// What [`StepStore::begin`] found.
#[derive(Clone, Debug, PartialEq)]
pub enum StepStart {
    /// No record had the key; a started record is now stored and the caller
    /// runs the step.
    Started(StepRecord),
    /// A record for the same request already exists: completed, so replay its
    /// outcome, or still started, because an earlier attempt was interrupted
    /// (ADR-0026).
    Existing(StepRecord),
}

/// Step records keyed by idempotency key (blueprint section 6.3, ADR-0026).
/// The store sets `started_at` and `completed_at` from its own clock
/// (ADR-0023). Both writes need a current fence; no unfenced reason covers
/// them.
#[async_trait]
pub trait StepStore: Send + Sync {
    /// Atomically stores a started record unless the key exists. Of
    /// concurrent callers with one key, exactly one gets `Started`. Conflict
    /// if the key exists for a different run, step, tool or request hash.
    async fn begin(
        &self,
        step: &NewStep,
        authority: Option<&WriteAuthority>,
    ) -> Result<StepStart, Error>;

    /// Stores the outcome of a started step. Completing again with the same
    /// outcome is Ok and returns the stored record; a different outcome is
    /// Conflict. NotFound if no step has the key.
    async fn complete(
        &self,
        key: &Identifier,
        outcome: &StepOutcome,
        authority: Option<&WriteAuthority>,
    ) -> Result<StepRecord, Error>;

    /// NotFound if no step has the key.
    async fn get(&self, key: &Identifier) -> Result<StepRecord, Error>;
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
