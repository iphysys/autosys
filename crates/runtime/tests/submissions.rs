//! `Submissions` keeps the run-submitter contract (ADR-0018): the committed
//! orchestrator, the repository's prompts and the overlay's tools, on
//! testkit's in-memory stores.

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use std::sync::Arc;

use contracts::ids::{Identifier, SdrVersion};
use contracts::types::{ElementKind, RunRequest, SdrSlice};
use contracts::{ErrorCode, ExecutionContext, JobStore, RunSubmitter};
use runtime::{AgentRegistry, Submissions};
use sdr::SdrManager;
use serde_json::json;
use support::*;
use testkit::builders::{id, parse};
use testkit::clock::ManualClock;
use testkit::contract::{self, SubmitterWorld};
use testkit::stores::{InMemoryEventLog, InMemoryJobStore, sdr_storage};

struct Parts {
    submissions: Arc<Submissions>,
    jobs: Arc<InMemoryJobStore>,
    sdr: Arc<SdrManager>,
    /// The same stores, as the suite reads them.
    world: SubmitterWorld,
}

fn parts() -> Parts {
    let clock = Arc::new(ManualClock::at_epoch());
    let jobs = Arc::new(InMemoryJobStore::new(clock.clone()));
    let events = Arc::new(InMemoryEventLog::fenced_by(jobs.leases()));
    // The suite reads version 0 straight from storage, through testkit.
    let storage = sdr_storage();
    let sdr = Arc::new(SdrManager::new(storage.clone(), clock.clone()).expect("SDR schemas"));
    let tools = overlay_tools();
    let names: Vec<&str> = tools.iter().map(String::as_str).collect();
    let agents = Arc::new(
        AgentRegistry::new(vec![orchestrator_spec()], &prompts(), &names)
            .expect("the committed agent registers"),
    );
    let submissions = Arc::new(
        Submissions::new(
            jobs.clone(),
            events.clone(),
            sdr.clone(),
            agents,
            clock,
            parse(json!("test-build")),
        )
        .expect("the run request schema compiles"),
    );
    Parts {
        world: SubmitterWorld {
            submitter: submissions.clone(),
            jobs: jobs.clone(),
            events: events.clone(),
            sdr: storage,
            agent: orchestrator_spec(),
        },
        submissions,
        jobs,
        sdr,
    }
}

#[tokio::test]
async fn submissions_keep_the_run_submitter_contract() {
    contract::run_submitter(|| async { parts().world }).await;
}

/// R-mass with a limit of `limit_g`.
fn requirement(limit_g: f64) -> serde_json::Value {
    mass_requirement("R-mass", limit_g)["requirement"].clone()
}

fn request() -> RunRequest {
    parse(json!({
        "schema_version": "0.1",
        "agent_id": "main_orchestrator",
        "name": "Size the plate",
        "description": "Choose plate dimensions that meet the requirements.",
        "goal": "A committed design whose requirements all have passing checks.",
        "acceptance_criteria": ["Every requirement has a passing check."],
        "requirements": [requirement(150.0)]
    }))
}

fn ctx() -> ExecutionContext {
    ExecutionContext::new(id("corr-submit"))
}

/// The task a brief becomes: the agent's tools and result schema, every
/// kind to read, and everything but requirements to write. Version 0
/// records who asked, under the build that took the request.
#[tokio::test]
async fn a_brief_becomes_the_agents_task() {
    let p = parts();
    let key: Identifier = id("key-1");
    let actor = parse(json!({ "kind": "user", "user_id": "dev" }));
    let submitted = p
        .submissions
        .submit(&key, &request(), &actor, &ctx())
        .await
        .expect("submit");
    let task = &submitted.job.task;
    let spec = orchestrator_spec();
    assert_eq!(task.tools, spec.tools);
    assert_eq!(task.result_schema, spec.result_schema);
    assert_eq!(task.budget, spec.limits);
    assert_eq!(task.sdr.read_slice.kinds.len(), 4);
    assert!(
        !task
            .sdr
            .write_scope
            .kinds
            .contains(&ElementKind::Requirement),
        "{:?}",
        task.sdr.write_scope
    );
    let run = submitted.job.run_id.as_str();
    assert!(run.starts_with("run-") && run.len() == 36, "{run}");

    let record = p
        .sdr
        .version_record(&submitted.job.project_id, &SdrVersion(0), &ctx())
        .await
        .expect("version 0's record");
    assert_eq!(record.actor, actor);
    assert_eq!(record.harness.harness_build.as_str(), "test-build");
    assert_eq!(record.harness.agent_specs.len(), 1);
    let slice = SdrSlice {
        kinds: vec![ElementKind::Requirement],
        ids: Vec::new(),
    };
    let v0 = p
        .sdr
        .read(&submitted.job.project_id, &slice, &SdrVersion(0), &ctx())
        .await
        .expect("version 0");
    assert_eq!(v0.requirements, request().requirements);
}

/// A submit that stopped after creating the project is finished by the
/// next with the same key; the job is inserted and recorded once.
#[tokio::test]
async fn a_submit_that_stopped_after_version_0_is_finished_by_a_repeat() {
    let p = parts();
    let key: Identifier = id("key-1");
    let actor = parse(json!({ "kind": "user", "user_id": "dev" }));
    let first = p
        .submissions
        .submit(&key, &request(), &actor, &ctx())
        .await
        .expect("submit");
    // New job and event stores beside the same SDR: as if the job never
    // landed after version 0 was written.
    let fresh = parts();
    let retried = Submissions::new(
        fresh.jobs.clone(),
        fresh.world.events.clone(),
        p.sdr.clone(),
        Arc::new(
            AgentRegistry::new(
                vec![orchestrator_spec()],
                &prompts(),
                &overlay_tools()
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            )
            .expect("agents"),
        ),
        Arc::new(ManualClock::at_epoch()),
        parse(json!("test-build")),
    )
    .expect("submissions");
    let finished = retried
        .submit(&key, &request(), &actor, &ctx())
        .await
        .expect("the repeat finishes the submit");
    assert!(finished.created, "the job had not landed");
    assert_eq!(finished.job.run_id, first.job.run_id);
    let jobs: &dyn JobStore = fresh.jobs.as_ref();
    assert!(jobs.get(&first.job.id).await.is_ok());

    let mut other = request();
    other.requirements = vec![parse(requirement(100.0))];
    let error = retried
        .submit(&key, &other, &actor, &ctx())
        .await
        .expect_err("other requirements under the same key");
    assert_eq!(error.code(), ErrorCode::Conflict, "{error}");
}
