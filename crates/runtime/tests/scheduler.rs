//! The scheduler in memory (ADR-0031): claims, job states, waking, retries,
//! a lost lease, shutdown, and a transition a dead worker decided. Each job
//! here is job-2 of run-2; the test world's own job-1 holds a lease for the
//! whole test and is never claimed.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "integration tests")]

mod support;

use std::sync::Arc;
use std::time::Duration;

use contracts::ids::RunId;
use contracts::types::{EventPayload, Job, JobFailureKind, JobState, JobStatus, WakeCondition};
use contracts::{Clock, Error, ErrorCode, EventKind, EventLog, JobStore, Spawner, job_state};
use runtime::{Ended, Scheduler, SchedulerSettings, Tick, checkpoint_ref};
use serde_json::json;
use support::*;
use testkit::builders::{id, new_event};
use testkit::fakes::HookFuture;
use testkit::faults::{At, EventFaults, JobFaults, Pause};
use testkit::leases::{api_command, job_submit};
use testkit::spawn::ManualSpawner;
use tokio_util::sync::CancellationToken;

const LEASE: Duration = Duration::from_secs(30);
const WAIT: Duration = Duration::from_secs(10);
const RUN_2: &str = "run-2";

fn settings(holder: &str) -> SchedulerSettings {
    SchedulerSettings {
        holder: id(holder),
        lease: LEASE,
        heartbeat: Duration::from_secs(10),
        poll: Duration::from_millis(10),
        poll_jitter: Duration::ZERO,
        max_attempts: 3,
        backoff_min: Duration::from_secs(5),
        backoff_max: Duration::from_secs(60),
    }
}

fn scheduler_with(world: &World, holder: &str, jobs: Arc<dyn JobStore>) -> Scheduler {
    let spawner: Arc<dyn Spawner> = Arc::new(ManualSpawner::holding());
    Scheduler::new(
        jobs,
        world.events.clone(),
        world.sdr.clone(),
        world.orchestrator.clone(),
        spawner,
        world.clock.clone(),
        settings(holder),
    )
}

fn scheduler(world: &World, holder: &str) -> Scheduler {
    scheduler_with(world, holder, world.leased.jobs().clone())
}

/// job-2 of run-2, as a client submits it.
fn job2() -> Job {
    let mut job = job_for(task_json(&["R-mass"], roomy_budget()));
    job.id = id("job-2");
    job.run_id = id(RUN_2);
    job.idempotency_key = id("job-2");
    job.status = JobStatus::Queued { checkpoint: None };
    job.attempt = 0;
    job
}

fn succeeding() -> Vec<ScriptStep> {
    vec![
        call_tools(vec![("c1", "create_plate", plate())]),
        call_tools(vec![("c2", "measure", measure_args())]),
        from_request(|request| {
            let evidence = last_evidence(request);
            tool_use(&[(
                "c3".to_owned(),
                "sdr_propose".to_owned(),
                propose_mass(MASS_G, &evidence, true),
            )])
        }),
    ]
}

async fn stored(world: &World) -> Job {
    world.leased.jobs().get(&id("job-2")).await.expect("job-2")
}

/// run-2's job_state_changed records, as (from, to).
async fn chain(world: &World) -> Vec<(Option<JobState>, JobState)> {
    world
        .events
        .read(&id::<RunId>(RUN_2), 0, 10_000)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            EventPayload::JobStateChanged { from, to, .. } => Some((from, job_state(&to))),
            _ => None,
        })
        .collect()
}

/// The job's status names the record it holds.
async fn assert_rests_on_its_record(world: &World) {
    let job = stored(world).await;
    let record = world
        .leased
        .jobs()
        .continuation(&job.id)
        .await
        .unwrap()
        .expect("a record");
    let named = match &job.status {
        JobStatus::Completed { checkpoint, .. }
        | JobStatus::Cancelled { checkpoint }
        | JobStatus::Failed { checkpoint, .. }
        | JobStatus::Suspended { checkpoint, .. } => checkpoint.clone(),
        other => panic!("{other:?} names no checkpoint"),
    };
    assert_eq!(checkpoint_ref(&record).unwrap(), named);
}

/// The record's harness is the projection of run-2's log (ADR-0033), with
/// one build per segment the run had.
async fn assert_harness_is_the_logs(world: &World, segments: usize) {
    let job = stored(world).await;
    let record = world
        .leased
        .jobs()
        .continuation(&job.id)
        .await
        .unwrap()
        .expect("a record");
    let events = world
        .events
        .read(&id::<RunId>(RUN_2), 0, 10_000)
        .await
        .unwrap();
    let harness = record.harness.expect("a 0.2 record");
    assert_eq!(harness, contracts::provenance::fold(&events));
    assert_eq!(harness.harness_builds.len(), segments, "{harness:?}");
    assert!(!harness.model_ids.is_empty(), "{harness:?}");
    assert!(!harness.prompts.is_empty(), "{harness:?}");
    assert_eq!(harness.agent_specs.len(), 1, "{harness:?}");
}

use JobState::{Cancelled, Completed, Failed, Queued, Running, Suspended};

#[tokio::test]
async fn a_submitted_job_runs_to_completion() {
    let world = World::new(Setup::new(succeeding())).await;
    let scheduler = scheduler(&world, "worker-a");
    scheduler.submit(&job2(), None).await.unwrap();
    let tick = scheduler.tick(&CancellationToken::new()).await.unwrap();
    assert_eq!(
        tick,
        Tick::Ran {
            job: id("job-2"),
            ended: Ended::Left(Completed)
        }
    );
    let job = stored(&world).await;
    assert!(
        matches!(&job.status, JobStatus::Completed { stop_reason, .. } if stop_reason.to_string() == "success"),
        "{:?}",
        job.status
    );
    assert_eq!(job.lease, None);
    assert_rests_on_its_record(&world).await;
    assert_harness_is_the_logs(&world, 1).await;
    assert_eq!(
        chain(&world).await,
        [
            (None, Queued),
            (Some(Queued), Running),
            (Some(Running), Completed)
        ]
    );
    assert_eq!(
        scheduler.tick(&CancellationToken::new()).await.unwrap(),
        Tick::Scanned { woken: 0 },
        "nothing is left to claim"
    );
}

/// `Reports` keeps the run-reports contract (ADR-0033) on a run the
/// scheduler completed, while the world's own run-1 is still running.
#[tokio::test]
async fn reports_keep_the_run_reports_contract() {
    let world = World::new(Setup::new(succeeding())).await;
    let scheduler = scheduler(&world, "worker-a");
    scheduler.submit(&job2(), None).await.unwrap();
    assert!(matches!(
        scheduler.tick(&CancellationToken::new()).await.unwrap(),
        Tick::Ran {
            ended: Ended::Left(Completed),
            ..
        }
    ));
    let assembler = artifacts::ReportAssembler::new(
        artifacts::ReportSources {
            jobs: world.leased.jobs().clone(),
            events: world.events.clone(),
            evidence: world.evidence.clone(),
            artifacts: Arc::new(testkit::stores::InMemoryArtifactStore::new()),
            sdr: world.sdr.clone(),
        },
        Duration::from_secs(5),
    )
    .unwrap();
    testkit::contract::run_reports(testkit::contract::ReportsWorld {
        reports: Arc::new(runtime::Reports::new(assembler)),
        not_stopped: id("run-1"),
        stopped: id(RUN_2),
    })
    .await;
}

#[tokio::test]
async fn a_suspended_job_wakes_on_its_answer_and_completes() {
    let world = World::new(Setup::new(succeeding()).autonomy(committed_autonomy())).await;
    let scheduler = scheduler(&world, "worker-a");
    let shutdown = CancellationToken::new();
    scheduler.submit(&job2(), None).await.unwrap();
    assert!(matches!(
        scheduler.tick(&shutdown).await.unwrap(),
        Tick::Ran {
            ended: Ended::Left(Suspended),
            ..
        }
    ));
    let job = stored(&world).await;
    let JobStatus::Suspended {
        wake_condition: WakeCondition::ApprovalAnswered(approval_id),
        ..
    } = job.status.clone()
    else {
        panic!("{:?}", job.status);
    };
    assert_rests_on_its_record(&world).await;
    let parked = world.leased.jobs().parked(10).await.unwrap();
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].checked_seq, job.progress.last_event_seq);
    assert_eq!(
        scheduler.tick(&shutdown).await.unwrap(),
        Tick::Scanned { woken: 0 },
        "no answer yet"
    );

    world
        .events
        .append(
            new_event(
                RUN_2,
                world.clock.now(),
                json!({ "type": "approval_answered", "approval_id": approval_id.as_str(), "answer": "approve" }),
            ),
            Some(&api_command()),
        )
        .await
        .unwrap();
    assert_eq!(
        scheduler.tick(&shutdown).await.unwrap(),
        Tick::Scanned { woken: 1 }
    );
    assert!(matches!(
        stored(&world).await.status,
        JobStatus::Queued {
            checkpoint: Some(_)
        }
    ));
    assert!(matches!(
        scheduler.tick(&shutdown).await.unwrap(),
        Tick::Ran {
            ended: Ended::Left(Completed),
            ..
        }
    ));
    assert_eq!(world.head().await, 2);
    assert_eq!(world.create_plate.calls(), 1);
    assert_eq!(
        chain(&world).await,
        [
            (None, Queued),
            (Some(Queued), Running),
            (Some(Running), Suspended),
            (Some(Suspended), Queued),
            (Some(Queued), Running),
            (Some(Running), Completed)
        ]
    );
    assert_harness_is_the_logs(&world, 2).await;
}

#[tokio::test]
async fn a_cancel_wakes_a_suspended_job_which_stops_without_a_model_call() {
    let world = World::new(Setup::new(succeeding()).autonomy(committed_autonomy())).await;
    let scheduler = scheduler(&world, "worker-a");
    let shutdown = CancellationToken::new();
    scheduler.submit(&job2(), None).await.unwrap();
    scheduler.tick(&shutdown).await.unwrap();
    let requests = world.model.requests().len();
    world
        .events
        .append(
            new_event(
                RUN_2,
                world.clock.now(),
                json!({ "type": "cancel_requested" }),
            ),
            Some(&api_command()),
        )
        .await
        .unwrap();
    assert_eq!(
        scheduler.tick(&shutdown).await.unwrap(),
        Tick::Scanned { woken: 1 }
    );
    assert!(matches!(
        scheduler.tick(&shutdown).await.unwrap(),
        Tick::Ran {
            ended: Ended::Left(Cancelled),
            ..
        }
    ));
    assert_eq!(
        world.model.requests().len(),
        requests,
        "the model was called"
    );
    assert_rests_on_its_record(&world).await;
    assert_eq!(world.head().await, 1);
}

/// Every attempt's model call fails as unavailable, through the gateway's
/// own retries. Each failed attempt requeues the job behind a longer delay,
/// and the third fails it.
#[tokio::test(start_paused = true)]
async fn retryable_failures_requeue_with_a_delay_until_the_attempts_run_out() {
    let unavailable =
        || -> ScriptStep { Box::new(|_| Err(Error::new(ErrorCode::Unavailable, "overloaded"))) };
    let world = World::new(Setup::new((0..30).map(|_| unavailable()).collect())).await;
    let scheduler = scheduler(&world, "worker-a");
    let shutdown = CancellationToken::new();
    scheduler.submit(&job2(), None).await.unwrap();

    for (attempt, delay) in [(1u64, 5u64), (2, 10)] {
        assert!(matches!(
            scheduler.tick(&shutdown).await.unwrap(),
            Tick::Ran {
                ended: Ended::Left(Queued),
                ..
            }
        ));
        let job = stored(&world).await;
        assert_eq!(job.failed_attempts, attempt);
        assert_eq!(
            job.not_before
                .map(|t| (t.0 - world.clock.now()).num_seconds()),
            Some(delay as i64),
            "attempt {attempt}"
        );
        assert_eq!(
            scheduler.tick(&shutdown).await.unwrap(),
            Tick::Scanned { woken: 0 },
            "claimed before its delay"
        );
        world.clock.advance(Duration::from_secs(delay + 1));
    }
    assert!(matches!(
        scheduler.tick(&shutdown).await.unwrap(),
        Tick::Ran {
            ended: Ended::Left(Failed),
            ..
        }
    ));
    let job = stored(&world).await;
    match &job.status {
        JobStatus::Failed { failure, .. } => {
            assert_eq!(failure.kind, JobFailureKind::AttemptsExhausted);
            assert_eq!(failure.error.code.as_str(), "unavailable");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(job.attempt_history.len(), 3);
    assert_rests_on_its_record(&world).await;
}

/// Worker A's lease is lost while create_plate runs: another worker claims
/// the job. A's next write fails, and A drops the job without a trace.
#[tokio::test]
async fn a_lost_lease_drops_the_run_without_a_trace() {
    let hook_world: testkit::fakes::Slot<(
        Arc<testkit::stores::InMemoryJobStore>,
        Arc<testkit::clock::ManualClock>,
    )> = testkit::fakes::Slot::new("world");
    let hooked = {
        let slot = hook_world.clone();
        returning("create_plate", plate_result()).before_answer(move |index, _| {
            let slot = slot.clone();
            Box::pin(async move {
                if index == 0 {
                    let (jobs, clock) = slot.get();
                    clock.advance(LEASE + Duration::from_secs(1));
                    jobs.claim(&id("worker-b"), LEASE)
                        .await
                        .unwrap()
                        .expect("worker-b claims the expired job");
                }
                Ok(())
            }) as HookFuture
        })
    };
    let world = World::new(Setup::new(succeeding()).create_plate(hooked)).await;
    hook_world.fill((world.leased.jobs().clone(), world.clock.clone()));
    let scheduler = scheduler(&world, "worker-a");
    scheduler.submit(&job2(), None).await.unwrap();
    assert_eq!(
        scheduler.tick(&CancellationToken::new()).await.unwrap(),
        Tick::Ran {
            job: id("job-2"),
            ended: Ended::LeaseLost
        }
    );
    let job = stored(&world).await;
    assert_eq!(job.status, JobStatus::Running);
    assert_eq!(
        job.lease.map(|l| l.holder.as_str().to_owned()).as_deref(),
        Some("worker-b")
    );
    let logged = world
        .events
        .read(&id::<RunId>(RUN_2), 0, 1000)
        .await
        .unwrap();
    for event in &logged {
        assert!(
            !matches!(
                event.payload,
                EventPayload::ToolCallCompleted { .. } | EventPayload::RunStopped { .. }
            ),
            "worker-a wrote after losing its lease: {:?}",
            event.payload
        );
    }
}

/// Shutting down mid-call gives the job back, queued on its last
/// checkpoint, without counting a failed attempt; it then runs to the end.
#[tokio::test]
async fn shutting_down_releases_the_job() {
    let shutdown = CancellationToken::new();
    let hooked = {
        let shutdown = shutdown.clone();
        returning("create_plate", plate_result()).before_answer(move |index, _| {
            let shutdown = shutdown.clone();
            Box::pin(async move {
                if index == 0 {
                    shutdown.cancel();
                    std::future::pending::<()>().await;
                }
                Ok(())
            }) as HookFuture
        })
    };
    let mut script = succeeding();
    script.insert(1, call_tools(vec![("c1b", "create_plate", plate())]));
    let world = World::new(Setup::new(script).create_plate(hooked)).await;
    let scheduler = scheduler(&world, "worker-a");
    scheduler.submit(&job2(), None).await.unwrap();
    assert!(matches!(
        scheduler.tick(&shutdown).await.unwrap(),
        Tick::Ran {
            ended: Ended::Released,
            ..
        }
    ));
    let job = stored(&world).await;
    assert!(
        matches!(
            job.status,
            JobStatus::Queued {
                checkpoint: Some(_)
            }
        ),
        "{:?}",
        job.status
    );
    assert_eq!(job.failed_attempts, 0);
    assert_eq!(job.not_before, None);
    assert!(matches!(
        scheduler.tick(&CancellationToken::new()).await.unwrap(),
        Tick::Ran {
            ended: Ended::Left(Completed),
            ..
        }
    ));
    let chain = chain(&world).await;
    assert_eq!(chain[2], (Some(Running), Queued), "{chain:?}");
}

/// Worker A records the run's completion and stops before its save. Worker
/// B, claiming the expired job, finishes that save: nothing runs again and
/// nothing more is recorded.
#[tokio::test]
async fn a_transition_a_dead_worker_decided_is_finished_on_claim() {
    let world = World::new(Setup::new(succeeding())).await;
    let pause = Pause::new("before the completed save", At::Before);
    let faults = JobFaults::new().pause_save(&pause, |job, _| {
        matches!(job.status, JobStatus::Completed { .. })
    });
    let paused: Arc<dyn JobStore> = faults.wrapper()(world.leased.jobs().clone());
    let a = scheduler_with(&world, "worker-a", paused);
    a.submit(&job2(), None).await.unwrap();
    let running = CancellationToken::new();
    tokio::select! {
        tick = a.tick(&running) => panic!("worker-a finished: {tick:?}"),
        () = pause.reached(WAIT) => {}
    }
    drop(a);
    assert_eq!(stored(&world).await.status, JobStatus::Running);
    let requests = world.model.requests().len();

    world.clock.advance(LEASE + Duration::from_secs(1));
    let b = scheduler(&world, "worker-b");
    assert!(matches!(
        b.tick(&CancellationToken::new()).await.unwrap(),
        Tick::Ran {
            ended: Ended::Left(Completed),
            ..
        }
    ));
    assert_eq!(world.model.requests().len(), requests, "the run ran again");
    assert_rests_on_its_record(&world).await;
    assert_eq!(
        chain(&world).await,
        [
            (None, Queued),
            (Some(Queued), Running),
            (Some(Running), Completed)
        ],
        "the claim recorded itself, or the transition twice"
    );
}

/// Worker A records the run's stop and its final checkpoint, and dies
/// before the transition. Worker B records the transition from the stop:
/// the run does not stop a second time.
#[tokio::test]
async fn a_stop_a_dead_worker_recorded_is_finished_on_claim() {
    let world = World::new(Setup::new(succeeding())).await;
    let pause = Pause::new("before the completed transition", At::Before);
    let events: Arc<dyn EventLog> = EventFaults::pause_append(&pause, |payload| {
        matches!(
            payload,
            EventPayload::JobStateChanged {
                to: JobStatus::Completed { .. },
                ..
            }
        )
    })
    .wrapper()(world.events.clone());
    let a = Scheduler::new(
        world.leased.jobs().clone(),
        events,
        world.sdr.clone(),
        world.orchestrator.clone(),
        Arc::new(ManualSpawner::holding()),
        world.clock.clone(),
        settings("worker-a"),
    );
    a.submit(&job2(), None).await.unwrap();
    let running = CancellationToken::new();
    tokio::select! {
        tick = a.tick(&running) => panic!("worker-a finished: {tick:?}"),
        () = pause.reached(WAIT) => {}
    }
    drop(a);
    let requests = world.model.requests().len();

    world.clock.advance(LEASE + Duration::from_secs(1));
    let b = scheduler(&world, "worker-b");
    assert!(matches!(
        b.tick(&CancellationToken::new()).await.unwrap(),
        Tick::Ran {
            ended: Ended::Left(Completed),
            ..
        }
    ));
    assert_eq!(world.model.requests().len(), requests, "the run ran again");
    assert_rests_on_its_record(&world).await;
    let stops = world
        .events
        .read_kinds(&id::<RunId>(RUN_2), 0, &[EventKind::RUN_STOPPED], 100)
        .await
        .unwrap();
    assert_eq!(stops.len(), 1, "the run stopped twice");

    // Worker B's claim records its build, but finishing the stop must not
    // change the record its status names; so the record's harness lacks that
    // segment and the log has it. The run report folds the log (ADR-0033).
    let job = stored(&world).await;
    let record = world.leased.jobs().continuation(&job.id).await.unwrap();
    let cached = record.and_then(|r| r.harness).expect("a 0.2 record");
    let all = world
        .events
        .read(&id::<RunId>(RUN_2), 0, 10_000)
        .await
        .unwrap();
    let logged = contracts::provenance::fold(&all);
    assert_eq!(cached.harness_builds.len(), 1);
    assert_eq!(logged.harness_builds.len(), 2, "{logged:?}");
    assert_eq!(
        (cached.model_ids, cached.prompts),
        (logged.model_ids, logged.prompts),
        "only the build of the recovery-only segment differs"
    );
}

/// A submit that crashed after its insert left no record; the claim adds it.
#[tokio::test]
async fn a_claim_adds_the_record_a_crashed_submit_lost() {
    let world = World::new(Setup::new(succeeding())).await;
    world
        .leased
        .jobs()
        .insert(&job2(), Some(&job_submit()))
        .await
        .unwrap();
    let scheduler = scheduler(&world, "worker-a");
    scheduler.tick(&CancellationToken::new()).await.unwrap();
    assert_eq!(
        chain(&world).await,
        [
            (None, Queued),
            (Some(Queued), Running),
            (Some(Running), Completed)
        ]
    );
}

/// A cancel that arrives while the job runs stops the run with cancelled.
#[tokio::test]
async fn a_cancel_while_running_stops_the_run() {
    let events: testkit::fakes::Slot<Arc<testkit::stores::InMemoryEventLog>> =
        testkit::fakes::Slot::new("events");
    let hooked = {
        let events = events.clone();
        returning("create_plate", plate_result()).before_answer(move |_, _| {
            let events = events.clone();
            Box::pin(async move {
                events
                    .get()
                    .append(
                        new_event(
                            RUN_2,
                            testkit::clock::ManualClock::at_epoch().now(),
                            json!({ "type": "cancel_requested", "reason": "stop" }),
                        ),
                        Some(&api_command()),
                    )
                    .await
                    .unwrap();
                // Long enough for the scheduler to see the cancel.
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(())
            }) as HookFuture
        })
    };
    let world = World::new(Setup::new(succeeding()).create_plate(hooked)).await;
    events.fill(world.events.clone());
    let scheduler = scheduler(&world, "worker-a");
    scheduler.submit(&job2(), None).await.unwrap();
    assert!(matches!(
        scheduler.tick(&CancellationToken::new()).await.unwrap(),
        Tick::Ran {
            ended: Ended::Left(Cancelled),
            ..
        }
    ));
    assert_rests_on_its_record(&world).await;
    assert_eq!(
        world.head().await,
        1,
        "the cancelled run committed nothing past the initial version"
    );
}
