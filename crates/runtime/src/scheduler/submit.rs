//! Submitting runs (ADR-0018, ADR-0031): what a client's brief becomes, and
//! the one path by which a top-level job enters the job store.
//!
//! A submit acts, then records: version 0, the insert, then the
//! `job_state_changed` into queued. Each step is idempotent, so a submit
//! that stopped partway is finished by the next with the same key. Only
//! the submit whose insert succeeds records; a submit that finds the job
//! inserted leaves a missing record to the claim, which adds it.

use std::sync::Arc;

use async_trait::async_trait;
use contracts::ids::{Identifier, JobId, ProjectId, RunId, SdrVersion, TaskId, TransitionId};
use contracts::types::{
    Actor, ElementKind, HarnessVersions, Job, JobProgress, JobSchemaVersion, JobStatus,
    NonEmptyString, RunRequest, SdrSlice, Task, TaskSchemaVersion, TaskSdrAccess, Timestamp, Usage,
};
use contracts::{
    Clock, Error, ErrorCode, EventLog, ExecutionContext, JobStore, RunSubmitter, SchemaSet,
    Submitted, UnfencedReason, WriteAuthority,
};
use sdr::{InitialVersion, SdrManager};
use sha2::Digest;

use super::{correlation, transition};
use crate::agents::AgentRegistry;
use crate::error::RuntimeError;

const RUN_REQUEST: &str = "api/run-request.schema.json";

/// Inserts `job` and records it queued, with the submit's own authority.
/// Conflict if the id exists.
pub(crate) async fn insert_and_record(
    jobs: &dyn JobStore,
    events: &dyn EventLog,
    clock: &dyn Clock,
    job: &Job,
) -> Result<(), Error> {
    let submit = WriteAuthority::Unfenced(UnfencedReason::JobSubmit);
    jobs.insert(job, Some(&submit)).await?;
    transition::record(
        events,
        clock,
        &job.run_id,
        &correlation(&job.id, 0)?,
        transition::changed(
            None,
            JobStatus::Queued { checkpoint: None },
            transition::SUBMITTED,
        ),
        &submit,
    )
    .await?;
    Ok(())
}

/// Creates `job`'s project at version 0 from `initial`, with the reason
/// that creates projects.
pub(crate) async fn create_project(
    sdr: &SdrManager,
    job: &Job,
    initial: InitialVersion,
) -> Result<(), Error> {
    let ctx = ExecutionContext::builder(correlation(&job.id, 0)?)
        .project(job.project_id.clone())
        .authorized_by(WriteAuthority::Unfenced(UnfencedReason::ProjectCreate))
        .build();
    sdr.create_project(&job.project_id, initial, &ctx)
        .await
        .map(|_| ())
}

/// The ids of the run a key starts: one digest of the key, under each
/// kind's prefix.
struct RunIds {
    run: RunId,
    job: JobId,
    project: ProjectId,
    task: TaskId,
    transition: TransitionId,
}

impl RunIds {
    fn from_key(key: &Identifier) -> Result<Self, Error> {
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"autosys run v0\0");
        hasher.update(key.as_str().as_bytes());
        let h: String = hasher
            .finalize()
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect();
        let invalid = |_| Error::new(ErrorCode::Internal, "a derived run id is invalid");
        Ok(Self {
            run: format!("run-{h}").parse().map_err(invalid)?,
            job: format!("job-{h}").parse().map_err(invalid)?,
            project: format!("proj-{h}").parse().map_err(invalid)?,
            task: format!("task-{h}").parse().map_err(invalid)?,
            transition: format!("tr-{h}-requirements").parse().map_err(invalid)?,
        })
    }
}

/// Starts the runs clients ask for. See the module docs.
pub struct Submissions {
    jobs: Arc<dyn JobStore>,
    events: Arc<dyn EventLog>,
    sdr: Arc<SdrManager>,
    agents: Arc<AgentRegistry>,
    clock: Arc<dyn Clock>,
    /// The build recorded with version 0.
    harness_build: NonEmptyString,
    schemas: SchemaSet,
}

impl Submissions {
    pub fn new(
        jobs: Arc<dyn JobStore>,
        events: Arc<dyn EventLog>,
        sdr: Arc<SdrManager>,
        agents: Arc<AgentRegistry>,
        clock: Arc<dyn Clock>,
        harness_build: NonEmptyString,
    ) -> Result<Self, RuntimeError> {
        let schemas = SchemaSet::compile(&[RUN_REQUEST]).map_err(|e| {
            RuntimeError::Internal(format!("the run request schema does not compile: {e}"))
        })?;
        Ok(Self {
            jobs,
            events,
            sdr,
            agents,
            clock,
            harness_build,
            schemas,
        })
    }

    /// The queued job `request` asks for, under `ids`, once its task has
    /// passed every check of its agent.
    fn job(
        &self,
        key: &Identifier,
        ids: &RunIds,
        request: &RunRequest,
    ) -> Result<(Job, HarnessVersions), Error> {
        let problems = self.schemas.errors_of(RUN_REQUEST, request)?;
        if !problems.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                format!("the run request is invalid: {}", problems.join("; ")),
            ));
        }
        let spec = self
            .agents
            .get(request.agent_id.as_str())
            .map_err(invalid_request)?
            .spec();
        let task = Task {
            schema_version: TaskSchemaVersion::X01,
            id: ids.task.clone(),
            project_id: ids.project.clone(),
            agent_id: request.agent_id.clone(),
            name: request.name.clone(),
            description: request.description.clone(),
            goal: request.goal.clone(),
            acceptance_criteria: request.acceptance_criteria.clone(),
            requirement_ids: request.requirements.iter().map(|r| r.id.clone()).collect(),
            skills: spec.skills.clone(),
            tools: spec.tools.clone(),
            sdr: TaskSdrAccess {
                base_version: SdrVersion(0),
                read_slice: SdrSlice {
                    kinds: vec![
                        ElementKind::Requirement,
                        ElementKind::Parameter,
                        ElementKind::Decision,
                        ElementKind::Assumption,
                    ],
                    ids: Vec::new(),
                },
                // The requirements are the client's: the agent designs.
                write_scope: SdrSlice {
                    kinds: vec![
                        ElementKind::Parameter,
                        ElementKind::Decision,
                        ElementKind::Assumption,
                    ],
                    ids: Vec::new(),
                },
            },
            budget: request
                .budget
                .clone()
                .unwrap_or_else(|| spec.limits.clone()),
            result_schema: spec.result_schema.clone(),
        };
        let agent = self.agents.agent_for(&task).map_err(invalid_request)?;
        let harness = HarnessVersions {
            harness_build: self.harness_build.clone(),
            model_ids: Vec::new(),
            prompts: Vec::new(),
            skills: Vec::new(),
            agent_specs: vec![agent.provenance().clone()],
        };
        let now = Timestamp(self.clock.now());
        let job = Job {
            schema_version: JobSchemaVersion::X02,
            id: ids.job.clone(),
            run_id: ids.run.clone(),
            project_id: ids.project.clone(),
            status: JobStatus::Queued { checkpoint: None },
            task,
            idempotency_key: key.clone(),
            attempt: 0,
            failed_attempts: 0,
            attempt_history: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            sdr_base_version: SdrVersion(0),
            progress: JobProgress {
                steps_completed: 0,
                last_event_seq: 0,
            },
            pending_work: Vec::new(),
            artifact_ids: Vec::new(),
            budget_used: Usage {
                input_tokens: 0,
                output_tokens: 0,
                tool_calls: 0,
                steps: 0,
                wall_time_seconds: 0.0,
            },
            lease: None,
            not_before: None,
        };
        Ok((job, harness))
    }

    /// Whether the project's version 0 holds exactly these requirements.
    async fn holds_requirements(&self, job: &Job, request: &RunRequest) -> Result<bool, Error> {
        let ctx = ExecutionContext::builder(correlation(&job.id, 0)?)
            .project(job.project_id.clone())
            .build();
        let all = SdrSlice {
            kinds: vec![ElementKind::Requirement],
            ids: Vec::new(),
        };
        let v0 = self
            .sdr
            .read(&job.project_id, &all, &SdrVersion(0), &ctx)
            .await?;
        Ok(v0.requirements == request.requirements)
    }
}

#[async_trait]
impl RunSubmitter for Submissions {
    async fn submit(
        &self,
        key: &Identifier,
        request: &RunRequest,
        actor: &Actor,
        ctx: &ExecutionContext,
    ) -> Result<Submitted, Error> {
        ctx.ensure_not_cancelled()?;
        let ids = RunIds::from_key(key)?;
        let (job, harness) = self.job(key, &ids, request)?;
        let initial = InitialVersion {
            transition_id: ids.transition.clone(),
            actor: actor.clone(),
            reason: "The run's requirements, as the client gave them."
                .parse()
                .map_err(|_| Error::new(ErrorCode::Internal, "the version 0 reason is empty"))?,
            harness,
            requirements: request.requirements.clone(),
        };
        match create_project(&self.sdr, &job, initial).await {
            Ok(()) => {}
            Err(e) if e.code() == ErrorCode::Conflict => {
                if !self.holds_requirements(&job, request).await? {
                    return Err(reused(key));
                }
            }
            Err(e) => return Err(e),
        }
        match insert_and_record(
            self.jobs.as_ref(),
            self.events.as_ref(),
            self.clock.as_ref(),
            &job,
        )
        .await
        {
            Ok(()) => {
                tracing::info!(
                    run = job.run_id.as_str(),
                    job = job.id.as_str(),
                    "run submitted"
                );
                Ok(Submitted { job, created: true })
            }
            Err(e) if e.code() == ErrorCode::Conflict => {
                let stored = self.jobs.get(&job.id).await?;
                if stored.task == job.task {
                    Ok(Submitted {
                        job: stored,
                        created: false,
                    })
                } else {
                    Err(reused(key))
                }
            }
            Err(e) => Err(e),
        }
    }
}

/// A request the agent registry refused, as the client's error.
fn invalid_request(error: RuntimeError) -> Error {
    Error::new(ErrorCode::InvalidInput, error.to_string()).with_source(error)
}

fn reused(key: &Identifier) -> Error {
    Error::new(
        ErrorCode::Conflict,
        format!(
            "idempotency key {} was used for a different request",
            key.as_str()
        ),
    )
}
