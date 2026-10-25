//! Doubles that stop a worker at a chosen point, for crash and race tests
//! (ADR-0031). Each wraps a real store or provider and forwards every call,
//! except that the first call matching its trigger stops at a [`Pause`],
//! before the call reaches the inner one or after it returned. The test
//! waits for [`Pause::reached`], then kills the worker, or releases it.
//!
//! Bootstrap builds the real stores, so a test does not hand it a double:
//! it hands it a wrapper ([`Wrap`]) from one of the `*Faults` values below,
//! which wraps what bootstrap built. Tests never name `SdrRepository`.
//!
//! A tool call pauses through [`crate::fakes::FakeTool::before_answer`],
//! whose hook calls [`Pause::stop`].

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use contracts::ids::{Identifier, JobId, ProjectId, RunId, SdrVersion};
use contracts::types::{
    ContinuationRecord, Event, EventPayload, Job, SdrDocument, SdrVersionRecord, StepOutcome,
    StepRecord,
};
use contracts::{
    Claimed, Error, EventKind, EventLog, ExecutionContext, FenceToken, JobStore, JobWrite,
    ModelProvider, ModelRequest, ModelResponse, NewEvent, NewStep, Parked, RecordWrite,
    SdrRepository, StepStart, StepStore, WakeReason, WriteAuthority,
};
use tokio::sync::watch;

/// A wrapper bootstrap applies to what it built.
pub type Wrap<T> = Box<dyn FnOnce(Arc<T>) -> Arc<T> + Send>;

/// Whether a double stops before the matching call reaches the inner one,
/// or after it returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum At {
    Before,
    After,
}

struct PauseState {
    name: &'static str,
    at: At,
    fired: AtomicBool,
    reached: watch::Sender<bool>,
    released: watch::Sender<bool>,
}

/// A point the first matching call stops at. Cloning shares it.
#[derive(Clone)]
pub struct Pause {
    state: Arc<PauseState>,
}

impl Pause {
    pub fn new(name: &'static str, at: At) -> Self {
        Self {
            state: Arc::new(PauseState {
                name,
                at,
                fired: AtomicBool::new(false),
                reached: watch::channel(false).0,
                released: watch::channel(false).0,
            }),
        }
    }

    pub fn name(&self) -> &'static str {
        self.state.name
    }

    /// Whether a call has stopped here.
    pub fn is_reached(&self) -> bool {
        *self.state.reached.borrow()
    }

    /// Resolves once a call stops here; at once if one has. Panics, naming
    /// the pause, if none does within `within`.
    pub async fn reached(&self, within: Duration) {
        let mut reached = self.state.reached.subscribe();
        let waited = tokio::time::timeout(within, reached.wait_for(|r| *r)).await;
        assert!(
            matches!(waited, Ok(Ok(_))),
            "pause {} was not reached within {within:?}",
            self.state.name
        );
    }

    /// Lets the stopped call go on. A test that kills the worker instead
    /// never calls it.
    pub fn release(&self) {
        self.state.released.send_replace(true);
    }

    /// Stops here, unless a call already has: for a tool, from
    /// [`crate::fakes::FakeTool::before_answer`].
    pub async fn stop(&self) {
        self.stop_if(self.state.at, true).await;
    }

    /// For a double: stops here if `matched` is the first match at `at`.
    async fn stop_if(&self, at: At, matched: bool) {
        if at != self.state.at || !matched || self.state.fired.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut released = self.state.released.subscribe();
        self.state.reached.send_replace(true);
        let _ = released.wait_for(|r| *r).await;
    }
}

type SavePredicate = dyn Fn(&Job, Option<&ContinuationRecord>) -> bool + Send + Sync;

/// What a [`PausingJobStore`] does: stop at a save, and record every claim.
#[derive(Clone, Default)]
pub struct JobFaults {
    save: Option<(Pause, Arc<SavePredicate>)>,
    claims: Arc<Mutex<Vec<Claimed>>>,
}

impl JobFaults {
    /// Records claims and pauses nowhere.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stops at the first save whose job, and new continuation record if
    /// it writes one, match.
    pub fn pause_save(
        mut self,
        pause: &Pause,
        matches: impl Fn(&Job, Option<&ContinuationRecord>) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.save = Some((pause.clone(), Arc::new(matches)));
        self
    }

    /// Every claim the store answered, in order: with one, a test builds a
    /// worker's context through `testkit::leases::fenced_ctx`, for example
    /// one whose lease another worker has since taken.
    pub fn claims(&self) -> Vec<Claimed> {
        self.claims.lock().unwrap().clone()
    }

    pub fn wrapper(&self) -> Wrap<dyn JobStore> {
        let faults = self.clone();
        Box::new(move |inner| Arc::new(PausingJobStore { inner, faults }))
    }
}

/// A [`JobStore`] that forwards every call; see [`JobFaults`].
pub struct PausingJobStore {
    inner: Arc<dyn JobStore>,
    faults: JobFaults,
}

impl PausingJobStore {
    async fn stop(&self, at: At, write: &JobWrite<'_>) {
        if let Some((pause, matches)) = &self.faults.save {
            let record = match write.continuation {
                RecordWrite::Replace(record) => Some(record),
                RecordWrite::Keep => None,
            };
            pause.stop_if(at, matches(write.job, record)).await;
        }
    }
}

#[async_trait]
impl JobStore for PausingJobStore {
    async fn insert(&self, job: &Job, authority: Option<&WriteAuthority>) -> Result<(), Error> {
        self.inner.insert(job, authority).await
    }

    async fn get(&self, id: &JobId) -> Result<Job, Error> {
        self.inner.get(id).await
    }

    async fn find_by_run(&self, run_id: &RunId) -> Result<Option<Job>, Error> {
        self.inner.find_by_run(run_id).await
    }

    async fn continuation(&self, id: &JobId) -> Result<Option<ContinuationRecord>, Error> {
        self.inner.continuation(id).await
    }

    async fn parked(&self, limit: usize) -> Result<Vec<Parked>, Error> {
        self.inner.parked(limit).await
    }

    async fn claim(
        &self,
        holder: &Identifier,
        lease_for: Duration,
    ) -> Result<Option<Claimed>, Error> {
        let claimed = self.inner.claim(holder, lease_for).await?;
        if let Some(claimed) = &claimed {
            self.faults.claims.lock().unwrap().push(claimed.clone());
        }
        Ok(claimed)
    }

    async fn renew_lease(&self, fence: &FenceToken, lease_for: Duration) -> Result<Job, Error> {
        self.inner.renew_lease(fence, lease_for).await
    }

    async fn save(&self, write: &JobWrite<'_>, fence: &FenceToken) -> Result<(), Error> {
        self.stop(At::Before, write).await;
        let saved = self.inner.save(write, fence).await;
        self.stop(At::After, write).await;
        saved
    }

    async fn wake(
        &self,
        id: &JobId,
        reason: &WakeReason,
        authority: Option<&WriteAuthority>,
    ) -> Result<bool, Error> {
        self.inner.wake(id, reason, authority).await
    }
}

type EventPredicate = dyn Fn(&EventPayload) -> bool + Send + Sync;

/// What a [`PausingEventLog`] does: stop at an append.
#[derive(Clone)]
pub struct EventFaults {
    pause: Pause,
    matches: Arc<EventPredicate>,
}

impl EventFaults {
    /// Stops at the first append whose payload matches.
    pub fn pause_append(
        pause: &Pause,
        matches: impl Fn(&EventPayload) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            pause: pause.clone(),
            matches: Arc::new(matches),
        }
    }

    /// Stops at the first append of `kind`.
    pub fn pause_kind(pause: &Pause, kind: EventKind) -> Self {
        Self::pause_append(pause, move |payload| EventKind::of(payload) == kind)
    }

    pub fn wrapper(&self) -> Wrap<dyn EventLog> {
        let faults = self.clone();
        Box::new(move |inner| Arc::new(PausingEventLog { inner, faults }))
    }
}

/// An [`EventLog`] that forwards every call; see [`EventFaults`].
pub struct PausingEventLog {
    inner: Arc<dyn EventLog>,
    faults: EventFaults,
}

#[async_trait]
impl EventLog for PausingEventLog {
    async fn append(
        &self,
        event: NewEvent,
        authority: Option<&WriteAuthority>,
    ) -> Result<Event, Error> {
        let matched = (self.faults.matches)(&event.payload);
        self.faults.pause.stop_if(At::Before, matched).await;
        let stored = self.inner.append(event, authority).await;
        self.faults.pause.stop_if(At::After, matched).await;
        stored
    }

    async fn read(&self, run_id: &RunId, after: u64, limit: usize) -> Result<Vec<Event>, Error> {
        self.inner.read(run_id, after, limit).await
    }

    async fn read_kinds(
        &self,
        run_id: &RunId,
        after: u64,
        kinds: &[EventKind],
        limit: usize,
    ) -> Result<Vec<Event>, Error> {
        self.inner.read_kinds(run_id, after, kinds, limit).await
    }

    async fn last_seq(&self, run_id: &RunId) -> Result<u64, Error> {
        self.inner.last_seq(run_id).await
    }

    async fn wait_for(
        &self,
        run_id: &RunId,
        after: u64,
        ctx: &ExecutionContext,
    ) -> Result<(), Error> {
        self.inner.wait_for(run_id, after, ctx).await
    }
}

/// What a [`PausingStepStore`] does: stop at a step's completion.
#[derive(Clone)]
pub struct StepFaults {
    pause: Pause,
}

impl StepFaults {
    /// Stops at the first `complete`.
    pub fn pause_complete(pause: &Pause) -> Self {
        Self {
            pause: pause.clone(),
        }
    }

    pub fn wrapper(&self) -> Wrap<dyn StepStore> {
        let faults = self.clone();
        Box::new(move |inner| Arc::new(PausingStepStore { inner, faults }))
    }
}

/// A [`StepStore`] that forwards every call; see [`StepFaults`].
pub struct PausingStepStore {
    inner: Arc<dyn StepStore>,
    faults: StepFaults,
}

#[async_trait]
impl StepStore for PausingStepStore {
    async fn begin(
        &self,
        step: &NewStep,
        authority: Option<&WriteAuthority>,
    ) -> Result<StepStart, Error> {
        self.inner.begin(step, authority).await
    }

    async fn complete(
        &self,
        key: &Identifier,
        outcome: &StepOutcome,
        authority: Option<&WriteAuthority>,
    ) -> Result<StepRecord, Error> {
        self.faults.pause.stop_if(At::Before, true).await;
        let completed = self.inner.complete(key, outcome, authority).await;
        self.faults.pause.stop_if(At::After, true).await;
        completed
    }

    async fn get(&self, key: &Identifier) -> Result<StepRecord, Error> {
        self.inner.get(key).await
    }
}

/// What [`PausingSdrStorage`] does: stop at a commit of a version above 0.
#[derive(Clone)]
pub struct SdrFaults {
    pause: Pause,
}

impl SdrFaults {
    /// Stops at the first append of a version other than 0, which is the
    /// project's creation.
    pub fn pause_commit(pause: &Pause) -> Self {
        Self {
            pause: pause.clone(),
        }
    }

    pub fn wrapper(&self) -> Wrap<dyn SdrRepository> {
        let faults = self.clone();
        Box::new(move |inner| Arc::new(PausingSdrStorage { inner, faults }))
    }
}

/// SDR storage that forwards every call; see [`SdrFaults`].
pub struct PausingSdrStorage {
    inner: Arc<dyn SdrRepository>,
    faults: SdrFaults,
}

#[async_trait]
impl SdrRepository for PausingSdrStorage {
    async fn head(&self, project_id: &ProjectId) -> Result<Option<SdrVersion>, Error> {
        self.inner.head(project_id).await
    }

    async fn document(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
    ) -> Result<SdrDocument, Error> {
        self.inner.document(project_id, version).await
    }

    async fn version_record(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
    ) -> Result<SdrVersionRecord, Error> {
        self.inner.version_record(project_id, version).await
    }

    async fn append_version(
        &self,
        document: &SdrDocument,
        record: &SdrVersionRecord,
        authority: Option<&WriteAuthority>,
    ) -> Result<(), Error> {
        let commit = record.version.0 > 0;
        self.faults.pause.stop_if(At::Before, commit).await;
        let appended = self.inner.append_version(document, record, authority).await;
        self.faults.pause.stop_if(At::After, commit).await;
        appended
    }
}

/// A [`ModelProvider`] that forwards every call, and stops at call number
/// `call` (from 0): before it, the request is lost with the worker; after
/// it, the reply is.
pub struct PausingModelProvider {
    inner: Arc<dyn ModelProvider>,
    pause: Pause,
    call: usize,
    calls: AtomicUsize,
}

impl PausingModelProvider {
    pub fn new(inner: Arc<dyn ModelProvider>, pause: &Pause, call: usize) -> Self {
        Self {
            inner,
            pause: pause.clone(),
            call,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ModelProvider for PausingModelProvider {
    fn provider_id(&self) -> &Identifier {
        self.inner.provider_id()
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ModelResponse, Error> {
        let matched = self.calls.fetch_add(1, Ordering::SeqCst) == self.call;
        self.pause.stop_if(At::Before, matched).await;
        let response = self.inner.complete(request, ctx).await;
        self.pause.stop_if(At::After, matched).await;
        response
    }
}
