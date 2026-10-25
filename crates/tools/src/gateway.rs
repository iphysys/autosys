//! The execution pipeline of a tool call (blueprint section 14.2) and replay
//! from step records (ADR-0026).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use contracts::ids::{AgentId, ApprovalId, EvidenceId, RunId, SdrVersion, StepId, ToolName};
use contracts::types::{
    Actor, ApprovalAnswer, ErrorInfo, EventPayload, Evidence, EvidenceBody, EvidenceSchemaVersion,
    GateDecision, NonEmptyString, StepOutcome, StepRecord, Timestamp, ToolCallOutcome,
};
use contracts::{
    Clock, Error, ErrorCode, EventLog, EvidenceStore, ExecutionContext, GateSubject, NewEvent,
    NewStep, StepStart, StepStore, ToolRequest,
};
use governance::GateRunner;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::bounded;
use crate::normalizer::{evidence_id, quantities, request_sha256, validated_result};
use crate::registry::{Registered, ToolRegistry};

/// One tool call as the orchestrator asks for it.
#[derive(Clone, Debug)]
pub struct ToolCall<'a> {
    pub request: &'a ToolRequest,
    pub agent_id: &'a AgentId,
    /// The SDR version the step works against, recorded on its evidence.
    pub sdr_version: SdrVersion,
    /// A human's answer, when the run resumes after this call asked.
    pub approval: Option<ApprovalAnswer>,
}

/// What a tool call produced. Every tool problem is an observation; only
/// failures of the harness itself, such as an unreachable store, are `Err`.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolObservation {
    /// The tool returned a result that satisfies its result schema.
    Succeeded {
        tool_version: NonEmptyString,
        result: Map<String, Value>,
        evidence_ids: Vec<EvidenceId>,
        /// True when the result came from the step record, without calling
        /// the tool.
        replayed: bool,
    },
    /// The tool reported an error, timed out, was cancelled, returned an
    /// invalid result, or the arguments broke the request schema.
    Failed { error: ErrorInfo, replayed: bool },
    /// A gate denied the call, or the tool is not available.
    Denied { reasons: Vec<String> },
    /// A gate asked for a human answer; call again with it.
    ApprovalRequired {
        approval_id: ApprovalId,
        reasons: Vec<String>,
    },
}

/// Runs tool calls:
///
/// 1. replay: a step whose record holds an outcome returns it without
///    calling the tool or writing evidence. Its `tool_call_completed` is
///    appended again, marked `recovered`, from the record: the log keeps
///    the first one, so this only repairs one a crash lost (ADR-0031);
/// 2. resolve the tool; an unavailable tool is still put to the gates, so
///    its denial is recorded;
/// 3. validate the arguments against the tool's request schema;
/// 4. gates (tool permission and approval), each decision in the event log;
/// 5. serialize calls to a tool that is not concurrency-safe;
/// 6. begin the step record and append `tool_call_started`;
/// 7. execute with the tool's timeout and the context's cancellation;
///    only a committed `tool_call_started` leads here (see [`StartedCall`]);
/// 8. validate the result against its schema and store evidence;
/// 9. complete the step record and append `tool_call_completed`.
///
/// A step whose earlier attempt was interrupted runs again only if its tool
/// is idempotent; otherwise it fails with `outcome_unknown` without calling
/// the tool (ADR-0026).
///
/// Every write carries the context's authority (ADR-0032). A worker whose
/// lease was reclaimed fails with `lease_lost` at its first write, before the
/// tool is reached.
pub struct ToolGateway {
    registry: ToolRegistry,
    gates: GateRunner,
    steps: Arc<dyn StepStore>,
    evidence: Arc<dyn EvidenceStore>,
    events: Arc<dyn EventLog>,
    clock: Arc<dyn Clock>,
    exclusive: HashMap<String, Mutex<()>>,
}

impl ToolGateway {
    pub fn new(
        registry: ToolRegistry,
        gates: GateRunner,
        steps: Arc<dyn StepStore>,
        evidence: Arc<dyn EvidenceStore>,
        events: Arc<dyn EventLog>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let exclusive = registry
            .available()
            .into_iter()
            .filter(|name| registry.policy(name).is_some_and(|p| !p.concurrency_safe))
            .map(|name| (name.to_owned(), Mutex::new(())))
            .collect();
        Self {
            registry,
            gates,
            steps,
            evidence,
            events,
            clock,
            exclusive,
        }
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Runs `call`. Needs a run in `ctx`; pass the step's context
    /// ([`ExecutionContext::for_step`]) so cancelling the step stops the
    /// call. Conflict if the idempotency key was used for a different
    /// request.
    pub async fn call(
        &self,
        call: ToolCall<'_>,
        ctx: &ExecutionContext,
    ) -> Result<ToolObservation, Error> {
        let request = call.request;
        let run_id = ctx.run_id().cloned().ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidInput,
                "tool calls need a run in the context",
            )
        })?;
        let step = NewStep {
            idempotency_key: request.idempotency_key.clone(),
            run_id: run_id.clone(),
            step_id: request.step_id.clone(),
            tool: request.tool.clone(),
            request_sha256: request_sha256(request)?,
        };
        let tool = request.tool.as_str();

        let interrupted = match self.steps.get(&step.idempotency_key).await {
            Ok(record) => {
                ensure_same_request(&record, &step)?;
                if let Some(outcome) = &record.outcome {
                    return self.replay(&record, outcome, ctx).await;
                }
                true
            }
            Err(e) if e.code() == ErrorCode::NotFound => false,
            Err(e) => return Err(e),
        };

        let registered = match self.registry.get(tool) {
            Ok(registered) => registered,
            Err(unavailable) if interrupted => {
                return self
                    .finish(&step, &call, ctx, Instant::now(), Err(unavailable))
                    .await;
            }
            Err(unavailable) => {
                let subject = GateSubject::ToolCall {
                    agent_id: call.agent_id,
                    request,
                };
                let decision = self.gates.decide(subject, call.approval, ctx).await?;
                let mut reasons = decision.outcome.reasons;
                reasons.push(unavailable.message().to_owned());
                return Ok(ToolObservation::Denied { reasons });
            }
        };
        if interrupted && !registered.policy.idempotent {
            return self
                .finish(&step, &call, ctx, Instant::now(), Err(not_repeated(tool)))
                .await;
        }

        let arguments = Value::Object(request.arguments.clone());
        let problems = self
            .registry
            .schemas()
            .errors(&registered.request_schema, &arguments)?;
        if !problems.is_empty() {
            let error = Error::new(
                ErrorCode::InvalidInput,
                format!(
                    "arguments for {tool} break {}: {}",
                    registered.request_schema,
                    problems.join("; ")
                ),
            );
            return Ok(failed(error, ctx, false));
        }

        let subject = GateSubject::ToolCall {
            agent_id: call.agent_id,
            request,
        };
        let decision = self.gates.decide(subject, call.approval, ctx).await?;
        match decision.outcome.decision {
            GateDecision::Allow => {}
            GateDecision::Ask => {
                let approval_id = decision.approval_id.ok_or_else(|| {
                    Error::new(
                        ErrorCode::Internal,
                        "an ask decision without an approval id",
                    )
                })?;
                return Ok(ToolObservation::ApprovalRequired {
                    approval_id,
                    reasons: decision.outcome.reasons,
                });
            }
            GateDecision::Deny | GateDecision::Retry | GateDecision::RejectTransition => {
                return Ok(ToolObservation::Denied {
                    reasons: decision.outcome.reasons,
                });
            }
        }

        let limit = Duration::from_secs(registered.policy.timeout_seconds.get());
        let _exclusive = match self.exclusive.get(tool) {
            Some(lock) => {
                let waited = bounded(ctx, limit, &format!("waiting for {tool}"), async {
                    Ok(lock.lock().await)
                })
                .await;
                match waited {
                    Ok(guard) => Some(guard),
                    Err(error) => return Ok(failed(error, ctx, false)),
                }
            }
            None => None,
        };

        if !interrupted {
            match self.steps.begin(&step, ctx.write_authority()).await? {
                StepStart::Started(_) => {}
                StepStart::Existing(record) => {
                    if let Some(outcome) = &record.outcome {
                        return self.replay(&record, outcome, ctx).await;
                    }
                    if !registered.policy.idempotent {
                        let error = Error::new(
                            ErrorCode::Conflict,
                            format!(
                                "step {} is already running elsewhere and {tool} is not idempotent",
                                step.step_id.as_str()
                            ),
                        );
                        return Ok(failed(error, ctx, false));
                    }
                }
            }
        }
        let announced = self.announce_started(registered, &call, &step, ctx).await?;
        let started = Instant::now();
        let produced = self.invoke_started(announced, ctx).await;
        self.finish(&step, &call, ctx, started, produced).await
    }

    /// Appends `tool_call_started` with the context's authority. The append
    /// is the call's write-ahead record: its idempotency key is stored in the
    /// same transaction that checks the fence, so a stale fence ends the
    /// call here.
    async fn announce_started<'a>(
        &self,
        registered: &'a Registered,
        call: &'a ToolCall<'a>,
        step: &NewStep,
        ctx: &ExecutionContext,
    ) -> Result<StartedCall<'a>, Error> {
        self.record(
            &step.run_id,
            ctx,
            EventPayload::ToolCallStarted {
                idempotency_key: step.idempotency_key.clone(),
                step_id: step.step_id.clone(),
                tool: step.tool.clone(),
            },
        )
        .await?;
        Ok(StartedCall {
            registered,
            call,
            _sealed: (),
        })
    }

    /// Calls the tool and checks its result. The only call of
    /// `Tool::invoke` (arch-check).
    async fn invoke_started(
        &self,
        started: StartedCall<'_>,
        ctx: &ExecutionContext,
    ) -> Result<Map<String, Value>, Error> {
        let StartedCall {
            registered, call, ..
        } = started;
        let tool = call.request.tool.as_str();
        let limit = Duration::from_secs(registered.policy.timeout_seconds.get());
        let output = bounded(ctx, limit, &format!("tool {tool}"), async {
            registered.tool.invoke(call.request, ctx).await
        })
        .await?;
        validated_result(
            output,
            self.registry.schemas(),
            &registered.result_schema,
            tool,
        )
    }

    /// Stores evidence for a result, completes the step record and appends
    /// `tool_call_completed`.
    async fn finish(
        &self,
        step: &NewStep,
        call: &ToolCall<'_>,
        ctx: &ExecutionContext,
        started: Instant,
        produced: Result<Map<String, Value>, Error>,
    ) -> Result<ToolObservation, Error> {
        let outcome = match produced {
            Ok(result) => {
                let tool_version = self.tool_version(step)?;
                let evidence = self.evidence_for(step, call, &tool_version, &result)?;
                let evidence_id = self.store_evidence(evidence).await?;
                StepOutcome::Succeeded {
                    tool_version,
                    result,
                    evidence_ids: vec![evidence_id],
                }
            }
            Err(error) => StepOutcome::Failed {
                error: with_correlation(error, ctx).to_info(),
            },
        };
        let record = self
            .steps
            .complete(&step.idempotency_key, &outcome, ctx.write_authority())
            .await?;
        let stored = record.outcome.as_ref().unwrap_or(&outcome);
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.record(
            &step.run_id,
            ctx,
            completed_event(&step.step_id, &step.tool, stored, duration_ms, None),
        )
        .await?;
        Ok(match stored {
            StepOutcome::Succeeded {
                tool_version,
                result,
                evidence_ids,
            } => ToolObservation::Succeeded {
                tool_version: tool_version.clone(),
                result: result.clone(),
                evidence_ids: evidence_ids.clone(),
                replayed: false,
            },
            StepOutcome::Failed { error } => ToolObservation::Failed {
                error: error.clone(),
                replayed: false,
            },
        })
    }

    /// The stored outcome of a completed step. Appends its
    /// `tool_call_completed` again, as the normal path builds it, from the
    /// record and marked `recovered`; if the first one was written, the log
    /// returns it and this adds nothing.
    async fn replay(
        &self,
        record: &StepRecord,
        outcome: &StepOutcome,
        ctx: &ExecutionContext,
    ) -> Result<ToolObservation, Error> {
        let duration_ms = record
            .completed_at
            .as_ref()
            .and_then(|completed| (completed.0 - record.started_at.0).to_std().ok())
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        self.record(
            &record.run_id,
            ctx,
            completed_event(
                &record.step_id,
                &record.tool,
                outcome,
                duration_ms,
                Some(true),
            ),
        )
        .await?;
        Ok(replayed(outcome))
    }

    fn tool_version(&self, step: &NewStep) -> Result<NonEmptyString, Error> {
        let version = self
            .registry
            .descriptor(step.tool.as_str())
            .map(|d| d.version.as_str())
            .unwrap_or_default();
        version.parse().map_err(|_| {
            Error::new(
                ErrorCode::Internal,
                format!("tool {} has no version", step.tool.as_str()),
            )
        })
    }

    fn evidence_for(
        &self,
        step: &NewStep,
        call: &ToolCall<'_>,
        tool_version: &NonEmptyString,
        result: &Map<String, Value>,
    ) -> Result<Evidence, Error> {
        Ok(Evidence {
            schema_version: EvidenceSchemaVersion::X01,
            id: evidence_id(&step.idempotency_key)?,
            run_id: step.run_id.clone(),
            created_at: Timestamp(self.clock.now()),
            sdr_version: call.sdr_version.clone(),
            subject_ids: Vec::new(),
            body: EvidenceBody::ToolObservation {
                tool: step.tool.clone(),
                tool_version: tool_version.clone(),
                step_id: step.step_id.clone(),
                outputs: quantities(result),
                artifact_ids: Vec::new(),
            },
        })
    }

    /// Stores `evidence`. A step re-run after an interruption may find its
    /// evidence already stored with an earlier time; that record is kept.
    async fn store_evidence(&self, evidence: Evidence) -> Result<EvidenceId, Error> {
        match self.evidence.put(&evidence).await {
            Ok(()) => Ok(evidence.id),
            Err(e) if e.code() == ErrorCode::Conflict => {
                let stored = self.evidence.get(&evidence.id).await?;
                let same = Evidence {
                    created_at: stored.created_at.clone(),
                    ..evidence.clone()
                };
                if stored == same {
                    Ok(evidence.id)
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn record(
        &self,
        run_id: &RunId,
        ctx: &ExecutionContext,
        payload: EventPayload,
    ) -> Result<(), Error> {
        let component = "tools"
            .parse()
            .map_err(|_| Error::new(ErrorCode::Internal, "invalid component id"))?;
        self.events
            .append(
                NewEvent {
                    run_id: run_id.clone(),
                    occurred_at: Timestamp(self.clock.now()),
                    correlation_id: ctx.correlation_id().clone(),
                    actor: Actor::System { component },
                    payload,
                },
                ctx.write_authority(),
            )
            .await
            .map(|_| ())
    }
}

/// Proof that the fenced `tool_call_started` append committed. Only
/// [`ToolGateway::announce_started`] makes one, and
/// [`ToolGateway::invoke_started`] needs one, so a tool is never called
/// without that record. The window after the commit is the one the
/// idempotency key covers (ADR-0026).
struct StartedCall<'a> {
    registered: &'a Registered,
    call: &'a ToolCall<'a>,
    _sealed: (),
}

fn ensure_same_request(record: &StepRecord, step: &NewStep) -> Result<(), Error> {
    if record.run_id == step.run_id
        && record.step_id == step.step_id
        && record.tool == step.tool
        && record.request_sha256 == step.request_sha256
    {
        Ok(())
    } else {
        Err(Error::new(
            ErrorCode::Conflict,
            format!(
                "idempotency key {} was used for a different request",
                step.idempotency_key.as_str()
            ),
        ))
    }
}

/// The `tool_call_completed` of a step with `outcome`: the one builder of the
/// normal path and of a replay that repairs a lost event.
fn completed_event(
    step_id: &StepId,
    tool: &ToolName,
    outcome: &StepOutcome,
    duration_ms: u64,
    recovered: Option<bool>,
) -> EventPayload {
    let (result, error, evidence_ids) = match outcome {
        StepOutcome::Succeeded { evidence_ids, .. } => {
            (ToolCallOutcome::Ok, None, evidence_ids.clone())
        }
        StepOutcome::Failed { error } => (ToolCallOutcome::Error, Some(error.clone()), Vec::new()),
    };
    EventPayload::ToolCallCompleted {
        step_id: step_id.clone(),
        tool: tool.clone(),
        outcome: result,
        error,
        evidence_ids,
        duration_ms,
        recovered,
    }
}

fn replayed(outcome: &StepOutcome) -> ToolObservation {
    match outcome {
        StepOutcome::Succeeded {
            tool_version,
            result,
            evidence_ids,
        } => ToolObservation::Succeeded {
            tool_version: tool_version.clone(),
            result: result.clone(),
            evidence_ids: evidence_ids.clone(),
            replayed: true,
        },
        StepOutcome::Failed { error } => ToolObservation::Failed {
            error: error.clone(),
            replayed: true,
        },
    }
}

fn failed(error: Error, ctx: &ExecutionContext, replayed: bool) -> ToolObservation {
    ToolObservation::Failed {
        error: with_correlation(error, ctx).to_info(),
        replayed,
    }
}

fn with_correlation(error: Error, ctx: &ExecutionContext) -> Error {
    if error.correlation_id().is_some() {
        error
    } else {
        error.with_correlation_id(ctx.correlation_id().clone())
    }
}

fn not_repeated(tool: &str) -> Error {
    Error::new(
        ErrorCode::OutcomeUnknown,
        format!(
            "an earlier attempt of this step was interrupted and its outcome is unknown; \
             {tool} is not idempotent, so it is not called again"
        ),
    )
}
