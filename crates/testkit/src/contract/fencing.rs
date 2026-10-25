use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use contracts::ids::{Identifier, JobId, ProjectId, RunId, SdrVersion};
use contracts::{
    Claimed, Clock, Error, ErrorCode, EventLog, JobStore, NewEvent, SdrRepository, StaleFence,
    Staleness, StepStore, UnfencedReason, WakeReason, WriteAuthority,
};
use serde_json::{Value, json};

use super::{WAIT, fenced, unfenced};
use crate::builders::{
    self, id, job, marker_payload, new_event, new_step, sdr_document, succeeded, version_record,
};
use crate::clock::ManualClock;

const LEASE: Duration = Duration::from_secs(30);
/// Just past [`LEASE`].
const PAST_LEASE: Duration = Duration::from_secs(31);

/// The stores the fencing suite writes to. All four share one job store's
/// fence state and read time from the suite's clock: in memory through
/// `fenced_by(jobs.leases())`, in Postgres through one database.
pub struct FencedStores {
    pub jobs: Arc<dyn JobStore>,
    pub events: Arc<dyn EventLog>,
    pub steps: Arc<dyn StepStore>,
    pub sdr: Arc<dyn SdrRepository>,
}

struct Fixture {
    clock: Arc<ManualClock>,
    s: FencedStores,
}

async fn fresh<F, Fut>(make: &F) -> Fixture
where
    F: Fn(Arc<dyn Clock>) -> Fut,
    Fut: Future<Output = FencedStores>,
{
    let clock = Arc::new(ManualClock::at_epoch());
    let s = make(clock.clone()).await;
    Fixture { clock, s }
}

/// Lease fencing (ADR-0032): every store refuses a write whose fence is
/// stale and accepts one whose fence is current, checked in the write's own
/// transaction and before any other condition; unfenced reasons permit only
/// their writes. `make` builds empty stores on the given clock.
pub async fn fencing<F, Fut>(make: F)
where
    F: Fn(Arc<dyn Clock>) -> Fut,
    Fut: Future<Output = FencedStores>,
{
    each_claim_raises_the_epoch_by_one(fresh(&make).await).await;
    the_current_fence_writes_everything(fresh(&make).await).await;
    a_stale_fence_writes_nothing(fresh(&make).await).await;
    the_epoch_not_the_holder_decides(fresh(&make).await).await;
    an_expired_fence_is_refused_before_any_reclaim(fresh(&make).await).await;
    a_fence_ends_when_the_job_leaves_running(fresh(&make).await).await;
    fence_is_checked_before_sdr_head(fresh(&make).await).await;
    unfenced_reasons_permit_only_their_writes(fresh(&make).await).await;
    a_reclaim_never_interleaves_with_stale_writes(fresh(&make).await).await;
}

fn worker(n: usize) -> Identifier {
    id(&format!("worker-{n}"))
}

fn run() -> RunId {
    id("run-1")
}

fn project(name: &str) -> ProjectId {
    id(name)
}

fn event(payload: Value) -> NewEvent {
    new_event(
        "run-1",
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        payload,
    )
}

/// An event no unfenced reason permits.
fn run_started() -> NewEvent {
    event(json!({ "type": "run_started", "task_id": "task-1", "agent_id": "test_agent" }))
}

impl Fixture {
    async fn insert(&self, job_id: &str) {
        self.s
            .jobs
            .insert(
                &job(job_id, "run-1", self.clock.now()),
                Some(&unfenced(UnfencedReason::JobSubmit)),
            )
            .await
            .unwrap_or_else(|e| panic!("insert {job_id}: {e}"));
    }

    async fn claim(&self, n: usize) -> Claimed {
        self.s
            .jobs
            .claim(&worker(n), LEASE)
            .await
            .expect("claim")
            .expect("a claimable job was not claimed")
    }

    async fn append_version(
        &self,
        project: &str,
        version: u64,
        parent: Option<u64>,
        authority: Option<&WriteAuthority>,
    ) -> Result<(), Error> {
        let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        self.s
            .sdr
            .append_version(
                &sdr_document(project, version, parent),
                &version_record(project, version, parent, at),
                authority,
            )
            .await
    }
}

/// Asserts that `result` is LeaseLost because of a stale fence at `epoch`,
/// with the job now at `current`, and returns the refusal.
#[track_caller]
fn assert_stale<T: Debug>(
    result: Result<T, Error>,
    epoch: u64,
    current: Option<u64>,
    case: &str,
) -> StaleFence {
    let err = match result {
        Ok(value) => panic!("{case}: expected lease_lost, got Ok({value:?})"),
        Err(err) => err,
    };
    assert_eq!(err.code(), ErrorCode::LeaseLost, "{case}: {err}");
    let stale = std::error::Error::source(&err)
        .and_then(|source| source.downcast_ref::<StaleFence>())
        .unwrap_or_else(|| panic!("{case}: no StaleFence source in {err}"))
        .clone();
    assert_eq!(stale.fence.epoch(), epoch, "{case}: the stale epoch");
    assert_eq!(stale.current_epoch, current, "{case}: the current epoch");
    stale
}

#[track_caller]
fn assert_lease_lost<T: Debug>(result: Result<T, Error>, case: &str) {
    match result {
        Err(e) => assert_eq!(e.code(), ErrorCode::LeaseLost, "{case}: {e}"),
        Ok(value) => panic!("{case}: expected lease_lost, got Ok({value:?})"),
    }
}

async fn each_claim_raises_the_epoch_by_one(f: Fixture) {
    f.insert("job-1").await;
    let first = f.claim(1).await;
    assert_eq!(first.fence.epoch(), 1, "epoch: first claim");
    assert_eq!(first.fence.holder(), &worker(1), "epoch: first holder");
    f.clock.advance(PAST_LEASE);
    let second = f.claim(2).await;
    assert_eq!(second.fence.epoch(), 2, "epoch: reclaim by another holder");
    f.clock.advance(PAST_LEASE);
    let third = f.claim(2).await;
    assert_eq!(third.fence.epoch(), 3, "epoch: reclaim by the same holder");
    assert_eq!(third.fence.job_id(), &id::<JobId>("job-1"), "epoch: job");

    // Epochs are per job.
    f.insert("job-2").await;
    let other = f.claim(1).await;
    assert_eq!(other.fence.job_id(), &id::<JobId>("job-2"), "epoch: job-2");
    assert_eq!(other.fence.epoch(), 1, "epoch: another job starts at 1");
}

async fn the_current_fence_writes_everything(f: Fixture) {
    f.insert("job-1").await;
    let claimed = f.claim(1).await;
    let authority = fenced(&claimed);
    let auth = Some(&authority);
    f.s.jobs
        .renew_lease(&claimed.fence, LEASE)
        .await
        .expect("current: renew");
    let mut progressed = claimed.job.clone();
    progressed.progress.steps_completed = 1;
    f.s.jobs
        .save(&progressed, &claimed.fence)
        .await
        .expect("current: save");
    f.s.events
        .append(run_started(), auth)
        .await
        .expect("current: append");
    let step = new_step("step-1", "create_plate", "request-a");
    f.s.steps.begin(&step, auth).await.expect("current: begin");
    let outcome = succeeded(json!({}));
    f.s.steps
        .complete(&step.idempotency_key, &outcome, auth)
        .await
        .expect("current: complete");
    f.append_version("p1", 0, None, auth)
        .await
        .expect("current: version 0");
    f.append_version("p1", 1, Some(0), auth)
        .await
        .expect("current: version 1");

    assert_eq!(f.s.events.last_seq(&run()).await.expect("last_seq"), 1);
    assert_eq!(
        f.s.sdr.head(&project("p1")).await.expect("head"),
        Some(SdrVersion(1))
    );
    let record = f.s.steps.get(&step.idempotency_key).await.expect("get");
    assert_eq!(record.outcome, Some(outcome), "current: step outcome");
    let stored = f.s.jobs.get(&claimed.job.id).await.expect("get");
    assert_eq!(stored.progress.steps_completed, 1, "current: saved job");
}

async fn a_stale_fence_writes_nothing(f: Fixture) {
    f.insert("job-1").await;
    let old = f.claim(1).await;
    let stale = fenced(&old);
    let started = new_step("step-1", "create_plate", "request-a");
    f.s.steps
        .begin(&started, Some(&stale))
        .await
        .expect("stale: begin under the live fence");
    f.append_version("p1", 0, None, Some(&stale))
        .await
        .expect("stale: version 0 under the live fence");
    f.s.events
        .append(run_started(), Some(&stale))
        .await
        .expect("stale: append under the live fence");

    f.clock.advance(PAST_LEASE);
    let new = f.claim(2).await;
    let current = fenced(&new);
    let job_before = f.s.jobs.get(&new.job.id).await.expect("get");
    let seq_before = f.s.events.last_seq(&run()).await.expect("last_seq");

    let mut progressed = new.job.clone();
    progressed.progress.steps_completed = 5;
    let later = new_step("step-2", "measure", "request-b");
    let outcome = succeeded(json!({}));
    let stale_writes = [
        (
            "renew",
            f.s.jobs.renew_lease(&old.fence, LEASE).await.map(|_| ()),
        ),
        ("save", f.s.jobs.save(&progressed, &old.fence).await),
        (
            "append",
            f.s.events
                .append(run_started(), Some(&stale))
                .await
                .map(|_| ()),
        ),
        (
            "begin",
            f.s.steps.begin(&later, Some(&stale)).await.map(|_| ()),
        ),
        (
            "complete",
            f.s.steps
                .complete(&started.idempotency_key, &outcome, Some(&stale))
                .await
                .map(|_| ()),
        ),
        (
            "append_version",
            f.append_version("p1", 1, Some(0), Some(&stale)).await,
        ),
    ];
    for (write, result) in stale_writes {
        let refused = assert_stale(result, 1, Some(2), &format!("stale: {write}"));
        assert_eq!(refused.staleness, Staleness::Reclaimed, "stale: {write}");
        assert_eq!(
            refused.current_holder,
            Some(worker(2)),
            "stale: {write} names the new holder"
        );
    }

    assert_eq!(
        f.s.jobs.get(&new.job.id).await.expect("get"),
        job_before,
        "stale: the job changed"
    );
    assert_eq!(
        f.s.events.last_seq(&run()).await.expect("last_seq"),
        seq_before,
        "stale: an event was appended"
    );
    let record = f.s.steps.get(&started.idempotency_key).await.expect("get");
    assert_eq!(record.outcome, None, "stale: the step was completed");
    assert_eq!(
        f.s.steps
            .get(&later.idempotency_key)
            .await
            .map_err(|e| e.code()),
        Err(ErrorCode::NotFound),
        "stale: a step was begun"
    );
    assert_eq!(
        f.s.sdr.head(&project("p1")).await.expect("head"),
        Some(SdrVersion(0)),
        "stale: the SDR head moved"
    );

    // The new holder's fence writes each of them.
    f.s.jobs
        .renew_lease(&new.fence, LEASE)
        .await
        .expect("stale: renew with the current fence");
    f.s.jobs
        .save(&progressed, &new.fence)
        .await
        .expect("stale: save with the current fence");
    f.s.events
        .append(run_started(), Some(&current))
        .await
        .expect("stale: append with the current fence");
    f.s.steps
        .begin(&later, Some(&current))
        .await
        .expect("stale: begin with the current fence");
    f.s.steps
        .complete(&started.idempotency_key, &outcome, Some(&current))
        .await
        .expect("stale: complete with the current fence");
    f.append_version("p1", 1, Some(0), Some(&current))
        .await
        .expect("stale: append_version with the current fence");
}

async fn the_epoch_not_the_holder_decides(f: Fixture) {
    f.insert("job-1").await;
    let old = f.claim(1).await;
    f.clock.advance(PAST_LEASE);
    let new = f.claim(1).await;
    assert_eq!(new.fence.holder(), old.fence.holder(), "same holder");
    assert_stale(
        f.s.events.append(run_started(), Some(&fenced(&old))).await,
        1,
        Some(2),
        "same holder: append with the old fence",
    );
    assert_stale(
        f.s.jobs.renew_lease(&old.fence, LEASE).await,
        1,
        Some(2),
        "same holder: renew with the old fence",
    );
    f.s.events
        .append(run_started(), Some(&fenced(&new)))
        .await
        .expect("same holder: append with the new fence");
    f.s.jobs
        .renew_lease(&new.fence, LEASE)
        .await
        .expect("same holder: renew with the new fence");
}

async fn an_expired_fence_is_refused_before_any_reclaim(f: Fixture) {
    f.insert("job-1").await;
    let claimed = f.claim(1).await;
    let authority = fenced(&claimed);
    f.clock.advance(PAST_LEASE);
    let refused = assert_stale(
        f.s.events.append(run_started(), Some(&authority)).await,
        1,
        Some(1),
        "expired: append",
    );
    assert_eq!(refused.staleness, Staleness::Expired, "expired: why");
    assert_stale(
        f.s.jobs.renew_lease(&claimed.fence, LEASE).await,
        1,
        Some(1),
        "expired: renew",
    );
    assert_stale(
        f.s.steps
            .begin(
                &new_step("step-1", "measure", "request-a"),
                Some(&authority),
            )
            .await,
        1,
        Some(1),
        "expired: begin",
    );
    let stored = f.s.jobs.get(&claimed.job.id).await.expect("get");
    assert_eq!(
        stored.lease.map(|l| l.holder),
        Some(worker(1)),
        "expired: nobody reclaimed the job"
    );
}

async fn a_fence_ends_when_the_job_leaves_running(f: Fixture) {
    f.insert("job-1").await;
    let suspended = f.claim(1).await;
    let mut waiting = suspended.job.clone();
    waiting.status = builders::suspended("appr-1", 1);
    f.s.jobs
        .save(&waiting, &suspended.fence)
        .await
        .expect("left running: suspend");
    let refused = assert_stale(
        f.s.events
            .append(run_started(), Some(&fenced(&suspended)))
            .await,
        1,
        Some(1),
        "left running: append after suspending",
    );
    assert_eq!(
        refused.staleness,
        Staleness::NotRunning,
        "left running: why"
    );
    assert_stale(
        f.s.steps
            .begin(
                &new_step("step-1", "measure", "request-a"),
                Some(&fenced(&suspended)),
            )
            .await,
        1,
        Some(1),
        "left running: begin after suspending",
    );

    f.clock.advance(Duration::from_secs(1));
    f.insert("job-2").await;
    let completed = f.claim(2).await;
    assert_eq!(
        completed.job.id,
        id::<JobId>("job-2"),
        "left running: job-2"
    );
    let mut done = completed.job.clone();
    done.status = builders::completed("success", 1);
    f.s.jobs
        .save(&done, &completed.fence)
        .await
        .expect("left running: complete");
    assert_stale(
        f.append_version("p1", 0, None, Some(&fenced(&completed)))
            .await,
        1,
        Some(1),
        "left running: append_version after completing",
    );
    assert_stale(
        f.s.jobs.save(&done, &completed.fence).await,
        1,
        Some(1),
        "left running: save after completing",
    );
}

async fn fence_is_checked_before_sdr_head(f: Fixture) {
    f.insert("job-1").await;
    let old = f.claim(1).await;
    f.append_version("p1", 0, None, Some(&fenced(&old)))
        .await
        .expect("before head: version 0");
    f.clock.advance(PAST_LEASE);
    let new = f.claim(2).await;
    f.append_version("p1", 1, Some(0), Some(&fenced(&new)))
        .await
        .expect("before head: version 1 by the new holder");
    // Version 1 again is a stale version too, which alone would be Conflict.
    assert_stale(
        f.append_version("p1", 1, Some(0), Some(&fenced(&old)))
            .await,
        1,
        Some(2),
        "before head: a stale fence and a stale version",
    );
    assert_stale(
        f.append_version("p1", 2, Some(1), Some(&fenced(&old)))
            .await,
        1,
        Some(2),
        "before head: a stale fence and the next version",
    );
    assert_eq!(
        f.s.sdr.head(&project("p1")).await.expect("head"),
        Some(SdrVersion(1)),
        "before head: the head moved"
    );
}

/// Every write kind the unfenced reasons are tried against. The
/// `job_state_changed` appends are named for the transition they record.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Write {
    Insert,
    Wake,
    Cancel,
    Answered,
    Started,
    Begin,
    Complete,
    Version0,
    Version1,
    Submitted,
    Woken,
    WokenFromWaiting,
    Requeued,
    Finished,
}

const WRITES: [Write; 14] = [
    Write::Insert,
    Write::Wake,
    Write::Cancel,
    Write::Answered,
    Write::Started,
    Write::Begin,
    Write::Complete,
    Write::Version0,
    Write::Version1,
    Write::Submitted,
    Write::Woken,
    Write::WokenFromWaiting,
    Write::Requeued,
    Write::Finished,
];

/// The allow-list of ADR-0032: each reason covers one operation, its job
/// write and the event that records it.
fn permits(reason: UnfencedReason, write: Write) -> bool {
    match reason {
        UnfencedReason::JobSubmit => matches!(write, Write::Insert | Write::Submitted),
        UnfencedReason::JobWake => {
            matches!(write, Write::Wake | Write::Woken | Write::WokenFromWaiting)
        }
        UnfencedReason::ApiCommand => matches!(write, Write::Cancel | Write::Answered),
        UnfencedReason::ProjectCreate => write == Write::Version0,
        _ => false,
    }
}

impl Write {
    fn is_append(self) -> bool {
        matches!(
            self,
            Write::Cancel
                | Write::Answered
                | Write::Started
                | Write::Submitted
                | Write::Woken
                | Write::WokenFromWaiting
                | Write::Requeued
                | Write::Finished
        )
    }
}

/// `job_state_changed` from `from` (absent if `None`) into `to`.
fn state_changed(from: Option<&str>, to: Value) -> Value {
    let mut payload = json!({ "type": "job_state_changed", "to": to });
    if let Some(from) = from {
        payload["from"] = json!(from);
    }
    payload
}

fn queued_on_checkpoint() -> Value {
    json!({ "state": "queued", "checkpoint": builders::checkpoint(1) })
}

impl Fixture {
    /// Makes `write`, the `n`th of the matrix, under `authority`. Each one
    /// would succeed if it were permitted.
    async fn try_write(
        &self,
        write: Write,
        n: usize,
        authority: Option<&WriteAuthority>,
    ) -> Result<(), Error> {
        match write {
            Write::Insert => {
                let new = job(&format!("job-m-{n}"), "run-1", self.clock.now());
                self.s.jobs.insert(&new, authority).await
            }
            Write::Wake => self
                .s
                .jobs
                .wake(&id("job-base"), &WakeReason::CancelRequested, authority)
                .await
                .map(|_| ()),
            Write::Cancel => self
                .s
                .events
                .append(event(marker_payload(n)), authority)
                .await
                .map(|_| ()),
            Write::Answered => self
                .s
                .events
                .append(
                    event(json!({
                        "type": "approval_answered",
                        "approval_id": "appr-1",
                        "answer": "approve"
                    })),
                    authority,
                )
                .await
                .map(|_| ()),
            Write::Started => self
                .s
                .events
                .append(run_started(), authority)
                .await
                .map(|_| ()),
            Write::Begin => self
                .s
                .steps
                .begin(
                    &new_step(&format!("step-m-{n}"), "measure", "request-a"),
                    authority,
                )
                .await
                .map(|_| ()),
            Write::Complete => self
                .s
                .steps
                .complete(&id("run-1:step-base"), &succeeded(json!({})), authority)
                .await
                .map(|_| ()),
            Write::Version0 => {
                self.append_version(&format!("p-m-{n}"), 0, None, authority)
                    .await
            }
            Write::Version1 => self.append_version("p-base", 1, Some(0), authority).await,
            Write::Submitted => {
                self.append_payload(state_changed(None, json!({ "state": "queued" })), authority)
                    .await
            }
            Write::Woken => {
                self.append_payload(
                    state_changed(Some("suspended"), queued_on_checkpoint()),
                    authority,
                )
                .await
            }
            Write::WokenFromWaiting => {
                self.append_payload(
                    state_changed(Some("waiting"), queued_on_checkpoint()),
                    authority,
                )
                .await
            }
            Write::Requeued => {
                self.append_payload(
                    state_changed(Some("running"), queued_on_checkpoint()),
                    authority,
                )
                .await
            }
            Write::Finished => {
                self.append_payload(
                    state_changed(
                        Some("running"),
                        serde_json::to_value(builders::completed("success", 1))
                            .expect("a status serializes"),
                    ),
                    authority,
                )
                .await
            }
        }
    }

    async fn append_payload(
        &self,
        payload: Value,
        authority: Option<&WriteAuthority>,
    ) -> Result<(), Error> {
        self.s
            .events
            .append(event(payload), authority)
            .await
            .map(|_| ())
    }
}

async fn unfenced_reasons_permit_only_their_writes(f: Fixture) {
    // Targets every write could succeed on: a job to wake, a started step,
    // a project at version 0.
    f.insert("job-base").await;
    f.append_version(
        "p-base",
        0,
        None,
        Some(&unfenced(UnfencedReason::ProjectCreate)),
    )
    .await
    .expect("matrix: version 0 of p-base");
    let claimed = f.claim(1).await;
    f.s.steps
        .begin(
            &new_step("step-base", "measure", "request-a"),
            Some(&fenced(&claimed)),
        )
        .await
        .expect("matrix: begin the base step");

    let mut n = 0;
    let mut permitted_appends = 0;
    for reason in UnfencedReason::ALL {
        let authority = unfenced(reason);
        for write in WRITES {
            n += 1;
            let result = f.try_write(write, n, Some(&authority)).await;
            let case = format!("matrix: {reason} with {write:?}");
            if permits(reason, write) {
                result.unwrap_or_else(|e| panic!("{case}: refused: {e}"));
                if write.is_append() {
                    permitted_appends += 1;
                }
            } else {
                assert_lease_lost(result, &case);
                match write {
                    Write::Insert => assert_eq!(
                        f.s.jobs
                            .get(&id(&format!("job-m-{n}")))
                            .await
                            .map_err(|e| e.code()),
                        Err(ErrorCode::NotFound),
                        "{case}: the job was stored"
                    ),
                    Write::Version0 => assert_eq!(
                        f.s.sdr
                            .head(&project(&format!("p-m-{n}")))
                            .await
                            .expect("head"),
                        None,
                        "{case}: the version was stored"
                    ),
                    _ => {}
                }
            }
        }
    }
    for write in WRITES {
        n += 1;
        assert_lease_lost(
            f.try_write(write, n, None).await,
            &format!("matrix: no authority with {write:?}"),
        );
    }

    assert_eq!(
        f.s.events.last_seq(&run()).await.expect("last_seq"),
        permitted_appends,
        "matrix: only the permitted appends were stored"
    );
    assert_eq!(
        f.s.sdr.head(&project("p-base")).await.expect("head"),
        Some(SdrVersion(0)),
        "matrix: version 1 was stored without a fence"
    );
    let base = f.s.steps.get(&id("run-1:step-base")).await.expect("get");
    assert_eq!(base.outcome, None, "matrix: a step was completed unfenced");
}

/// Workers keep appending under worker-1's fence while the lease expires
/// and worker-2 claims the job. No append that started after the claim may
/// succeed, and every one that succeeded is ordered before the claim.
async fn a_reclaim_never_interleaves_with_stale_writes(f: Fixture) {
    const WRITERS: usize = 4;
    const AFTER_CLAIM: usize = 5;
    f.insert("job-1").await;
    let old = f.claim(1).await;
    let claimed = Arc::new(AtomicBool::new(false));
    let successes = Arc::new(AtomicUsize::new(0));
    let mut writers = Vec::new();
    for w in 0..WRITERS {
        let events = Arc::clone(&f.s.events);
        let claimed = Arc::clone(&claimed);
        let successes = Arc::clone(&successes);
        let authority = fenced(&old);
        writers.push(tokio::spawn(async move {
            let mut stored = Vec::new();
            let mut late = 0;
            for n in 0.. {
                let after_claim = claimed.load(Ordering::SeqCst);
                let marker = event(marker_payload(w * 1_000_000 + n));
                match events.append(marker, Some(&authority)).await {
                    Ok(stored_event) => {
                        assert!(
                            !after_claim,
                            "interleave: an append that started after the claim stored seq {}",
                            stored_event.seq
                        );
                        stored.push(stored_event.seq.get());
                        successes.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(e) => assert_eq!(e.code(), ErrorCode::LeaseLost, "interleave: {e}"),
                }
                if after_claim {
                    late += 1;
                    if late == AFTER_CLAIM {
                        break;
                    }
                }
                // An in-memory append never waits, so give the claimer and
                // the timers their turn.
                tokio::task::yield_now().await;
            }
            stored
        }));
    }

    tokio::time::timeout(WAIT, async {
        while successes.load(Ordering::SeqCst) < 2 * WRITERS {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("interleave: the writers did not get going");
    f.clock.advance(PAST_LEASE);
    let new = tokio::time::timeout(WAIT, async {
        loop {
            if let Some(claim) = f.s.jobs.claim(&worker(2), LEASE).await.expect("claim") {
                break claim;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("interleave: the expired job was never reclaimed");
    claimed.store(true, Ordering::SeqCst);
    let last = f.s.events.last_seq(&run()).await.expect("last_seq");
    assert_eq!(new.fence.epoch(), 2, "interleave: epoch of the claim");

    let mut all = Vec::new();
    for writer in writers {
        all.extend(
            tokio::time::timeout(WAIT, writer)
                .await
                .expect("interleave: a writer hung")
                .expect("interleave: a writer panicked"),
        );
    }
    assert!(
        all.iter().all(|&seq| seq <= last),
        "interleave: a stale append was stored after the claim (last seq at the claim {last})"
    );
    assert_eq!(
        all.len() as u64,
        f.s.events.last_seq(&run()).await.expect("last_seq"),
        "interleave: the log holds events nobody stored"
    );
}
