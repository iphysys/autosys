//! Crash points (ADR-0031): worker A runs a job until a pause stops it at
//! one point, then dies there. Worker B, built fresh on the same database,
//! reclaims the job once A's lease expires and runs it to its end. Nothing
//! may happen twice and the invariants must hold.
//!
//! One table drives one test, so a new point is one more row. Reserved rows
//! name points a later increment adds; they are listed, not run.
//!
//! `lease_lost_mid_run`, where the old worker lives on after losing its
//! lease, is `scenarios::a_reclaimed_worker_cannot_write`.

use std::sync::Arc;

use contracts::types::{EventPayload, JobState, JobStatus};
use contracts::{ErrorCode, ModelProvider, job_state};
use serde_json::Value;
use testkit::fakes::{FakeTool, HookFuture, KeyedEffects};
use testkit::faults::{
    At, EventFaults, JobFaults, Pause, PausingModelProvider, SdrFaults, StepFaults,
};
use tokio::task::JoinSet;

use crate::support::db::TestDb;
use crate::support::*;

use JobState::{Cancelled, Completed, Failed, Queued, Suspended};

/// Where worker A stops.
#[derive(Clone, Copy, Debug)]
enum Fault {
    /// After the first step's record is completed, before its
    /// `tool_call_completed`.
    StepComplete,
    /// Around the SDR storage's append of the commit.
    SdrCommit(At),
    /// In the first `create_plate` call, after its effect is stored, before
    /// its result returns.
    ToolEffect { idempotent: bool },
    /// Suspended, then A is gone; B wakes the job on its answer.
    Suspended,
    /// At the C3 save, after `approval_requested`.
    ApprovalSave,
    /// At `{running → suspended}`, after C3.
    SuspendEvent,
    /// At the save into this state, after its `job_state_changed`.
    TransitionSave(JobState),
    /// At `{running → completed}`, after `run_stopped`.
    StopEvent,
    /// Before the second model call reaches the model.
    ModelCall,
    /// At the first checkpoint, after `model_call_completed`.
    FirstCheckpoint,
    /// Not run in this increment, for the reason given.
    Reserved(&'static str),
}

struct CrashPoint {
    name: &'static str,
    fault: Fault,
    /// The committed autonomy policy (ask before the final commit) instead
    /// of none; B answers the approval when the job suspends.
    approvals: bool,
    /// Where the job ends.
    ends: JobState,
}

const fn point(name: &'static str, fault: Fault, ends: JobState) -> CrashPoint {
    CrashPoint {
        name,
        fault,
        approvals: false,
        ends,
    }
}

const fn asking(name: &'static str, fault: Fault) -> CrashPoint {
    CrashPoint {
        name,
        fault,
        approvals: true,
        ends: Completed,
    }
}

const POINTS: &[CrashPoint] = &[
    point("step_recorded_before_event", Fault::StepComplete, Completed),
    point(
        "sdr_commit_before_write",
        Fault::SdrCommit(At::Before),
        Completed,
    ),
    point(
        "sdr_commit_after_write",
        Fault::SdrCommit(At::After),
        Completed,
    ),
    point(
        "tool_effect_before_result/idempotent",
        Fault::ToolEffect { idempotent: true },
        Completed,
    ),
    point(
        "tool_effect_before_result/non_idempotent",
        Fault::ToolEffect { idempotent: false },
        Completed,
    ),
    asking("waiting_on_approval", Fault::Suspended),
    asking("approval_requested_before_checkpoint", Fault::ApprovalSave),
    asking("checkpoint_before_transition", Fault::SuspendEvent),
    asking(
        "transition_before_save/suspended",
        Fault::TransitionSave(Suspended),
    ),
    point(
        "transition_before_save/completed",
        Fault::TransitionSave(Completed),
        Completed,
    ),
    point(
        "transition_before_save/failed",
        Fault::TransitionSave(Failed),
        Failed,
    ),
    point(
        "transition_before_save/cancelled",
        Fault::TransitionSave(Cancelled),
        Cancelled,
    ),
    point(
        "transition_before_save/queued",
        Fault::TransitionSave(Queued),
        Completed,
    ),
    point(
        "transition_before_save/waiting",
        Fault::Reserved("nothing enters waiting in PR 12"),
        Completed,
    ),
    point("stopped_before_transition", Fault::StopEvent, Completed),
    point("mid_model_call", Fault::ModelCall, Completed),
    point("event_before_checkpoint", Fault::FirstCheckpoint, Completed),
    point(
        "kernel_killed_mid_session",
        Fault::Reserved("kernel sessions arrive in Increment 3 (ADR-0013)"),
        Completed,
    ),
    point(
        "parent_waiting_on_child",
        Fault::Reserved("child jobs arrive in Increment 3"),
        Completed,
    ),
];

#[tokio::test(flavor = "multi_thread")]
async fn every_crash_point_recovers() {
    let mut points = JoinSet::new();
    for point in POINTS {
        if let Fault::Reserved(why) = point.fault {
            println!("reserved: {} ({why})", point.name);
            continue;
        }
        points.spawn(async move { (point.name, tokio::spawn(run(point)).await) });
    }
    let mut failed = Vec::new();
    while let Some(joined) = points.join_next().await {
        let (name, ran) = joined.expect("the point's task is not cancelled");
        match ran {
            Ok(()) => println!("recovered: {name}"),
            Err(error) => {
                let message = match error.try_into_panic() {
                    Ok(panic) => panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                        .unwrap_or_default(),
                    Err(error) => error.to_string(),
                };
                failed.push(format!("{name}: {message}"));
            }
        }
    }
    assert!(
        failed.is_empty(),
        "crash points failed:\n{}",
        failed.join("\n")
    );
}

async fn run(point: &'static CrashPoint) {
    let db = TestDb::create().await;
    let overlay = idempotent_create_plate();
    let mut replaced = Vec::new();
    if !point.approvals {
        replaced.push(("policies/autonomy.yaml", NO_APPROVALS_YAML));
    }
    if let Fault::ToolEffect { idempotent: true } = point.fault {
        replaced.push(("tools/build123d.yaml", overlay.as_str()));
    }
    let config = TestConfig::new(&replaced);
    let effects = KeyedEffects::new();
    let at = match point.fault {
        Fault::StepComplete => At::After,
        Fault::SdrCommit(at) => at,
        _ => At::Before,
    };
    let pause = Pause::new(point.name, at);

    // B is built first, without faults, and seeds and submits; it starts
    // only once A is gone.
    let b_model = Arc::new(PolicyModel::new());
    let b = build(
        "worker-b",
        &db,
        &config,
        Parts::new(b_model.clone()).create_plate(keyed(&effects, None)),
    )
    .await;
    seed(&b).await;
    let submitted = job("job-1", "run-1");
    b.scheduler.submit(&submitted, None).await.expect("submit");
    if let Fault::TransitionSave(Cancelled) = point.fault {
        cancel(&b, "run-1").await;
    }

    let a = build("worker-a", &db, &config, arm(point, &pause, &effects)).await;
    let a = Background::start(a);
    match point.fault {
        Fault::Suspended => {
            wait_for(&a.worker, "job-1", Suspended).await;
        }
        _ => pause.reached(WAIT).await,
    }
    a.abort().await;

    let worker = Arc::clone(&b);
    let b = Background::start(b);
    let mut stored = wait_until(&worker, "job-1", |state| {
        state == point.ends || (point.approvals && state == Suspended)
    })
    .await;
    if point.approvals && job_state(&stored.status) == Suspended {
        approve(&worker, &stored).await;
        stored = wait_for(&worker, "job-1", point.ends).await;
    }
    b.stop().await;

    invariants::hold(&worker, &stored, Some(&effects)).await;
    expect(point, &worker, b_model.requests(), &effects).await;
    drop(worker);
    db.drop().await;
}

/// Worker A's parts: the fault armed at `pause`.
fn arm(point: &CrashPoint, pause: &Pause, effects: &KeyedEffects) -> Parts {
    let model: Arc<dyn ModelProvider> = match point.fault {
        Fault::TransitionSave(Queued) => {
            Arc::new(PolicyModel::failing_first(ErrorCode::Unavailable))
        }
        Fault::TransitionSave(Failed) => {
            Arc::new(PolicyModel::failing_first(ErrorCode::InvalidInput))
        }
        Fault::ModelCall => Arc::new(PausingModelProvider::new(
            Arc::new(PolicyModel::new()),
            pause,
            1,
        )),
        _ => Arc::new(PolicyModel::new()),
    };
    let saves = |matches: fn(
        &contracts::types::Job,
        Option<&contracts::types::ContinuationRecord>,
    ) -> bool| { Some(JobFaults::new().pause_save(pause, matches).wrapper()) };
    let mut overrides = bootstrap::Overrides::default();
    match point.fault {
        Fault::StepComplete => {
            overrides.steps = Some(StepFaults::pause_complete(pause).wrapper());
        }
        Fault::SdrCommit(_) => {
            overrides.sdr_storage = Some(SdrFaults::pause_commit(pause).wrapper());
        }
        Fault::ApprovalSave => {
            overrides.jobs = saves(|_, record| record.is_some_and(|r| r.approval_id.is_some()));
        }
        Fault::FirstCheckpoint => overrides.jobs = saves(|_, record| record.is_some()),
        Fault::TransitionSave(state) => {
            let state_of =
                move |job: &contracts::types::Job,
                      _: Option<&contracts::types::ContinuationRecord>| {
                    job_state(&job.status) == state
                };
            overrides.jobs = Some(JobFaults::new().pause_save(pause, state_of).wrapper());
        }
        Fault::SuspendEvent => {
            overrides.events = Some(
                EventFaults::pause_append(pause, |payload| {
                    matches!(
                        payload,
                        EventPayload::JobStateChanged {
                            to: JobStatus::Suspended { .. },
                            ..
                        }
                    )
                })
                .wrapper(),
            );
        }
        Fault::StopEvent => {
            overrides.events = Some(
                EventFaults::pause_append(pause, |payload| {
                    matches!(
                        payload,
                        EventPayload::JobStateChanged {
                            to: JobStatus::Completed { .. },
                            ..
                        }
                    )
                })
                .wrapper(),
            );
        }
        _ => {}
    }
    let tool_pause = matches!(point.fault, Fault::ToolEffect { .. }).then(|| pause.clone());
    Parts::new(model)
        .create_plate(keyed(effects, tool_pause))
        .overrides(overrides)
}

/// `create_plate` over the shared keyed store; it stops at `pause` after
/// storing its effect, if one is given.
fn keyed(effects: &KeyedEffects, pause: Option<Pause>) -> FakeTool {
    effects.tool(
        "create_plate",
        |_| plate_result(),
        move |_, _| {
            let pause = pause.clone();
            Box::pin(async move {
                if let Some(pause) = pause {
                    pause.stop().await;
                }
                Ok(())
            }) as HookFuture
        },
    )
}

/// What each point must show beyond the invariants. `b_calls` is how many
/// model calls B made.
async fn expect(
    point: &CrashPoint,
    worker: &bootstrap::Worker,
    b_calls: usize,
    effects: &KeyedEffects,
) {
    let events: Vec<Value> = logged(worker, "run-1")
        .await
        .iter()
        .map(|e| serde_json::to_value(&e.payload).expect("a payload"))
        .collect();
    let of = |kind: &str| -> Vec<&Value> { events.iter().filter(|v| v["type"] == kind).collect() };
    let first_key = of("tool_call_started")
        .first()
        .and_then(|v| v["idempotency_key"].as_str())
        .unwrap_or_default()
        .to_owned();
    let name = point.name;
    match point.fault {
        Fault::StepComplete => {
            assert_eq!(effects.calls(&first_key), 1, "{name}: the tool ran again");
            assert!(
                of("tool_call_completed")
                    .iter()
                    .any(|v| v["recovered"] == true),
                "{name}: no recovered tool_call_completed"
            );
        }
        Fault::SdrCommit(At::After) => assert!(
            of("sdr_transition").iter().any(|v| v["recovered"] == true),
            "{name}: no recovered sdr_transition"
        ),
        Fault::ToolEffect { idempotent: true } => {
            assert_eq!(effects.calls(&first_key), 2, "{name}: not called again");
            assert_eq!(effects.effects(), 1, "{name}: a second effect");
        }
        Fault::ToolEffect { idempotent: false } => {
            assert_eq!(effects.calls(&first_key), 1, "{name}: called again");
            assert!(
                of("tool_call_completed")
                    .iter()
                    .any(|v| v["error"]["code"] == "outcome_unknown"),
                "{name}: no outcome_unknown"
            );
        }
        Fault::TransitionSave(Completed | Failed | Cancelled) | Fault::StopEvent => {
            assert_eq!(b_calls, 0, "{name}: B called the model");
        }
        _ => {}
    }
}
