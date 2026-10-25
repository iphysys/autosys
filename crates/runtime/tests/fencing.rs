//! A worker whose job was reclaimed cannot append an event, record a step
//! or commit (ADR-0032): PR 12a's second done-when test. In memory, with the
//! real components; `postgres_fencing` in infra proves the same refusals on
//! Postgres at store level.
//!
//! Worker 1 claims the job with a short lease, the clock passes it, and
//! worker 2 claims it again. Worker 1's context still carries its fence.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "integration tests")]

mod support;

use std::sync::Arc;
use std::time::Duration;

use contracts::ids::{AgentId, ProjectId, SdrVersion};
use contracts::types::{EventPayload, Job, JobStatus, ToolOverlay, Transition};
use contracts::{Claimed, ErrorCode, ExecutionContext, JobStore, StepStore};
use sdr::{CommitInfo, Validation};
use serde_json::json;
use support::*;
use testkit::builders::{id, parse, tool_request};
use testkit::clock::ManualClock;
use testkit::fakes::{HookFuture, Slot};
use testkit::leases::{LONG_LEASE, fenced_ctx};
use testkit::stores::InMemoryJobStore;
use tools::{ToolCall, ToolObservation};

const SHORT: Duration = Duration::from_secs(30);

fn job() -> Job {
    job_for(task_json(&["R-mass"], roomy_budget()))
}

fn agent() -> AgentId {
    id(AGENT)
}

/// Lets worker 1's lease expire and claims the job as worker 2.
async fn reclaim(jobs: &InMemoryJobStore, clock: &ManualClock) -> Claimed {
    clock.advance(SHORT + Duration::from_secs(1));
    jobs.claim(&id("worker-2"), LONG_LEASE)
        .await
        .expect("claim")
        .expect("the expired job is reclaimed")
}

/// Worker 2's root context.
fn reclaimed_ctx(claimed: &Claimed) -> ExecutionContext {
    fenced_ctx(claimed, id("corr-worker-2"))
}

/// The job as the store holds it: still running, now worker 2's. The old
/// worker never saved or failed it.
async fn assert_held_by_worker_2(world: &World) {
    let stored = world
        .leased
        .jobs()
        .get(&id("job-1"))
        .await
        .expect("get the job");
    assert_eq!(stored.status, JobStatus::Running, "the job left running");
    assert_eq!(stored.attempt, 2, "the job's attempt changed");
    assert_eq!(
        stored
            .lease
            .map(|l| l.holder.as_str().to_owned())
            .as_deref(),
        Some("worker-2"),
        "the job's lease"
    );
}

/// create_plate, measure, then a final proposal of the measured mass.
fn succeeding_script() -> Vec<ScriptStep> {
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

#[tokio::test]
async fn a_reclaimed_worker_cannot_call_a_tool() {
    let world = World::new(Setup::new(Vec::new()).lease(SHORT)).await;
    let stale = world.ctx().with_run(id(RUN)).for_step(id("step-1"));
    let logged = world.logged().await.len();
    let reclaimed = reclaim(world.leased.jobs(), &world.clock).await;

    let request = tool_request("create_plate", "step-1", plate());
    let agent = agent();
    let call = || ToolCall {
        request: &request,
        agent_id: &agent,
        sdr_version: SdrVersion(1),
        approval: None,
    };
    let error = world
        .tools
        .call(call(), &stale)
        .await
        .expect_err("a stale fence cannot call a tool");
    assert_eq!(error.code(), ErrorCode::LeaseLost, "{error}");
    assert_eq!(world.create_plate.calls(), 0, "the tool was reached");
    assert_eq!(
        world
            .steps
            .get(&request.idempotency_key)
            .await
            .map_err(|e| e.code()),
        Err(ErrorCode::NotFound),
        "a step record was written"
    );
    assert_eq!(world.logged().await.len(), logged, "an event was appended");

    let current = reclaimed_ctx(&reclaimed)
        .with_run(id(RUN))
        .for_step(id("step-1"));
    let observation = world.tools.call(call(), &current).await.unwrap();
    assert!(
        matches!(observation, ToolObservation::Succeeded { .. }),
        "{observation:?}"
    );
    assert_eq!(world.create_plate.calls(), 1);
}

#[tokio::test]
async fn a_reclaimed_worker_cannot_commit() {
    let world = World::new(Setup::new(Vec::new()).lease(SHORT)).await;
    let stale = world.ctx();
    let reclaimed = reclaim(world.leased.jobs(), &world.clock).await;

    let project: ProjectId = id(PROJECT);
    let transition = || -> Transition {
        parse(json!({
            "schema_version": "0.1",
            "id": "tr-late",
            "base_version": 1,
            "actor": { "kind": "agent", "agent_id": AGENT },
            "reason": "A decision.",
            "final": false,
            "operations": [{
                "op": "add_decision",
                "decision": {
                    "id": "dec-1",
                    "choice": "A thinner plate.",
                    "rationale": "Lighter.",
                    "related_ids": ["R-mass"]
                }
            }]
        }))
    };
    let info = || CommitInfo {
        evidence_ids: Vec::new(),
        harness: harness(),
    };
    let validate = |ctx: ExecutionContext| {
        let proposed = world.sdr.propose(project.clone(), transition());
        let sdr = Arc::clone(&world.sdr);
        async move {
            match sdr.validate(proposed, &ctx).await.expect("validation runs") {
                Validation::Valid(validated) => validated,
                Validation::Invalid(invalid) => panic!("invalid: {invalid:?}"),
            }
        }
    };

    let validated = validate(stale.clone()).await;
    let error = world
        .sdr
        .commit(validated, info(), &stale)
        .await
        .expect_err("a stale fence cannot commit");
    assert_eq!(error.code(), ErrorCode::LeaseLost, "{error}");
    assert_eq!(world.head().await, 1, "the head moved");

    let current = reclaimed_ctx(&reclaimed);
    let validated = validate(current.clone()).await;
    let record = world
        .sdr
        .commit(validated, info(), &current)
        .await
        .expect("the new holder commits");
    assert_eq!(record.version, SdrVersion(2));
}

#[tokio::test]
async fn a_reclaimed_worker_cannot_start_a_run() {
    let world = World::new(Setup::new(succeeding_script()).lease(SHORT)).await;
    let stale = world.ctx();
    let logged = world.logged().await.len();
    let reclaimed = reclaim(world.leased.jobs(), &world.clock).await;

    let ran = world.run_with(job(), stale).await;
    let error = ran.outcome.as_ref().expect_err("the run ends with Err");
    assert_eq!(error.code(), ErrorCode::LeaseLost, "{error}");
    assert!(!error.retryable());
    assert!(
        ran.events.is_empty(),
        "the stream yielded {:?}",
        ran.kinds()
    );
    assert_eq!(world.logged().await.len(), logged, "an event was appended");
    assert!(world.model.requests().is_empty(), "the model was called");
    assert_held_by_worker_2(&world).await;

    // The new holder runs the same job.
    let ran = world.run_with(job(), reclaimed_ctx(&reclaimed)).await;
    assert_eq!(
        ran.outcome().stop_reason,
        contracts::types::StopReason::Success
    );
}

/// The lease is lost while create_plate runs. The run ends with lease_lost
/// at the gateway's next write: nothing more is recorded, nothing is
/// repaired, and the job is left to its new holder, who resumes the call
/// with the same idempotency key. create_plate is not idempotent here, so
/// it is not called again.
#[tokio::test]
async fn a_lease_lost_mid_call_ends_the_run_without_a_trace() {
    let world_slot: Slot<(Arc<InMemoryJobStore>, Arc<ManualClock>)> = Slot::new("world");
    let reclaimed_slot: Slot<(Claimed, contracts::ToolRequest)> = Slot::new("reclaim");
    let hooked = {
        let world_slot = world_slot.clone();
        let reclaimed_slot = reclaimed_slot.clone();
        returning("create_plate", plate_result()).before_answer(move |index, request| {
            let world_slot = world_slot.clone();
            let reclaimed_slot = reclaimed_slot.clone();
            let request = request.clone();
            Box::pin(async move {
                if index == 0 {
                    let (jobs, clock) = world_slot.get();
                    let claimed = reclaim(jobs, clock).await;
                    reclaimed_slot.fill((claimed, request));
                }
                Ok(())
            }) as HookFuture
        })
    };
    let mut overlay: ToolOverlay = build123d_overlay();
    for tool in &mut overlay.tools {
        if tool.name.as_str() == "create_plate" {
            tool.idempotent = false;
        }
    }
    let world = World::new(
        Setup::new(vec![
            call_tools(vec![("c1", "create_plate", plate())]),
            reply("Never asked."),
        ])
        .lease(SHORT)
        .overlay(overlay)
        .create_plate(hooked),
    )
    .await;
    world_slot.fill((Arc::clone(world.leased.jobs()), Arc::clone(&world.clock)));

    let ran = world.run(job()).await;
    let error = ran.outcome.as_ref().expect_err("the run ends with Err");
    assert_eq!(error.code(), ErrorCode::LeaseLost, "{error}");
    assert_eq!(world.create_plate.calls(), 1);
    assert_eq!(world.model.requests().len(), 1, "a repair called the model");

    let (reclaimed, request) = reclaimed_slot.get().clone();
    let record = world
        .steps
        .get(&request.idempotency_key)
        .await
        .expect("the step was begun");
    assert_eq!(record.outcome, None, "worker 1 completed the step");
    let logged = world.logged().await;
    let kinds: Vec<&str> = logged.iter().map(|e| kind(&e.payload)).collect();
    for absent in ["tool_call_completed", "error", "run_stopped"] {
        assert!(!kinds.contains(&absent), "{absent} was appended: {kinds:?}");
    }
    assert_eq!(
        kinds.last(),
        Some(&"tool_call_started"),
        "worker 1 wrote after the reclaim: {kinds:?}"
    );
    assert_held_by_worker_2(&world).await;

    // Worker 2 resumes the call with the same idempotency key. The step's
    // outcome is unknown and create_plate is not idempotent, so it fails as
    // an outcome_unknown observation instead of calling the tool again
    // (ADR-0026).
    let agent = agent();
    let current = reclaimed_ctx(&reclaimed)
        .with_run(id(RUN))
        .for_step(request.step_id.clone());
    let observation = world
        .tools
        .call(
            ToolCall {
                request: &request,
                agent_id: &agent,
                sdr_version: SdrVersion(1),
                approval: None,
            },
            &current,
        )
        .await
        .expect("worker 2's call");
    match observation {
        ToolObservation::Failed { error, replayed } => {
            assert_eq!(error.code.as_str(), "outcome_unknown", "{error:?}");
            assert!(!replayed);
        }
        other => panic!("expected a failed observation, got {other:?}"),
    }
    assert_eq!(world.create_plate.calls(), 1, "the tool was called again");
    let record = world
        .steps
        .get(&request.idempotency_key)
        .await
        .expect("get");
    assert!(
        record.outcome.is_some(),
        "worker 2 did not complete the step"
    );
    let completed = world
        .logged()
        .await
        .into_iter()
        .filter(|e| matches!(e.payload, EventPayload::ToolCallCompleted { .. }))
        .count();
    assert_eq!(completed, 1, "worker 2 records the completion");
}
