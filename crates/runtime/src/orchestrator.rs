//! The orchestrator (blueprint sections 2.3 and 2.6): runs a job's task with
//! the agent the task names, through the `bounded_worker` loop.
//!
//! Every component is built at startup and passed in; nothing here reads
//! configuration or constructs a client. Bootstrap will build them (PR 12);
//! tests do here.

use std::sync::Arc;

use context::{ContextBuilder, ContextProfiles};
use contracts::types::{ApprovalAnswer, Job, NonEmptyString, RuntimeSettings};
use contracts::{Clock, EventLog, EvidenceStore, ExecutionContext, Gate, GateSubjectKind};
use evaluation::Evaluator;
use governance::SdrCommitGate;
use model_gateway::ModelGateway;
use sdr::SdrManager;
use tools::ToolGateway;
use tracing::Instrument;

use crate::agents::AgentRegistry;
use crate::agents::bounded_worker;
use crate::error::RuntimeError;
use crate::query_loop::{CheckpointSink, Continuation, NoCheckpoints, RunStream, Start};
use crate::tool_schemas::ToolSchemas;

/// What a run uses, built once at startup.
pub struct Components {
    pub agents: Arc<AgentRegistry>,
    /// The only writer of design state.
    pub sdr: Arc<SdrManager>,
    pub context: Arc<ContextBuilder>,
    /// The profiles the context builder was given: the tool definitions are
    /// counted with the default profile's estimate.
    pub context_profiles: ContextProfiles,
    pub tools: Arc<ToolGateway>,
    pub evaluator: Arc<Evaluator>,
    /// Where the evaluation and tool observations are stored: a run that
    /// continues from a checkpoint reads its last commit's evidence here.
    pub evidence: Arc<dyn EvidenceStore>,
    /// The commit gates every run shares: the `sdr_commit` gate and the
    /// approval gate for SDR commits. Each run adds its task's write scope.
    pub commit_gates: Vec<Arc<dyn Gate>>,
    pub models: Arc<ModelGateway>,
    /// The log every component appends to.
    pub events: Arc<dyn EventLog>,
    pub clock: Arc<dyn Clock>,
    pub settings: RuntimeSettings,
    /// The build of the harness, recorded with every commit.
    pub harness_build: NonEmptyString,
}

pub struct Orchestrator {
    components: Components,
    schemas: ToolSchemas,
}

impl Orchestrator {
    /// Fails if no commit gate is the `sdr_commit` gate: a commit would then
    /// need no passing checks.
    pub fn new(components: Components) -> Result<Self, RuntimeError> {
        let has_commit_gate = components.commit_gates.iter().any(|gate| {
            gate.id().as_str() == SdrCommitGate::ID
                && gate.subject_kind() == GateSubjectKind::SdrCommit
        });
        if !has_commit_gate {
            let ids: Vec<&str> = components
                .commit_gates
                .iter()
                .map(|g| g.id().as_str())
                .collect();
            return Err(RuntimeError::InvalidSetup(format!(
                "the commit gates ({}) do not include `{}`",
                if ids.is_empty() {
                    "none".to_owned()
                } else {
                    ids.join(", ")
                },
                SdrCommitGate::ID
            )));
        }
        Ok(Self {
            components,
            schemas: ToolSchemas::load()?,
        })
    }

    /// Runs `job` from its start: appends `run_started`, then steps until a
    /// stop reason. The stream yields the run's events, then how it stopped.
    /// `ctx` carries the correlation id and the cancellation; the job's ids
    /// are added to it.
    pub fn run(&self, job: Job, ctx: ExecutionContext) -> RunStream<'_> {
        self.drive(Start::New(Box::new(job)), &NoCheckpoints, ctx)
    }

    /// Runs from `start`, saving a checkpoint through `sink` after each model
    /// reply, after each tool result, and when a turn closes or a commit
    /// lands (ADR-0031). The stream yields the events from the job's
    /// `progress.last_event_seq` on, then how the run stopped; the outcome
    /// carries the run's final record.
    pub fn drive<'a>(
        &'a self,
        start: Start,
        sink: &'a dyn CheckpointSink,
        ctx: ExecutionContext,
    ) -> RunStream<'a> {
        let span = run_span(start.job(), &ctx);
        RunStream::new(move |sender| {
            bounded_worker::run(&self.components, &self.schemas, start, sink, ctx, sender)
                .instrument(span)
        })
    }

    /// Continues a run that stopped with `approval_required`, in memory.
    /// The call that asked goes through the gates again with `answer`, then
    /// the run goes on. The stream yields the events after the ones the
    /// stopped run yielded.
    pub fn resume(
        &self,
        continuation: Continuation,
        answer: ApprovalAnswer,
        ctx: ExecutionContext,
    ) -> RunStream<'_> {
        self.drive(
            Start::Resume {
                continuation: Box::new(continuation),
                answer,
            },
            &NoCheckpoints,
            ctx,
        )
    }
}

fn run_span(job: &Job, ctx: &ExecutionContext) -> tracing::Span {
    tracing::info_span!(
        "run",
        project_id = job.project_id.as_str(),
        run_id = job.run_id.as_str(),
        task_id = job.task.id.as_str(),
        agent_id = job.task.agent_id.as_str(),
        correlation_id = ctx.correlation_id().as_str(),
    )
}
