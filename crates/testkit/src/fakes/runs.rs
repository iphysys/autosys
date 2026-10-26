use std::sync::Arc;

use async_trait::async_trait;
use contracts::ids::{Identifier, ProjectId, RunId, SdrVersion};
use contracts::types::{
    Actor, AgentSpec, Event, EventPayload, Job, JobState, JobStatus, NonEmptyString, RunReport,
    RunRequest, SdrDocument, SdrVersionView, Timestamp,
};
use contracts::{
    Clock, Error, ErrorCode, EventLog, ExecutionContext, JobStore, NewEvent, RunReports,
    RunSubmitter, SdrRepository, SdrViews, Submitted, job_state,
};
use serde_json::json;
use sha2::Digest;

use crate::builders::{parse, version_record};
use crate::leases::{job_submit, project_create};

/// A [`RunSubmitter`] on the given stores, for tests of what calls it. It
/// keeps the contract the runtime's submitter keeps, with a simpler task:
/// the agent's tools, its limits unless the request narrows them, and
/// version 0 written straight to `sdr`.
pub struct FakeSubmitter {
    jobs: Arc<dyn JobStore>,
    events: Arc<dyn EventLog>,
    sdr: Arc<dyn SdrRepository>,
    clock: Arc<dyn Clock>,
    agents: Vec<AgentSpec>,
}

impl FakeSubmitter {
    pub fn new(
        jobs: Arc<dyn JobStore>,
        events: Arc<dyn EventLog>,
        sdr: Arc<dyn SdrRepository>,
        clock: Arc<dyn Clock>,
        agents: Vec<AgentSpec>,
    ) -> Self {
        Self {
            jobs,
            events,
            sdr,
            clock,
            agents,
        }
    }

    fn job(&self, key: &Identifier, request: &RunRequest) -> Result<Job, Error> {
        let agent = self
            .agents
            .iter()
            .find(|a| a.id == request.agent_id)
            .ok_or_else(|| {
                invalid(format!(
                    "agent {} is not registered",
                    request.agent_id.as_str()
                ))
            })?;
        let budget = request
            .budget
            .clone()
            .unwrap_or_else(|| agent.limits.clone());
        let limits = &agent.limits;
        let fits = budget.max_tokens <= limits.max_tokens
            && budget.max_tool_calls <= limits.max_tool_calls
            && budget.max_steps <= limits.max_steps
            && budget.max_wall_time_seconds <= limits.max_wall_time_seconds
            && budget.max_repair_attempts <= limits.max_repair_attempts;
        if !fits {
            return Err(invalid("the budget exceeds the agent's limits".to_owned()));
        }
        let h = digest(key);
        let now = Timestamp(self.clock.now());
        Ok(parse(json!({
            "schema_version": "0.2",
            "id": format!("job-{h}"),
            "run_id": format!("run-{h}"),
            "project_id": format!("proj-{h}"),
            "status": { "state": "queued" },
            "task": {
                "schema_version": "0.1",
                "id": format!("task-{h}"),
                "project_id": format!("proj-{h}"),
                "agent_id": request.agent_id,
                "name": request.name,
                "description": request.description,
                "goal": request.goal,
                "acceptance_criteria": request.acceptance_criteria,
                "requirement_ids": request.requirements.iter().map(|r| &r.id).collect::<Vec<_>>(),
                "skills": agent.skills,
                "tools": agent.tools,
                "sdr": {
                    "base_version": 0,
                    "read_slice": { "kinds": ["requirement", "parameter", "decision", "assumption"], "ids": [] },
                    "write_scope": { "kinds": ["parameter", "decision", "assumption"], "ids": [] }
                },
                "budget": budget,
                "result_schema": agent.result_schema
            },
            "idempotency_key": key,
            "attempt": 0,
            "failed_attempts": 0,
            "attempt_history": [],
            "created_at": now,
            "updated_at": now,
            "sdr_base_version": 0,
            "progress": { "steps_completed": 0, "last_event_seq": 0 },
            "pending_work": [],
            "artifact_ids": [],
            "budget_used": {
                "input_tokens": 0, "output_tokens": 0, "tool_calls": 0, "steps": 0,
                "wall_time_seconds": 0
            }
        })))
    }

    /// Writes version 0, or checks the one there holds the same
    /// requirements.
    async fn version_zero(&self, job: &Job, request: &RunRequest) -> Result<(), Error> {
        if let Some(cited) = request
            .requirements
            .iter()
            .find(|r| !r.evidence_ids.is_empty())
        {
            return Err(invalid(format!(
                "requirement {} cites evidence; version 0 cites none",
                cited.id.as_str()
            )));
        }
        let project = &job.project_id;
        if self.sdr.head(project).await?.is_some() {
            let stored = self.sdr.document(project, &SdrVersion(0)).await?;
            return if stored.requirements == request.requirements {
                Ok(())
            } else {
                Err(reused())
            };
        }
        let mut document: SdrDocument = parse(json!({
            "schema_version": "0.1",
            "project_id": project,
            "version": 0,
            "requirements": [],
            "design": { "parameters": [], "decisions": [], "assumptions": [] }
        }));
        document.requirements = request.requirements.clone();
        let record = version_record(project.as_str(), 0, None, self.clock.now());
        self.sdr
            .append_version(&document, &record, Some(&project_create()))
            .await
    }
}

#[async_trait]
impl RunSubmitter for FakeSubmitter {
    async fn submit(
        &self,
        key: &Identifier,
        request: &RunRequest,
        _actor: &Actor,
        ctx: &ExecutionContext,
    ) -> Result<Submitted, Error> {
        ctx.ensure_not_cancelled()?;
        let job = self.job(key, request)?;
        self.version_zero(&job, request).await?;
        match self.jobs.insert(&job, Some(&job_submit())).await {
            Ok(()) => {}
            Err(e) if e.code() == ErrorCode::Conflict => {
                let stored = self.jobs.get(&job.id).await?;
                return if stored.task == job.task {
                    Ok(Submitted {
                        job: stored,
                        created: false,
                    })
                } else {
                    Err(reused())
                };
            }
            Err(e) => return Err(e),
        }
        let submitted: Event = self
            .events
            .append(
                NewEvent {
                    run_id: job.run_id.clone(),
                    occurred_at: Timestamp(self.clock.now()),
                    correlation_id: ctx.correlation_id().clone(),
                    actor: parse(json!({ "kind": "system", "component": "scheduler" })),
                    payload: EventPayload::JobStateChanged {
                        from: None,
                        to: JobStatus::Queued { checkpoint: None },
                        reason: "submitted".parse::<NonEmptyString>().ok(),
                        harness_build: None,
                    },
                },
                Some(&job_submit()),
            )
            .await?;
        debug_assert_eq!(submitted.run_id, job.run_id);
        Ok(Submitted { job, created: true })
    }
}

/// A [`RunReports`] over a job store that serves `report` for every
/// stopped run, under that run's id.
pub struct FakeReports {
    jobs: Arc<dyn JobStore>,
    report: RunReport,
}

impl FakeReports {
    pub fn new(jobs: Arc<dyn JobStore>, report: RunReport) -> Self {
        Self { jobs, report }
    }
}

#[async_trait]
impl RunReports for FakeReports {
    async fn report(&self, run_id: &RunId, ctx: &ExecutionContext) -> Result<RunReport, Error> {
        ctx.ensure_not_cancelled()?;
        let job = self.jobs.find_by_run(run_id).await?.ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!("run {} does not exist", run_id.as_str()),
            )
        })?;
        match job_state(&job.status) {
            JobState::Queued | JobState::Running | JobState::Waiting => Err(Error::new(
                ErrorCode::Conflict,
                format!("run {} has not stopped", run_id.as_str()),
            )),
            state => {
                let mut report = self.report.clone();
                report.run_id = run_id.clone();
                report.project_id = job.project_id;
                report.final_ = state != JobState::Suspended;
                Ok(report)
            }
        }
    }
}

/// A [`SdrViews`] that serves the views it was given, by project and
/// version.
#[derive(Default)]
pub struct FakeSdrViews {
    views: Vec<SdrVersionView>,
}

impl FakeSdrViews {
    pub fn new(views: Vec<SdrVersionView>) -> Self {
        Self { views }
    }
}

#[async_trait]
impl SdrViews for FakeSdrViews {
    async fn version(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
        ctx: &ExecutionContext,
    ) -> Result<SdrVersionView, Error> {
        ctx.ensure_not_cancelled()?;
        self.views
            .iter()
            .find(|v| &v.document.project_id == project_id && &v.document.version == version)
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::NotFound,
                    format!(
                        "version {} of project {} does not exist",
                        version.0,
                        project_id.as_str()
                    ),
                )
            })
    }
}

fn digest(key: &Identifier) -> String {
    sha2::Sha256::digest(key.as_str().as_bytes())
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn invalid(message: String) -> Error {
    Error::new(ErrorCode::InvalidInput, message)
}

fn reused() -> Error {
    Error::new(
        ErrorCode::Conflict,
        "the idempotency key was used for a different request",
    )
}
