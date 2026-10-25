//! The capability router: turns the model's tool calls into calls on the
//! capability that serves each one, and the outcome into the tool result
//! the model sees next (ADR-0030).
//!
//! - The model gets a definition of each tool the task lists, built from
//!   the tool's canonical request schema.
//! - A name the task does not list is denied without running anything, and
//!   an `error` event (`permission_denied`) records it.
//! - `sdr_read` and `sdr_propose` run in process ([`crate::sdr_tools`]);
//!   every other tool goes through the tool gateway, which validates,
//!   gates, runs and records it.
//! - A result's content follows `schemas/tools/tool-result.schema.json` and
//!   is passed raw; the context builder wraps it as untrusted.

use contracts::ids::{ApprovalId, EvidenceId, Identifier, RunId, SdrVersion, StepId, ToolName};
use contracts::types::{
    ApprovalAnswer, Evidence, NonEmptyString, SdrDocument, SdrProposeRequest, SdrReadRequest, Task,
    ToolResultContent,
};
use contracts::{ContentBlock, Error, ErrorCode, ExecutionContext, ToolRequest, ToolSpec};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use sha2::Digest;
use tools::{ToolCall, ToolGateway, ToolObservation};

use crate::error::RuntimeError;
use crate::events::Recorder;
use crate::sdr_tools::{self, Proposal, Provenance, SdrTools};
use crate::tool_schemas::{PROPOSE_REQUEST, READ_REQUEST, ToolSchemas};

/// One tool call the model made.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ToolCallBlock {
    /// The model's id for the call, which its result answers.
    pub(crate) id: String,
    pub(crate) name: ToolName,
    pub(crate) arguments: Map<String, Value>,
}

/// What routing a call came to.
pub(crate) enum Routed {
    Answered(Answer),
    /// A gate asked a human; the call waits for the answer.
    Ask {
        approval_id: ApprovalId,
        evidence_ids: Vec<EvidenceId>,
    },
}

/// The result the model gets, and what the call means for the run.
pub(crate) struct Answer {
    pub(crate) content: ToolResultContent,
    pub(crate) is_error: bool,
    pub(crate) effect: Effect,
    /// Evidence the call produced: a tool's observation, or a proposal's
    /// check findings.
    pub(crate) evidence_ids: Vec<EvidenceId>,
}

pub(crate) enum Effect {
    /// A result, and nothing changed.
    Observed,
    /// The call failed or was refused; one failure toward the repair bound.
    Failed,
    /// A proposal committed.
    Committed(Box<Commit>),
}

/// A committed proposal, with what the success rule needs.
pub(crate) struct Commit {
    pub(crate) transition_id: contracts::ids::TransitionId,
    pub(crate) version: SdrVersion,
    pub(crate) final_: bool,
    pub(crate) document: SdrDocument,
    pub(crate) evidence: Vec<Evidence>,
}

/// What the model step a call belongs to contributes.
pub(crate) struct StepInfo<'a> {
    /// The SDR version of the step's context: a proposal's base version,
    /// and the version tool evidence is recorded against.
    pub(crate) sdr_version: SdrVersion,
    pub(crate) provenance: Provenance<'a>,
}

pub(crate) struct CapabilityRouter<'a> {
    pub(crate) task: &'a Task,
    pub(crate) tools: &'a ToolGateway,
    pub(crate) sdr: SdrTools<'a>,
    pub(crate) schemas: &'a ToolSchemas,
    pub(crate) recorder: Recorder<'a>,
}

impl CapabilityRouter<'_> {
    /// A definition of each tool the task lists, in the task's order.
    pub(crate) fn tool_specs(&self) -> Result<Vec<ToolSpec>, RuntimeError> {
        let mut specs = Vec::new();
        let mut problems = Vec::new();
        for name in &self.task.tools {
            let schema = match name.as_str() {
                sdr_tools::READ => Some(READ_REQUEST.to_owned()),
                sdr_tools::PROPOSE => Some(PROPOSE_REQUEST.to_owned()),
                other => self
                    .tools
                    .registry()
                    .policy(other)
                    .and_then(|p| p.request_schema.as_ref())
                    .map(|s| s.as_str().to_owned()),
            };
            match schema {
                Some(schema) => specs.push(self.schemas.spec(name, &schema)?),
                None => problems.push(format!(
                    "tool `{}` is not available from the tool gateway; available: {}",
                    name.as_str(),
                    self.tools.registry().available().join(", ")
                )),
            }
        }
        if problems.is_empty() {
            Ok(specs)
        } else {
            Err(RuntimeError::InvalidTask {
                task: self.task.id.as_str().to_owned(),
                problems,
            })
        }
    }

    /// Runs `call` as tool call `call_step` of the step `step`. `ctx` is the
    /// call's context, a child of the step's. `approval` is a human's answer
    /// when the run resumes after this call asked.
    pub(crate) async fn route(
        &self,
        call: &ToolCallBlock,
        call_step: &StepId,
        step: &StepInfo<'_>,
        approval: Option<ApprovalAnswer>,
        ctx: &ExecutionContext,
    ) -> Result<Routed, RuntimeError> {
        let name = call.name.as_str();
        if !self.task.tools.contains(&call.name) {
            return self.not_allowed(name, ctx).await;
        }
        match name {
            sdr_tools::READ => self.read(call, call_step, ctx).await,
            sdr_tools::PROPOSE => self.propose(call, call_step, step, approval, ctx).await,
            _ => self.tool(call, call_step, step, approval, ctx).await,
        }
    }

    async fn not_allowed(
        &self,
        name: &str,
        ctx: &ExecutionContext,
    ) -> Result<Routed, RuntimeError> {
        let allowed: Vec<&str> = self.task.tools.iter().map(|t| t.as_str()).collect();
        let error = Error::new(
            ErrorCode::PermissionDenied,
            format!(
                "tool {name} is not allowed by the task; allowed: {}",
                allowed.join(", ")
            ),
        )
        .with_correlation_id(ctx.correlation_id().clone());
        self.recorder.error(error.to_info(), ctx).await?;
        Ok(answered(
            ToolResultContent::Denied {
                reasons: non_empty(&[error.message().to_owned()]),
            },
            Effect::Failed,
            Vec::new(),
        ))
    }

    async fn read(
        &self,
        call: &ToolCallBlock,
        call_step: &StepId,
        ctx: &ExecutionContext,
    ) -> Result<Routed, RuntimeError> {
        let request: SdrReadRequest = match self.arguments(READ_REQUEST, call, ctx)? {
            Ok(request) => request,
            Err(failed) => return Ok(failed),
        };
        Ok(match self.sdr.read(request, ctx).await? {
            Ok(result) => answered(
                ToolResultContent::Ok {
                    step_id: call_step.clone(),
                    result: to_map(&result)?,
                    evidence_ids: Vec::new(),
                },
                Effect::Observed,
                Vec::new(),
            ),
            Err(error) => answered(
                ToolResultContent::Error {
                    error: error.to_info(),
                },
                Effect::Failed,
                Vec::new(),
            ),
        })
    }

    async fn propose(
        &self,
        call: &ToolCallBlock,
        call_step: &StepId,
        step: &StepInfo<'_>,
        approval: Option<ApprovalAnswer>,
        ctx: &ExecutionContext,
    ) -> Result<Routed, RuntimeError> {
        let request: SdrProposeRequest = match self.arguments(PROPOSE_REQUEST, call, ctx)? {
            Ok(request) => request,
            Err(failed) => return Ok(failed),
        };
        let run = run_id(ctx)?;
        let id = sdr_tools::transition_id(&run, call_step)?;
        let proposal = self
            .sdr
            .propose(
                request,
                id,
                step.sdr_version.clone(),
                approval,
                &step.provenance,
                ctx,
            )
            .await?;
        Ok(match proposal {
            Proposal::Committed {
                result,
                final_,
                document,
                evidence,
            } => {
                let evidence_ids = ids(&evidence);
                let version = result.new_version.clone().ok_or_else(|| {
                    RuntimeError::Internal("a commit without a new version".to_owned())
                })?;
                let commit = Commit {
                    transition_id: result.transition_id.clone(),
                    version,
                    final_,
                    document: *document,
                    evidence,
                };
                answered(
                    ToolResultContent::Ok {
                        step_id: call_step.clone(),
                        result: to_map(&result)?,
                        evidence_ids: evidence_ids.clone(),
                    },
                    Effect::Committed(Box::new(commit)),
                    evidence_ids,
                )
            }
            Proposal::Rejected { result, evidence } => {
                let evidence_ids = ids(&evidence);
                answered(
                    ToolResultContent::Ok {
                        step_id: call_step.clone(),
                        result: to_map(&result)?,
                        evidence_ids: evidence_ids.clone(),
                    },
                    Effect::Failed,
                    evidence_ids,
                )
            }
            Proposal::Ask {
                approval_id,
                evidence,
            } => Routed::Ask {
                approval_id,
                evidence_ids: ids(&evidence),
            },
        })
    }

    async fn tool(
        &self,
        call: &ToolCallBlock,
        call_step: &StepId,
        step: &StepInfo<'_>,
        approval: Option<ApprovalAnswer>,
        ctx: &ExecutionContext,
    ) -> Result<Routed, RuntimeError> {
        let run = run_id(ctx)?;
        let request = ToolRequest {
            tool: call.name.clone(),
            step_id: call_step.clone(),
            idempotency_key: idempotency_key(&run, call_step)?,
            arguments: call.arguments.clone(),
        };
        let observation = self
            .tools
            .call(
                ToolCall {
                    request: &request,
                    agent_id: &self.sdr.agent,
                    sdr_version: step.sdr_version.clone(),
                    approval,
                },
                ctx,
            )
            .await
            .map_err(|source| RuntimeError::Component {
                operation: "calling a tool",
                source,
            })?;
        Ok(match observation {
            ToolObservation::Succeeded {
                result,
                evidence_ids,
                ..
            } => answered(
                ToolResultContent::Ok {
                    step_id: call_step.clone(),
                    result,
                    evidence_ids: evidence_ids.clone(),
                },
                Effect::Observed,
                evidence_ids,
            ),
            ToolObservation::Failed { error, .. } => answered(
                ToolResultContent::Error { error },
                Effect::Failed,
                Vec::new(),
            ),
            ToolObservation::Denied { reasons } => answered(
                ToolResultContent::Denied {
                    reasons: non_empty(&reasons),
                },
                Effect::Failed,
                Vec::new(),
            ),
            ToolObservation::ApprovalRequired { approval_id, .. } => Routed::Ask {
                approval_id,
                evidence_ids: Vec::new(),
            },
        })
    }

    /// The call's arguments as `T`, after checking them against the SDR
    /// tool's request schema. Arguments that break it are the model's
    /// mistake: an `invalid_input` error result, never a harness failure.
    fn arguments<T: DeserializeOwned>(
        &self,
        schema: &str,
        call: &ToolCallBlock,
        ctx: &ExecutionContext,
    ) -> Result<Result<T, Routed>, RuntimeError> {
        let invalid = |message: String| {
            let error = Error::new(ErrorCode::InvalidInput, message)
                .with_correlation_id(ctx.correlation_id().clone());
            answered(
                ToolResultContent::Error {
                    error: error.to_info(),
                },
                Effect::Failed,
                Vec::new(),
            )
        };
        let problems = self.schemas.argument_problems(schema, &call.arguments)?;
        if !problems.is_empty() {
            return Ok(Err(invalid(format!(
                "arguments for {} break {schema}: {}",
                call.name.as_str(),
                problems.join("; ")
            ))));
        }
        Ok(
            serde_json::from_value(Value::Object(call.arguments.clone())).map_err(|e| {
                invalid(format!(
                    "arguments for {} do not match {schema}: {e}",
                    call.name.as_str()
                ))
            }),
        )
    }
}

fn answered(content: ToolResultContent, effect: Effect, evidence_ids: Vec<EvidenceId>) -> Routed {
    let is_error = !matches!(effect, Effect::Observed | Effect::Committed(_));
    Routed::Answered(Answer {
        content,
        is_error,
        effect,
        evidence_ids,
    })
}

/// The block that answers tool call `call_id`, with the content as JSON.
pub(crate) fn tool_result(
    call_id: &str,
    content: &ToolResultContent,
    is_error: bool,
) -> Result<ContentBlock, RuntimeError> {
    let content = serde_json::to_string(content)
        .map_err(|e| RuntimeError::Internal(format!("cannot serialize a tool result: {e}")))?;
    Ok(ContentBlock::ToolResult {
        call_id: call_id.to_owned(),
        content,
        is_error,
    })
}

/// `step-{step}.{call}`: the `call`th tool call of model step `step`,
/// counting from 1.
pub(crate) fn call_step_id(step: u64, call: usize) -> Result<StepId, RuntimeError> {
    format!("step-{step}.{call}")
        .parse()
        .map_err(|_| RuntimeError::Internal("a generated step id is invalid".to_owned()))
}

/// `{run}:{step}`, or `sha256:` and the hex digest of that when it is longer
/// than an identifier may be. A retry is a later step, so it gets a new key
/// (ADR-0026).
pub(crate) fn idempotency_key(run: &RunId, step: &StepId) -> Result<Identifier, RuntimeError> {
    let key = format!("{}:{}", run.as_str(), step.as_str());
    let key = if key.len() <= 128 {
        key
    } else {
        let hex: String = sha2::Sha256::digest(key.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("sha256:{hex}")
    };
    key.parse()
        .map_err(|_| RuntimeError::Internal("a generated idempotency key is invalid".to_owned()))
}

fn run_id(ctx: &ExecutionContext) -> Result<RunId, RuntimeError> {
    ctx.run_id()
        .cloned()
        .ok_or_else(|| RuntimeError::Internal("a tool call without a run".to_owned()))
}

fn ids(evidence: &[Evidence]) -> Vec<EvidenceId> {
    evidence.iter().map(|e| e.id.clone()).collect()
}

fn non_empty(reasons: &[String]) -> Vec<NonEmptyString> {
    reasons.iter().filter_map(|r| r.parse().ok()).collect()
}

fn to_map(value: &impl Serialize) -> Result<Map<String, Value>, RuntimeError> {
    match serde_json::to_value(value) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(RuntimeError::Internal(
            "a tool result is not an object".to_owned(),
        )),
        Err(e) => Err(RuntimeError::Internal(format!(
            "cannot serialize a tool result: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_ids_and_idempotency_keys_are_deterministic() {
        let run: RunId = "run-1".parse().expect("id");
        let call = call_step_id(4, 2).expect("id");
        assert_eq!(call.as_str(), "step-4.2");
        assert_eq!(
            idempotency_key(&run, &call).expect("key").as_str(),
            "run-1:step-4.2"
        );

        let long: RunId = format!("run-{}", "x".repeat(120)).parse().expect("id");
        let key = idempotency_key(&long, &call).expect("key");
        assert!(key.as_str().starts_with("sha256:"), "{}", key.as_str());
        assert_eq!(key, idempotency_key(&long, &call).expect("key"));
        let next = call_step_id(4, 3).expect("id");
        assert_ne!(key, idempotency_key(&long, &next).expect("key"));
    }

    #[test]
    fn a_result_block_holds_the_content_as_json() {
        let content = ToolResultContent::Denied {
            reasons: non_empty(&["not allowed".to_owned()]),
        };
        let block = tool_result("toolu_1", &content, true).expect("block");
        let ContentBlock::ToolResult {
            call_id,
            content: text,
            is_error,
        } = block
        else {
            panic!("not a tool result");
        };
        assert_eq!(call_id, "toolu_1");
        assert!(is_error);
        assert_eq!(text, r#"{"status":"denied","reasons":["not allowed"]}"#);
    }
}
