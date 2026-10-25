use std::future::Future;
use std::sync::Arc;

use contracts::ids::{Identifier, SdrVersion};
use contracts::types::{
    Actor, AgentSpec, Budget, EventPayload, JobStatus, Requirement, RunRequest,
};
use contracts::{ErrorCode, EventLog, JobStore, RunSubmitter, SdrRepository};
use serde_json::json;

use super::{assert_code, ctx};
use crate::builders::{id, parse};

/// What the run-submitter suite tests, and where it looks for the effects.
/// `jobs`, `events` and `sdr` are the stores the submitter writes to.
pub struct SubmitterWorld {
    pub submitter: Arc<dyn RunSubmitter>,
    pub jobs: Arc<dyn JobStore>,
    pub events: Arc<dyn EventLog>,
    pub sdr: Arc<dyn SdrRepository>,
    /// An agent the submitter knows, as its spec says.
    pub agent: AgentSpec,
}

fn requirement(element_id: &str, limit_g: f64) -> Requirement {
    parse(json!({
        "id": element_id,
        "text": "The plate mass shall not exceed the limit.",
        "priority": "must",
        "numeric": {
            "parameter_id": "part.mass",
            "criterion": { "comparator": "le", "limit": { "value": limit_g, "unit": "g" } }
        },
        "evidence_ids": []
    }))
}

fn request(agent: &AgentSpec) -> RunRequest {
    parse(json!({
        "schema_version": "0.1",
        "agent_id": agent.id,
        "name": "Size the plate",
        "description": "Choose plate dimensions that meet the requirements.",
        "goal": "A committed design whose requirements all have passing checks.",
        "acceptance_criteria": ["Every requirement has a passing check."],
        "requirements": [requirement("R-mass", 150.0), requirement("R-mass-2", 200.0)]
    }))
}

fn actor() -> Actor {
    parse(json!({ "kind": "user", "user_id": "dev" }))
}

/// The `RunSubmitter` contract. `make` builds a fresh world for each case.
pub async fn run_submitter<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = SubmitterWorld>,
{
    let key: Identifier = id("key-1");

    // A submit makes a queued job of the agent, records it, and stores the
    // requirements as version 0.
    let w = make().await;
    let asked = request(&w.agent);
    let submitted = w
        .submitter
        .submit(&key, &asked, &actor(), &ctx())
        .await
        .unwrap_or_else(|e| panic!("run submitter: a valid submit failed: {e}"));
    assert!(
        submitted.created,
        "run submitter: a first submit is created"
    );
    let job = &submitted.job;
    assert_eq!(
        job.status,
        JobStatus::Queued { checkpoint: None },
        "run submitter: a submitted job is queued without a checkpoint"
    );
    assert_eq!(
        job.idempotency_key, key,
        "run submitter: the job keeps the key"
    );
    assert_eq!(
        job.task.agent_id, w.agent.id,
        "run submitter: the task's agent"
    );
    assert_eq!(
        job.task.project_id, job.project_id,
        "run submitter: one project"
    );
    let ids: Vec<&str> = job
        .task
        .requirement_ids
        .iter()
        .map(|r| r.as_str())
        .collect();
    assert_eq!(
        ids,
        ["R-mass", "R-mass-2"],
        "run submitter: the task's requirements"
    );
    assert_eq!(
        job.task.budget, w.agent.limits,
        "run submitter: the default budget"
    );
    assert_eq!(
        job.task.sdr.base_version,
        SdrVersion(0),
        "run submitter: base version"
    );
    let stored = w
        .jobs
        .find_by_run(&job.run_id)
        .await
        .expect("read the job")
        .expect("run submitter: the job is stored");
    assert_eq!(
        &stored, job,
        "run submitter: the returned job is the stored one"
    );
    let log = w
        .events
        .read(&job.run_id, 0, 10)
        .await
        .expect("read the log");
    assert!(
        matches!(
            log.as_slice(),
            [event] if matches!(
                &event.payload,
                EventPayload::JobStateChanged { from: None, to: JobStatus::Queued { checkpoint: None }, .. }
            )
        ),
        "run submitter: the log holds only the submit's record: {log:?}"
    );
    assert_eq!(
        w.sdr.head(&job.project_id).await.expect("head"),
        Some(SdrVersion(0)),
        "run submitter: the project is at version 0"
    );
    let v0 = w
        .sdr
        .document(&job.project_id, &SdrVersion(0))
        .await
        .expect("version 0");
    assert_eq!(
        v0.requirements, asked.requirements,
        "run submitter: version 0"
    );
    assert_eq!(
        v0.parent_version, None,
        "run submitter: version 0 has no parent"
    );

    // The same key and request again is the same run, recorded once.
    let again = w
        .submitter
        .submit(&key, &asked, &actor(), &ctx())
        .await
        .unwrap_or_else(|e| panic!("run submitter: a repeated submit failed: {e}"));
    assert!(!again.created, "run submitter: a repeat is not created");
    assert_eq!(
        again.job.id, job.id,
        "run submitter: a repeat returns the run"
    );
    assert_eq!(
        again.job.task, job.task,
        "run submitter: a repeat keeps the task"
    );
    assert_eq!(
        w.events.last_seq(&job.run_id).await.expect("last seq"),
        1,
        "run submitter: a repeat records nothing"
    );

    // The same key for something else is refused, whatever differs.
    let mut other_goal = asked.clone();
    other_goal.goal = parse(json!("Another goal."));
    assert_code(
        w.submitter
            .submit(&key, &other_goal, &actor(), &ctx())
            .await,
        ErrorCode::Conflict,
        "run submitter: a reused key with another goal",
    );
    let mut other_requirements = asked.clone();
    other_requirements.requirements = vec![requirement("R-mass", 120.0)];
    assert_code(
        w.submitter
            .submit(&key, &other_requirements, &actor(), &ctx())
            .await,
        ErrorCode::Conflict,
        "run submitter: a reused key with other requirements",
    );

    // Another key is another run, on another project.
    let other = w
        .submitter
        .submit(&id("key-2"), &asked, &actor(), &ctx())
        .await
        .unwrap_or_else(|e| panic!("run submitter: a second run failed: {e}"));
    assert!(other.created, "run submitter: another key is created");
    assert_ne!(
        other.job.run_id, job.run_id,
        "run submitter: another run id"
    );
    assert_ne!(
        other.job.project_id, job.project_id,
        "run submitter: another project"
    );

    // What the agent does not allow is refused before anything is stored.
    let w = make().await;
    let mut unknown = request(&w.agent);
    unknown.agent_id = id("no_such_agent");
    assert_code(
        w.submitter.submit(&key, &unknown, &actor(), &ctx()).await,
        ErrorCode::InvalidInput,
        "run submitter: an unknown agent",
    );
    let mut over: RunRequest = request(&w.agent);
    let mut budget: Budget = w.agent.limits.clone();
    budget.max_steps = budget.max_steps.saturating_add(1);
    over.budget = Some(budget);
    assert_code(
        w.submitter.submit(&key, &over, &actor(), &ctx()).await,
        ErrorCode::InvalidInput,
        "run submitter: a budget over the agent's limits",
    );
    let mut cited = request(&w.agent);
    cited.requirements[0].evidence_ids = vec![id("ev-1")];
    assert_code(
        w.submitter.submit(&key, &cited, &actor(), &ctx()).await,
        ErrorCode::InvalidInput,
        "run submitter: a requirement that cites evidence",
    );
    let fine = w
        .submitter
        .submit(&key, &request(&w.agent), &actor(), &ctx())
        .await
        .unwrap_or_else(|e| panic!("run submitter: a refused request left something behind: {e}"));
    assert!(fine.created, "run submitter: nothing was stored before");
}
