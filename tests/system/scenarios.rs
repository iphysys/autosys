//! Scenarios of workers on one Postgres database: a reclaimed worker,
//! racing workers, cancels, retries and shutdown (ADR-0031, ADR-0032).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bootstrap::Overrides;
use contracts::types::{JobFailureKind, JobState, JobStatus, Transition};
use contracts::{Error, ErrorCode, StaleFence, Staleness};
use serde_json::json;
use testkit::builders::{id, marker_payload, new_event, new_step, parse};
use testkit::fakes::{FakeTool, HookFuture, KeyedEffects};
use testkit::faults::{At, JobFaults, Pause};
use testkit::leases::fenced_ctx;
use testkit::spawn::ManualSpawner;

use crate::support::db::TestDb;
use crate::support::*;

use JobState::{Cancelled, Completed, Failed, Queued, Running, Suspended};

/// `create_plate` that stops at `pause` before it answers, then sets
/// `went_on`.
fn holding(effects: &KeyedEffects, pause: &Pause, went_on: Arc<AtomicBool>) -> FakeTool {
    let pause = pause.clone();
    effects.tool(
        "create_plate",
        |_| plate_result(),
        move |_, _| {
            let (pause, went_on) = (pause.clone(), Arc::clone(&went_on));
            Box::pin(async move {
                pause.stop().await;
                went_on.store(true, Ordering::SeqCst);
                Ok(())
            }) as HookFuture
        },
    )
}

fn no_approvals() -> TestConfig {
    TestConfig::new(&[("policies/autonomy.yaml", NO_APPROVALS_YAML)])
}

/// Refused because a later claim, by `holder`, took the job.
fn reclaimed_by(error: Error, holder: &str) {
    assert_eq!(error.code(), ErrorCode::LeaseLost, "{error}");
    let stale = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<StaleFence>())
        .unwrap_or_else(|| panic!("no StaleFence behind {error}"));
    assert_eq!(stale.staleness, Staleness::Reclaimed);
    assert_eq!(
        stale.current_holder.as_ref().map(|h| h.as_str()),
        Some(holder)
    );
}

/// Worker A holds its first tool call while its heartbeat never runs, so
/// its lease expires and worker B claims the job. Every write A's fence
/// makes is refused in every Postgres store, A's run ends without a trace
/// once its call returns, and B finishes the job.
#[tokio::test(flavor = "multi_thread")]
async fn a_reclaimed_worker_cannot_write() {
    let db = TestDb::create().await;
    let config = no_approvals();
    let effects = KeyedEffects::new();
    let (a_hold, b_hold) = (
        Pause::new("worker A's create_plate", At::Before),
        Pause::new("worker B's create_plate", At::Before),
    );
    let (a_went_on, b_went_on) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let b = build(
        "worker-b",
        &db,
        &config,
        Parts::policy().create_plate(holding(&effects, &b_hold, b_went_on)),
    )
    .await;
    seed(&b).await;
    b.scheduler
        .submit(&job("job-1", "run-1"), None)
        .await
        .expect("submit");

    let claims = JobFaults::new();
    let a_parts = Parts::policy()
        .create_plate(holding(&effects, &a_hold, Arc::clone(&a_went_on)))
        .overrides(Overrides {
            jobs: Some(claims.wrapper()),
            spawner: Some(Arc::new(ManualSpawner::holding())),
            ..Overrides::default()
        });
    let a = Background::start(build("worker-a", &db, &config, a_parts).await);
    a_hold.reached(WAIT).await;
    let a_claim = claims.claims().first().cloned().expect("worker A claimed");

    // B claims once A's lease runs out in database time, and holds its call.
    let worker = Arc::clone(&b);
    let b = Background::start(b);
    b_hold.reached(WAIT).await;

    // A's fence is stale in the event log, the step store and the SDR.
    let ctx = fenced_ctx(&a_claim, id("corr-a"));
    let appended = worker
        .events
        .append(
            new_event("run-1", chrono::Utc::now(), marker_payload(1)),
            ctx.write_authority(),
        )
        .await;
    reclaimed_by(appended.expect_err("A appended"), "worker-b");
    let mut step = new_step("step-9.1", "create_plate", "{}");
    step.idempotency_key = id("job-1:step-9.1");
    let begun = worker.steps.begin(&step, ctx.write_authority()).await;
    reclaimed_by(begun.expect_err("A began a step"), "worker-b");
    let transition: Transition = parse(json!({
        "schema_version": "0.1",
        "id": "tr-zombie",
        "base_version": 1,
        "actor": { "kind": "agent", "agent_id": "main_orchestrator" },
        "reason": "A write from a reclaimed worker.",
        "final": false,
        "operations": [{
            "op": "add_decision",
            "decision": {
                "id": "dec-zombie",
                "choice": "Anything.",
                "rationale": "It must not land.",
                "related_ids": ["R-mass"]
            }
        }]
    }));
    let proposed = worker.sdr.propose(id(PROJECT), transition);
    let validated = match worker
        .sdr
        .validate(proposed, &ctx)
        .await
        .expect("validation runs")
    {
        sdr::Validation::Valid(validated) => validated,
        sdr::Validation::Invalid(invalid) => panic!("the transition is invalid: {invalid:?}"),
    };
    let committed = worker
        .sdr
        .commit(
            validated,
            sdr::CommitInfo {
                evidence_ids: Vec::new(),
                harness: harness(),
            },
            &ctx,
        )
        .await;
    reclaimed_by(committed.expect_err("A committed"), "worker-b");

    // A's call returns: its step completion is refused, and its run ends
    // without writing anything. B is still holding, so the log must not grow.
    let before = logged(&worker, "run-1").await.len();
    a_hold.release();
    let deadline = tokio::time::Instant::now() + WAIT;
    while !a_went_on.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "A's call never returned"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = logged(&worker, "run-1").await;
    assert_eq!(
        after.len(),
        before,
        "worker A wrote after losing its lease: {:?}",
        &after[before..]
    );
    a.stop().await;

    b_hold.release();
    let stored = wait_for(&worker, "job-1", Completed).await;
    b.stop().await;
    assert_eq!(head(&worker).await, 2, "only B's final commit landed");
    invariants::hold(&worker, &stored, Some(&effects)).await;
    drop(worker);
    db.drop().await;
}

/// Two workers poll one job: one claims it, once.
#[tokio::test(flavor = "multi_thread")]
async fn two_workers_race_for_one_job() {
    let db = TestDb::create().await;
    let config = no_approvals();
    let a = build("worker-a", &db, &config, Parts::policy()).await;
    let b = build("worker-b", &db, &config, Parts::policy()).await;
    seed(&a).await;
    a.scheduler
        .submit(&job("job-1", "run-1"), None)
        .await
        .expect("submit");
    let (ra, rb) = (
        Background::start(Arc::clone(&a)),
        Background::start(Arc::clone(&b)),
    );
    let stored = wait_for(&a, "job-1", Completed).await;
    ra.stop().await;
    rb.stop().await;
    assert_eq!(
        chain(&a, "run-1").await,
        [
            (None, Queued),
            (Some(Queued), Running),
            (Some(Running), Completed)
        ],
        "a second claim"
    );
    invariants::hold(&a, &stored, None).await;
    db.drop().await;
}

/// A cancel that arrives during a tool call stops the run before it
/// commits.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_while_running_stops_the_run() {
    let db = TestDb::create().await;
    let config = no_approvals();
    let effects = KeyedEffects::new();
    let hold = Pause::new("create_plate", At::Before);
    let worker = build(
        "worker-a",
        &db,
        &config,
        Parts::policy().create_plate(holding(&effects, &hold, Arc::new(AtomicBool::new(false)))),
    )
    .await;
    seed(&worker).await;
    worker
        .scheduler
        .submit(&job("job-1", "run-1"), None)
        .await
        .expect("submit");
    let running = Background::start(Arc::clone(&worker));
    hold.reached(WAIT).await;
    cancel(&worker, "run-1").await;
    hold.release();
    let stored = wait_for(&worker, "job-1", Cancelled).await;
    running.stop().await;
    assert_eq!(head(&worker).await, 1, "a cancelled run committed");
    invariants::hold(&worker, &stored, Some(&effects)).await;
    db.drop().await;
}

/// A cancel wakes a suspended job, which stops without a model call.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_while_suspended_stops_without_a_model_call() {
    let db = TestDb::create().await;
    let config = TestConfig::new(&[]);
    let model = Arc::new(PolicyModel::new());
    let worker = build("worker-a", &db, &config, Parts::new(model.clone())).await;
    seed(&worker).await;
    worker
        .scheduler
        .submit(&job("job-1", "run-1"), None)
        .await
        .expect("submit");
    let running = Background::start(Arc::clone(&worker));
    wait_for(&worker, "job-1", Suspended).await;
    let calls = model.requests();
    cancel(&worker, "run-1").await;
    let stored = wait_for(&worker, "job-1", Cancelled).await;
    running.stop().await;
    assert_eq!(
        model.requests(),
        calls,
        "the cancelled run called the model"
    );
    assert_eq!(head(&worker).await, 1, "a cancelled run committed");
    invariants::hold(&worker, &stored, None).await;
    db.drop().await;
}

/// A retryable model error sends the job back to the queue; the retry
/// completes it, and the progress it makes clears the failed attempt.
#[tokio::test(flavor = "multi_thread")]
async fn a_retryable_error_requeues_the_job() {
    let db = TestDb::create().await;
    let config = no_approvals();
    let model = PolicyModel::failing(ErrorCode::Unavailable, 1);
    let worker = build("worker-a", &db, &config, Parts::new(Arc::new(model))).await;
    seed(&worker).await;
    worker
        .scheduler
        .submit(&job("job-1", "run-1"), None)
        .await
        .expect("submit");
    let running = Background::start(Arc::clone(&worker));
    let stored = wait_for(&worker, "job-1", Completed).await;
    running.stop().await;
    assert_eq!(
        stored.attempt_history.len(),
        1,
        "{:?}",
        stored.attempt_history
    );
    assert_eq!(stored.failed_attempts, 0, "progress clears failed attempts");
    assert!(
        chain(&worker, "run-1")
            .await
            .contains(&(Some(Running), Queued)),
        "no requeue"
    );
    invariants::hold(&worker, &stored, None).await;
    db.drop().await;
}

/// A model that never answers uses up the attempts; the job fails.
#[tokio::test(flavor = "multi_thread")]
async fn running_out_of_attempts_fails_the_job() {
    let db = TestDb::create().await;
    let config = no_approvals();
    let model = PolicyModel::failing(ErrorCode::Unavailable, usize::MAX);
    let worker = build("worker-a", &db, &config, Parts::new(Arc::new(model))).await;
    seed(&worker).await;
    worker
        .scheduler
        .submit(&job("job-1", "run-1"), None)
        .await
        .expect("submit");
    let running = Background::start(Arc::clone(&worker));
    let stored = wait_for(&worker, "job-1", Failed).await;
    running.stop().await;
    let JobStatus::Failed { failure, .. } = &stored.status else {
        unreachable!("wait_for returned a failed job");
    };
    assert_eq!(failure.kind, JobFailureKind::AttemptsExhausted);
    assert_eq!(
        stored.attempt_history.len(),
        3,
        "{:?}",
        stored.attempt_history
    );
    invariants::hold(&worker, &stored, None).await;
    db.drop().await;
}

/// A worker shutting down gives its job back, which is not a failed
/// attempt; another worker finishes it.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_releases_the_job_without_a_failed_attempt() {
    let db = TestDb::create().await;
    let config = no_approvals();
    let effects = KeyedEffects::new();
    let hold = Pause::new("create_plate", At::Before);
    let a = build(
        "worker-a",
        &db,
        &config,
        Parts::policy().create_plate(holding(&effects, &hold, Arc::new(AtomicBool::new(false)))),
    )
    .await;
    seed(&a).await;
    a.scheduler
        .submit(&job("job-1", "run-1"), None)
        .await
        .expect("submit");
    let running = Background::start(Arc::clone(&a));
    hold.reached(WAIT).await;
    running.stop().await;
    let released = a.jobs.get(&id("job-1")).await.expect("the job");
    assert!(
        matches!(released.status, JobStatus::Queued { .. }),
        "{:?}",
        released.status
    );
    assert_eq!(released.failed_attempts, 0);

    let b = build(
        "worker-b",
        &db,
        &config,
        Parts::policy().create_plate(effects.tool(
            "create_plate",
            |_| plate_result(),
            testkit::fakes::go_on,
        )),
    )
    .await;
    let running = Background::start(Arc::clone(&b));
    let stored = wait_for(&b, "job-1", Completed).await;
    running.stop().await;
    assert!(
        stored.attempt_history.is_empty(),
        "{:?}",
        stored.attempt_history
    );
    invariants::hold(&b, &stored, Some(&effects)).await;
    drop(a);
    db.drop().await;
}
