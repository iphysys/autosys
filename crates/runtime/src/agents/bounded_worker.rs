//! The `bounded_worker` loop (blueprint section 5.2, ADR-0030): one model
//! call per step, its tool calls in order, and the stop conditions of
//! section 5.4 checked between them.
//!
//! - **A step** builds the context, calls the model, records the call, and
//!   runs the tool calls of the reply, if any.
//! - **The open turn** holds the replies with tool calls and their results.
//!   It closes when the model replies without a tool call, or when one of
//!   its proposals commits; the calls after that proposal are not run. The
//!   next context is rebuilt from the SDR and the event log alone.
//! - **Repairs.** A tool call that fails or is denied, a proposal that is
//!   invalid or rejected, and a reply that ends the turn without success
//!   (a cut-off reply and a refusal included) each count one failure. A
//!   commit resets the count; one failure beyond `max_repair_attempts`
//!   ends the run with `failure`.
//! - **Success** is decided by the checks and gates of a final commit
//!   ([`crate::stop`]), never by what the model says.
//! - **Harness failures** (a store, the model gateway after its retries, an
//!   invalid task) record an `error` event when they can and end the stream
//!   with `Err`; no `run_stopped` is written.

use std::sync::Arc;

use context::{AssembledContext, BuildRequest, ByteEstimate, ContextError, TokenCounter};
use contracts::ids::{ApprovalId, SdrVersion, StepId};
use contracts::replay::{CallKey, record_call};
use contracts::types::{
    ApprovalAnswer, ElementKind, ErrorInfo, Event, EventPayload, Job, NonEmptyString, Requirement,
    SdrSlice, StopReason, Task, Usage,
};
use contracts::{
    Error, ErrorCode, EventKind, ExecutionContext, Gate, Message, ModelRequest, Role, ToolSpec,
};
use governance::{GateRunner, WriteScopeGate};
use model_gateway::ModelCall;
use tokio::sync::mpsc;

use crate::agents::factory::ModelChoice;
use crate::agents::registry::AgentDefinition;
use crate::budget::{self, Exhausted};
use crate::cancellation::{Deadline, timed};
use crate::capability_router::{
    CapabilityRouter, Effect, Routed, StepInfo, call_step_id, tool_calls, tool_result,
};
use crate::continuation;
use crate::error::RuntimeError;
use crate::events::Recorder;
use crate::orchestrator::Components;
use crate::query_loop::{
    CheckpointSink, CommitView, Continuation, Forwarder, LoopState, OpenStep, RunOutcome, Start,
};
use crate::sdr_tools::{Provenance, SdrTools};
use crate::stop::{self, ResultParts};
use crate::tool_schemas::ToolSchemas;

/// The code of the error that ends a run whose repairs ran out.
const REPAIR_EXHAUSTED: &str = "repair.exhausted";

/// Runs a job from `start` until it stops, sending its events to `sender`
/// and its checkpoints to `sink`.
pub(crate) async fn run(
    components: &Components,
    schemas: &ToolSchemas,
    start: Start,
    sink: &dyn CheckpointSink,
    ctx: ExecutionContext,
    sender: mpsc::Sender<Event>,
) -> Result<RunOutcome, RuntimeError> {
    let job = start.job();
    let ctx = run_context(job, ctx)?;
    let io_timeout = std::time::Duration::from_secs(components.settings.io_timeout_seconds.get());
    let recorder = Recorder {
        events: components.events.as_ref(),
        clock: components.clock.as_ref(),
        io_timeout,
    };
    let forwarder = Forwarder {
        events: components.events.as_ref(),
        run_id: job.run_id.clone(),
        sender,
        io_timeout,
    };
    let task = job.task.clone();
    let deadline = Deadline::new(
        task.budget.max_wall_time_seconds.get(),
        job.budget_used.wall_time_seconds,
    );

    let prepared = prepare(components, schemas, &task, recorder, io_timeout);
    let (agent, model, router, specs, reserved_tokens) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let mut job = job.clone();
            return Err(failed(&recorder, &forwarder, &mut job, error, &ctx).await);
        }
    };
    let worker = Worker {
        components,
        task: &task,
        agent,
        model,
        router,
        specs,
        reserved_tokens,
        recorder,
        forwarder,
        sink,
        ctx: &ctx,
        deadline,
    };
    match start {
        Start::New(job) => worker.begin(LoopState::new(*job)).await,
        Start::Recover { job, record: None } => {
            let mut state = LoopState::new(*job);
            match recount(components, &mut state, io_timeout).await {
                Ok(()) => worker.begin(state).await,
                Err(error) => Err(worker.fail(&mut state, error).await),
            }
        }
        Start::Recover {
            job,
            record: Some(record),
        } => {
            let mut fallback = (*job).clone();
            match worker.load(*job, &record).await {
                Ok(loaded) => {
                    let continuation::Loaded { state, turn, step } = loaded;
                    worker
                        .drive(state, turn, step.map(|step| (step, None)))
                        .await
                }
                Err(error) => Err(failed(
                    &worker.recorder,
                    &worker.forwarder,
                    &mut fallback,
                    error,
                    &ctx,
                )
                .await),
            }
        }
        Start::Answer {
            job,
            record,
            answer,
        } => {
            let mut fallback = (*job).clone();
            let loaded = match worker.load(*job, &record).await {
                Ok(loaded) => loaded,
                Err(error) => {
                    return Err(failed(
                        &worker.recorder,
                        &worker.forwarder,
                        &mut fallback,
                        error,
                        &ctx,
                    )
                    .await);
                }
            };
            let continuation::Loaded { state, turn, step } = loaded;
            match step {
                Some(step) => worker.drive(state, turn, Some((step, Some(answer)))).await,
                None => {
                    let error = RuntimeError::Internal(
                        "an answer for a checkpoint with no open step".to_owned(),
                    );
                    Err(failed(
                        &worker.recorder,
                        &worker.forwarder,
                        &mut fallback,
                        error,
                        &ctx,
                    )
                    .await)
                }
            }
        }
        Start::Resume {
            continuation,
            answer,
        } => {
            let Continuation {
                state, turn, step, ..
            } = *continuation;
            worker.drive(state, turn, Some((step, Some(answer)))).await
        }
    }
}

/// Adds the model calls recorded after the job's last checkpoint to its
/// usage: they happened, though the checkpoint does not show them
/// (ADR-0031). A call that never returned is not counted.
async fn recount(
    components: &Components,
    state: &mut LoopState,
    io_timeout: std::time::Duration,
) -> Result<(), RuntimeError> {
    let after = state.job.progress.last_event_seq;
    let calls = timed(
        io_timeout,
        "reading the calls after the checkpoint",
        components.events.read_kinds(
            &state.job.run_id,
            after,
            &[EventKind::MODEL_CALL_COMPLETED],
            usize::MAX,
        ),
    )
    .await?;
    for event in calls {
        if let EventPayload::ModelCallCompleted { usage, .. } = event.payload {
            let used = &mut state.job.budget_used;
            used.input_tokens = used.input_tokens.saturating_add(usage.input_tokens);
            used.output_tokens = used.output_tokens.saturating_add(usage.output_tokens);
            used.steps += 1;
            state.job.progress.steps_completed += 1;
        }
    }
    Ok(())
}

type Prepared<'a> = (
    &'a AgentDefinition,
    ModelChoice,
    CapabilityRouter<'a>,
    Vec<ToolSpec>,
    u64,
);

/// The agent and model a task runs with, and its tools. A task that does
/// not fit its agent is a harness failure.
fn prepare<'a>(
    c: &'a Components,
    schemas: &'a ToolSchemas,
    task: &'a Task,
    recorder: Recorder<'a>,
    io_timeout: std::time::Duration,
) -> Result<Prepared<'a>, RuntimeError> {
    let agent = c.agents.agent_for(task)?;
    let model = agent.model(&c.models)?;
    let write_scope =
        WriteScopeGate::new(&task.sdr.write_scope).map_err(|source| RuntimeError::Component {
            operation: "building the write-scope gate",
            source,
        })?;
    let mut gates: Vec<Arc<dyn Gate>> = vec![Arc::new(write_scope)];
    gates.extend(c.commit_gates.iter().cloned());
    let gates = GateRunner::new(gates, c.events.clone(), c.clock.clone()).map_err(|source| {
        RuntimeError::Component {
            operation: "building the commit gates",
            source,
        }
    })?;
    let router = CapabilityRouter {
        task,
        tools: &c.tools,
        sdr: SdrTools {
            sdr: &c.sdr,
            evaluator: &c.evaluator,
            evidence: c.evidence.as_ref(),
            gates,
            recorder,
            io_timeout,
            project: task.project_id.clone(),
            read_slice: task.sdr.read_slice.clone(),
            agent: task.agent_id.clone(),
        },
        schemas,
        recorder,
    };
    let specs = router.tool_specs()?;
    let profile = c
        .context_profiles
        .resolve(None)
        .map_err(RuntimeError::Context)?;
    let definitions = serde_json::to_string(&specs).map_err(|e| {
        RuntimeError::Internal(format!("cannot serialize the tool definitions: {e}"))
    })?;
    let reserved_tokens = ByteEstimate::for_profile(profile).count(&definitions);
    Ok((agent, model, router, specs, reserved_tokens))
}

/// The run's context: the caller's, with the job's ids. A context that is
/// already for another run is refused.
fn run_context(job: &Job, ctx: ExecutionContext) -> Result<ExecutionContext, RuntimeError> {
    let mut problems = Vec::new();
    if let Some(run) = ctx.run_id()
        && run != &job.run_id
    {
        problems.push(format!(
            "the execution context is for run {}, the job for run {}",
            run.as_str(),
            job.run_id.as_str()
        ));
    }
    if job.task.project_id != job.project_id {
        problems.push(format!(
            "the task is for project {}, the job for project {}",
            job.task.project_id.as_str(),
            job.project_id.as_str()
        ));
    }
    if !problems.is_empty() {
        return Err(RuntimeError::InvalidTask {
            task: job.task.id.as_str().to_owned(),
            problems,
        });
    }
    Ok(ctx
        .with_project(job.project_id.clone())
        .with_run(job.run_id.clone())
        .with_task(job.task.id.clone())
        .with_agent(job.task.agent_id.clone()))
}

/// Records a harness failure, when it can, and returns it.
async fn failed(
    recorder: &Recorder<'_>,
    forwarder: &Forwarder<'_>,
    job: &mut Job,
    error: RuntimeError,
    ctx: &ExecutionContext,
) -> RuntimeError {
    let info = Error::new(error.code(), error.to_string())
        .with_correlation_id(ctx.correlation_id().clone())
        .to_info();
    if let Err(e) = recorder.error(info, ctx).await {
        tracing::warn!(error = %e, "the harness failure could not be recorded");
    }
    if let Err(e) = forwarder.forward_job(job).await {
        tracing::warn!(error = %e, "the run's last events could not be forwarded");
    }
    tracing::error!(code = %error.code(), error = %error, "run failed");
    error
}

/// Why a run stopped, before it is recorded.
enum Stop {
    Success,
    Failure,
    /// A budget is used up, or the context does not fit: the code and the
    /// message of the `error` event.
    Budget {
        code: &'static str,
        message: String,
    },
    Cancelled,
    Approval {
        turn: Vec<Message>,
        step: Box<OpenStep>,
        approval_id: ApprovalId,
    },
}

/// What running a reply's tool calls came to.
enum Calls {
    /// Every call is answered; the reply and its results join the turn.
    Answered(Box<OpenStep>),
    /// A proposal committed: the turn closes.
    TurnClosed,
    /// A call waits for a human's answer.
    Ask {
        step: Box<OpenStep>,
        approval_id: ApprovalId,
    },
    Stop(Stop),
}

struct Worker<'r> {
    components: &'r Components,
    task: &'r Task,
    agent: &'r AgentDefinition,
    model: ModelChoice,
    router: CapabilityRouter<'r>,
    specs: Vec<ToolSpec>,
    reserved_tokens: u64,
    recorder: Recorder<'r>,
    forwarder: Forwarder<'r>,
    sink: &'r dyn CheckpointSink,
    ctx: &'r ExecutionContext,
    deadline: Deadline,
}

impl Worker<'_> {
    /// Appends `run_started`, which the log keeps once, then runs.
    async fn begin(&self, mut state: LoopState) -> Result<RunOutcome, RuntimeError> {
        let started = self
            .recorder
            .append(
                EventPayload::RunStarted {
                    task_id: self.task.id.clone(),
                    agent_id: self.task.agent_id.clone(),
                },
                self.ctx,
            )
            .await;
        if let Err(error) = started {
            return Err(self.fail(&mut state, error).await);
        }
        self.drive(state, Vec::new(), None).await
    }

    /// The loop state of `record`, with the usage after it counted again.
    async fn load(
        &self,
        job: Job,
        record: &contracts::types::ContinuationRecord,
    ) -> Result<continuation::Loaded, RuntimeError> {
        let mut loaded = continuation::load(
            job,
            record,
            &self.components.sdr,
            self.components.evidence.as_ref(),
            &self.task.project_id,
            self.recorder.io_timeout,
            self.ctx,
        )
        .await?;
        recount(self.components, &mut loaded.state, self.recorder.io_timeout).await?;
        Ok(loaded)
    }

    /// Saves the run's state as its next checkpoint.
    async fn checkpoint(
        &self,
        state: &mut LoopState,
        turn: &[Message],
        step: Option<&OpenStep>,
    ) -> Result<(), RuntimeError> {
        self.tick(state);
        state.checkpoint += 1;
        let record = continuation::record(state, turn, step, None)?;
        timed(
            self.recorder.io_timeout,
            "saving a checkpoint",
            self.sink.checkpoint(&state.job, &record),
        )
        .await
    }

    async fn drive(
        &self,
        mut state: LoopState,
        mut turn: Vec<Message>,
        resumed: Option<(OpenStep, Option<ApprovalAnswer>)>,
    ) -> Result<RunOutcome, RuntimeError> {
        let stopped = async {
            self.forwarder.forward(&mut state).await?;
            self.steps(&mut state, &mut turn, resumed).await
        }
        .await;
        let stop = match stopped {
            Ok(stop) => stop,
            // Whatever was interrupted, a cancelled run or a spent wall-time
            // budget is a stop, not a harness failure.
            Err(error) => match self.interruption() {
                Some(stop) if error.code() == ErrorCode::Cancelled => stop,
                _ => return Err(self.fail(&mut state, error).await),
            },
        };
        match self.finish(&mut state, stop).await {
            Ok(outcome) => Ok(outcome),
            Err(error) => Err(self.fail(&mut state, error).await),
        }
    }

    async fn steps(
        &self,
        state: &mut LoopState,
        turn: &mut Vec<Message>,
        resumed: Option<(OpenStep, Option<ApprovalAnswer>)>,
    ) -> Result<Stop, RuntimeError> {
        if let Some((step, answer)) = resumed {
            match self.calls(state, turn, step, answer).await? {
                Calls::Answered(step) => close_message(turn, *step),
                Calls::TurnClosed => {
                    turn.clear();
                    self.checkpoint(state, turn, None).await?;
                }
                Calls::Ask { step, approval_id } => {
                    return Ok(Stop::Approval {
                        turn: turn.clone(),
                        step,
                        approval_id,
                    });
                }
                Calls::Stop(stop) => return Ok(stop),
            }
        }
        let budget = &self.task.budget;
        loop {
            if let Some(stop) = self.interruption() {
                return Ok(stop);
            }
            if let Some(exhausted) = budget::before_step(budget, &state.job.budget_used) {
                return Ok(self.exhausted(exhausted, state));
            }
            let number = state.job.budget_used.steps + 1;
            let step_ctx = self.ctx.for_step(step_id(number)?);

            let built = self
                .deadline
                .run(
                    &step_ctx,
                    self.components.context.build(
                        BuildRequest {
                            job: &state.job,
                            prompt: self.agent.spec().prompt.as_str(),
                            agent_spec: self.agent.provenance(),
                            skills: &[],
                            profile: None,
                            open_turn: turn,
                            reserved_tokens: self.reserved_tokens,
                            at: None,
                        },
                        &step_ctx,
                    ),
                )
                .await;
            self.tick(state);
            let assembled = match built {
                Ok(assembled) => assembled,
                Err(overflow @ ContextError::Overflow { .. }) => {
                    return Ok(Stop::Budget {
                        code: ErrorCode::ContextOverflow.as_str(),
                        message: overflow.to_string(),
                    });
                }
                Err(ContextError::Cancelled) => {
                    return self
                        .interruption()
                        .ok_or(RuntimeError::Context(ContextError::Cancelled));
                }
                Err(e) => return Err(RuntimeError::Context(e)),
            };
            state.head = Some(SdrVersion(assembled.manifest().sdr_version));
            self.forwarder.forward(state).await?;

            let Some(max_output) = budget::output_limit(
                budget,
                &state.job.budget_used,
                assembled.total_tokens(),
                self.model.max_output_tokens,
            ) else {
                return Ok(self.exhausted(Exhausted::Tokens, state));
            };
            // The gateway sets the profile's model id.
            let request =
                assembled
                    .clone()
                    .into_model_request(String::new(), self.specs.clone(), max_output);
            let called = self
                .deadline
                .run(
                    &step_ctx,
                    self.components.models.complete(
                        &self.model.profile,
                        request.clone(),
                        &step_ctx,
                    ),
                )
                .await;
            self.tick(state);
            let call = match called {
                Ok(call) => call,
                Err(e) if e.code() == ErrorCode::Cancelled => {
                    return self.interruption().ok_or(RuntimeError::Component {
                        operation: "the model call",
                        source: e,
                    });
                }
                Err(source) => {
                    return Err(RuntimeError::Component {
                        operation: "the model call",
                        source,
                    });
                }
            };
            self.account(state, &call);
            self.record_call(&assembled, &request, &call).await?;
            self.model_call_completed(&assembled, &call).await?;
            self.forwarder.forward(state).await?;

            let calls = tool_calls(&call.response.content);
            if calls.is_empty() {
                // The turn ends without success: whatever the reply says,
                // only a final commit through the gates succeeds.
                turn.clear();
                state.failures += 1;
                self.update_pending_work(state).await?;
                self.checkpoint(state, turn, None).await?;
                if budget::repairs_exhausted(budget, state.failures) {
                    return Ok(Stop::Failure);
                }
                continue;
            }
            let step = OpenStep {
                number,
                sdr_version: SdrVersion(assembled.manifest().sdr_version),
                prompts: assembled.manifest().prompts.clone(),
                assistant: Message {
                    role: Role::Assistant,
                    content: call.response.content,
                },
                calls,
                results: Vec::new(),
                next: 0,
            };
            self.checkpoint(state, turn, Some(&step)).await?;
            match self.calls(state, turn, step, None).await? {
                Calls::Answered(step) => close_message(turn, *step),
                Calls::TurnClosed => {
                    turn.clear();
                    self.checkpoint(state, turn, None).await?;
                }
                Calls::Ask { step, approval_id } => {
                    return Ok(Stop::Approval {
                        turn: turn.clone(),
                        step,
                        approval_id,
                    });
                }
                Calls::Stop(stop) => return Ok(stop),
            }
        }
    }

    /// Runs the step's tool calls from `step.next`, in order, and saves a
    /// checkpoint after each result. `answer` is a human's answer for the
    /// first of them, when the run resumes.
    async fn calls(
        &self,
        state: &mut LoopState,
        turn: &[Message],
        mut step: OpenStep,
        mut answer: Option<ApprovalAnswer>,
    ) -> Result<Calls, RuntimeError> {
        let budget = &self.task.budget;
        let step_ctx = self.ctx.for_step(step_id(step.number)?);
        while let Some(call) = step.calls.get(step.next).cloned() {
            let index = step.next;
            let approval = answer.take();
            if approval.is_none() {
                // A resumed call was counted when it first ran.
                if state.job.budget_used.tool_calls >= budget.max_tool_calls.get() {
                    return Ok(Calls::Stop(self.exhausted(Exhausted::ToolCalls, state)));
                }
                state.job.budget_used.tool_calls += 1;
            }
            let call_step = call_step_id(step.number, index + 1)?;
            let call_ctx = step_ctx.for_step(call_step.clone());
            let routed = {
                let info = StepInfo {
                    sdr_version: step.sdr_version.clone(),
                    provenance: Provenance {
                        harness_build: &self.components.harness_build,
                        model_ids: &state.harness.model_ids,
                        prompts: &step.prompts,
                        agent_spec: self.agent.provenance(),
                    },
                };
                self.deadline
                    .run(
                        &step_ctx,
                        self.router
                            .route(&call, &call_step, &info, approval, &call_ctx),
                    )
                    .await
            };
            self.tick(state);
            let routed = match routed {
                Ok(routed) => routed,
                Err(e) if e.code() == ErrorCode::Cancelled => {
                    self.forwarder.forward(state).await?;
                    return self.interruption().map(Calls::Stop).ok_or(e);
                }
                Err(e) => return Err(e),
            };
            self.forwarder.forward(state).await?;

            let answered = match routed {
                Routed::Ask {
                    approval_id,
                    evidence_ids,
                } => {
                    state.add_evidence(&evidence_ids);
                    return Ok(Calls::Ask {
                        step: Box::new(step),
                        approval_id,
                    });
                }
                Routed::Answered(answered) => answered,
            };
            state.add_evidence(&answered.evidence_ids);
            match answered.effect {
                Effect::Committed(commit) => {
                    state.failures = 0;
                    state.committed.push(commit.transition_id.clone());
                    state.head = Some(commit.version.clone());
                    let view = CommitView {
                        transition_id: commit.transition_id.clone(),
                        version: commit.version,
                        final_: commit.final_,
                        requirements: commit.document.requirements,
                        evidence: commit.evidence,
                    };
                    let unmet = stop::unmet(
                        &view.requirements,
                        &stop::required(&view.requirements, &self.task.requirement_ids),
                        &view.evidence,
                    );
                    let succeeded = view.final_ && unmet.is_empty();
                    state.job.pending_work = stop::pending_work(&unmet, &view.version, view.final_);
                    state.last_commit = Some(view);
                    if succeeded {
                        return Ok(Calls::Stop(Stop::Success));
                    }
                    if let Some(stop) = self.interruption() {
                        return Ok(Calls::Stop(stop));
                    }
                    // The commit closes the turn; the calls after it are not run.
                    return Ok(Calls::TurnClosed);
                }
                Effect::Failed => {
                    if let Some(stop) = self.interruption() {
                        return Ok(Calls::Stop(stop));
                    }
                    state.failures += 1;
                    if budget::repairs_exhausted(budget, state.failures) {
                        return Ok(Calls::Stop(Stop::Failure));
                    }
                }
                Effect::Observed => {
                    if let Some(stop) = self.interruption() {
                        return Ok(Calls::Stop(stop));
                    }
                }
            }
            step.results
                .push(tool_result(&call.id, &answered.content, answered.is_error)?);
            step.next = index + 1;
            self.checkpoint(state, turn, Some(&step)).await?;
        }
        Ok(Calls::Answered(Box::new(step)))
    }

    /// `cancelled` if the run was cancelled, `budget_exhausted` if its wall
    /// time is up.
    fn interruption(&self) -> Option<Stop> {
        if self.ctx.is_cancelled() {
            Some(Stop::Cancelled)
        } else if self.deadline.passed() {
            let used = Usage {
                input_tokens: 0,
                output_tokens: 0,
                tool_calls: 0,
                steps: 0,
                wall_time_seconds: self.deadline.used_seconds(),
            };
            Some(self.budget_stop(Exhausted::WallTime, &used))
        } else {
            None
        }
    }

    fn exhausted(&self, exhausted: Exhausted, state: &LoopState) -> Stop {
        self.budget_stop(exhausted, &state.job.budget_used)
    }

    fn budget_stop(&self, exhausted: Exhausted, used: &Usage) -> Stop {
        Stop::Budget {
            code: exhausted.code(),
            message: exhausted.message(&self.task.budget, used),
        }
    }

    fn tick(&self, state: &mut LoopState) {
        state.job.budget_used.wall_time_seconds = self.deadline.used_seconds();
    }

    /// Adds a model call to the job's usage.
    /// Counts a model call's usage. Its model reaches the harness from the
    /// log, when `model_call_completed` is forwarded.
    fn account(&self, state: &mut LoopState, call: &ModelCall) {
        let used = &mut state.job.budget_used;
        used.input_tokens = used
            .input_tokens
            .saturating_add(call.response.usage.input_tokens);
        used.output_tokens = used
            .output_tokens
            .saturating_add(call.response.usage.output_tokens);
        used.steps += 1;
        state.job.progress.steps_completed += 1;
    }

    /// Stores the call for exact replay (ADR-0035), keyed by the seq of the
    /// context's `context_assembled`. It comes before `model_call_completed`,
    /// so every such event has a record; if it fails, the step fails.
    async fn record_call(
        &self,
        assembled: &AssembledContext,
        request: &ModelRequest,
        call: &ModelCall,
    ) -> Result<(), RuntimeError> {
        let run_id = self
            .ctx
            .run_id()
            .ok_or_else(|| RuntimeError::Internal("a model call without a run".to_owned()))?;
        let context_seq = assembled.event_seq().ok_or_else(|| {
            RuntimeError::Internal("a built context has no context_assembled seq".to_owned())
        })?;
        let recorded = record_call(
            &CallKey {
                run_id,
                context_seq,
                context_hash: assembled.hash(),
                model_profile: &call.profile,
            },
            request,
            &call.response,
        )
        .map_err(|source| RuntimeError::Component {
            operation: "recording the model call",
            source,
        })?;
        timed(
            self.recorder.io_timeout,
            "recording the model call",
            self.components
                .replay
                .put_call(&recorded.record, &recorded.blobs),
        )
        .await
    }

    /// Appends `model_call_completed`, then compares the context's size
    /// estimate with the provider's count (ADR-0029).
    async fn model_call_completed(
        &self,
        assembled: &AssembledContext,
        call: &ModelCall,
    ) -> Result<(), RuntimeError> {
        let model_id: NonEmptyString = call.response.model_id.parse().map_err(|_| {
            RuntimeError::Internal("the model gateway returned an empty model id".to_owned())
        })?;
        self.recorder
            .append(
                EventPayload::ModelCallCompleted {
                    model_profile: call.profile.clone(),
                    model_id,
                    usage: call.response.usage.clone(),
                },
                self.ctx,
            )
            .await?;
        assembled.compare_usage(&call.response.usage);
        Ok(())
    }

    /// Sets the job's pending work to what is unmet at the latest version.
    async fn update_pending_work(&self, state: &mut LoopState) -> Result<(), RuntimeError> {
        let Some(head) = state.head.clone() else {
            return Ok(());
        };
        let (requirements, evidence, final_) = match &state.last_commit {
            Some(commit) if commit.version == head => (
                commit.requirements.clone(),
                commit.evidence.clone(),
                commit.final_,
            ),
            _ => (self.requirements_at(&head).await?, Vec::new(), false),
        };
        let unmet = stop::unmet(
            &requirements,
            &stop::required(&requirements, &self.task.requirement_ids),
            &evidence,
        );
        state.job.pending_work = stop::pending_work(&unmet, &head, final_);
        Ok(())
    }

    /// Every requirement at `version`, whatever the task may read: the
    /// harness decides what is unmet, not the model.
    async fn requirements_at(
        &self,
        version: &SdrVersion,
    ) -> Result<Vec<Requirement>, RuntimeError> {
        let slice = SdrSlice {
            kinds: vec![ElementKind::Requirement],
            ids: Vec::new(),
        };
        let document = timed(
            self.recorder.io_timeout,
            "reading the requirements",
            self.components
                .sdr
                .read(&self.task.project_id, &slice, version, self.ctx),
        )
        .await?;
        Ok(document.requirements)
    }

    /// Records the stop, checks the result against the agent's schema, and
    /// returns the outcome.
    async fn finish(&self, state: &mut LoopState, stop: Stop) -> Result<RunOutcome, RuntimeError> {
        self.tick(state);
        let budget = &self.task.budget;
        let (stop_reason, error, waiting) = match stop {
            Stop::Success => (StopReason::Success, None, None),
            Stop::Failure => {
                let message = format!(
                    "{} failures since the last commit, above max_repair_attempts {}",
                    state.failures, budget.max_repair_attempts
                );
                (
                    StopReason::Failure,
                    Some(info(REPAIR_EXHAUSTED, &message, self.ctx)?),
                    None,
                )
            }
            Stop::Budget { code, message } => (
                StopReason::BudgetExhausted,
                Some(info(code, &message, self.ctx)?),
                None,
            ),
            Stop::Cancelled => (StopReason::Cancelled, None, None),
            Stop::Approval {
                turn,
                step,
                approval_id,
            } => (
                StopReason::ApprovalRequired,
                None,
                Some((turn, step, approval_id)),
            ),
        };
        let result = stop::task_result(ResultParts {
            job: &state.job,
            stop_reason,
            committed: &state.committed,
            last_committed: state.last_commit.as_ref().map(|c| c.version.clone()),
            evidence_ids: &state.evidence_ids,
            error: error.clone(),
        });
        let problems =
            self.agent
                .result_problems(&result)
                .map_err(|source| RuntimeError::Component {
                    operation: "checking the task result",
                    source,
                })?;
        if !problems.is_empty() {
            return Err(RuntimeError::Internal(format!(
                "the task result breaks {}: {}",
                self.agent.spec().result_schema.as_str(),
                problems.join("; ")
            )));
        }

        if let Some(error) = error {
            self.recorder.error(error, self.ctx).await?;
        }
        self.recorder
            .append(
                EventPayload::RunStopped {
                    stop_reason,
                    sdr_version: state.head.clone(),
                },
                self.ctx,
            )
            .await?;
        self.forwarder.forward(state).await?;
        tracing::info!(
            stop_reason = %stop_reason,
            steps = state.job.budget_used.steps,
            tool_calls = state.job.budget_used.tool_calls,
            "run stopped"
        );
        state.checkpoint += 1;
        let record = match &waiting {
            Some((turn, step, approval_id)) => {
                continuation::record(state, turn, Some(step), Some(approval_id))?
            }
            None => continuation::record(state, &[], None, None)?,
        };
        let continuation = waiting.map(|(turn, step, approval_id)| Continuation {
            state: state.clone(),
            turn,
            step: *step,
            approval_id,
        });
        Ok(RunOutcome {
            stop_reason,
            result,
            job: state.job.clone(),
            record,
            continuation,
        })
    }

    async fn fail(&self, state: &mut LoopState, error: RuntimeError) -> RuntimeError {
        self.tick(state);
        failed(
            &self.recorder,
            &self.forwarder,
            &mut state.job,
            error,
            self.ctx,
        )
        .await
    }
}

/// Adds the reply and its results to the open turn.
fn close_message(turn: &mut Vec<Message>, step: OpenStep) {
    turn.push(step.assistant);
    turn.push(Message {
        role: Role::User,
        content: step.results,
    });
}

fn step_id(number: u64) -> Result<StepId, RuntimeError> {
    format!("step-{number}")
        .parse()
        .map_err(|_| RuntimeError::Internal("a generated step id is invalid".to_owned()))
}

/// An error record whose code need not be a contract error code, such as
/// `budget.steps_exhausted`. None of these is retryable.
fn info(code: &str, message: &str, ctx: &ExecutionContext) -> Result<ErrorInfo, RuntimeError> {
    let mut info = Error::new(ErrorCode::Internal, message)
        .with_correlation_id(ctx.correlation_id().clone())
        .to_info();
    info.code = code
        .parse()
        .map_err(|_| RuntimeError::Internal(format!("{code} is not an error code")))?;
    info.retryable = false;
    Ok(info)
}
