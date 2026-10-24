//! The two design tools the model sees: `sdr_read` and `sdr_propose`
//! (ADR-0030). They are not MCP tools: a tool server may not hold SDR access
//! or harness logic (blueprint sections 3.2, 13.2), so the runtime runs them
//! in process, through `SdrManager`, the only writer of design state.
//!
//! - `sdr_read` reads the task's read slice, at the head or at a version the
//!   model names. It records nothing.
//! - `sdr_propose` turns the model's request into a transition, then runs
//!   it through `SdrManager::validate`, the checks, and the commit gates.
//!   Only an `allow` commits. The model supplies the reason, `final` and the
//!   operations; the harness sets the id, the base version, the actor and
//!   the schema version. The outcome is one `sdr_transition` event, except
//!   on `ask`, when the run waits for a human and nothing is decided.
//!
//! Neither tool gets a step record: the SDR's version history records what
//! they changed.

use std::time::Duration;

use contracts::ids::{AgentId, ApprovalId, ProjectId, RunId, SdrVersion, StepId, TransitionId};
use contracts::types::{
    Actor, ApprovalAnswer, EventPayload, Evidence, EvidenceBody, GateDecision, HarnessVersions,
    NonEmptyString, SdrDocument, SdrProposeCheck, SdrProposeRequest, SdrProposeResult,
    SdrReadRequest, SdrReadResult, SdrSlice, Transition, TransitionOutcome,
    TransitionSchemaVersion, VersionedHash,
};
use contracts::{Error, ErrorCode, ExecutionContext, GateSubject};
use evaluation::Evaluator;
use governance::GateRunner;
use sdr::{CommitInfo, SdrManager, Validation};
use sha2::Digest;

use crate::cancellation::{bounded, timed};
use crate::error::RuntimeError;
use crate::events::Recorder;

pub(crate) const READ: &str = "sdr_read";
pub(crate) const PROPOSE: &str = "sdr_propose";

/// Both names; agent specs and tasks may list them beside the tools of
/// `config/tools/`.
pub(crate) const NAMES: [&str; 2] = [READ, PROPOSE];

/// The SDR tools of one run.
pub(crate) struct SdrTools<'a> {
    pub(crate) sdr: &'a SdrManager,
    pub(crate) evaluator: &'a Evaluator,
    /// The shared commit gates and the task's write scope.
    pub(crate) gates: GateRunner,
    pub(crate) recorder: Recorder<'a>,
    pub(crate) io_timeout: Duration,
    pub(crate) project: ProjectId,
    pub(crate) read_slice: SdrSlice,
    pub(crate) agent: AgentId,
}

/// What a commit records about the harness that produced it (blueprint
/// section 2.4).
pub(crate) struct Provenance<'a> {
    pub(crate) harness_build: &'a NonEmptyString,
    /// Every model that answered in the run so far.
    pub(crate) model_ids: &'a [NonEmptyString],
    /// The prompts of the context the proposal was made from.
    pub(crate) prompts: &'a [VersionedHash],
    pub(crate) agent_spec: &'a VersionedHash,
}

/// What a proposal came to.
pub(crate) enum Proposal {
    Committed {
        result: SdrProposeResult,
        /// Whether the model marked it as the design to stop on.
        final_: bool,
        /// The committed version.
        document: Box<SdrDocument>,
        /// The commit's evaluation.
        evidence: Vec<Evidence>,
    },
    Rejected {
        result: SdrProposeResult,
        evidence: Vec<Evidence>,
    },
    /// A gate asked a human. Nothing was committed or rejected.
    Ask {
        approval_id: ApprovalId,
        evidence: Vec<Evidence>,
    },
}

impl SdrTools<'_> {
    /// The task's read slice at `request.version`, or at the head. A version
    /// that does not exist is the model's mistake and comes back as an
    /// `Err` observation; a store failure is a `RuntimeError`.
    pub(crate) async fn read(
        &self,
        request: SdrReadRequest,
        ctx: &ExecutionContext,
    ) -> Result<Result<SdrReadResult, Error>, RuntimeError> {
        let head = self.head(ctx).await?;
        let version = request.version.unwrap_or_else(|| head.clone());
        if version.0 > head.0 {
            return Ok(Err(Error::new(
                ErrorCode::NotFound,
                format!(
                    "SDR version {} does not exist; the latest is {}",
                    version.0, head.0
                ),
            )
            .with_correlation_id(ctx.correlation_id().clone())));
        }
        let mut document = bounded(
            ctx,
            self.io_timeout,
            "reading the SDR",
            self.sdr
                .read(&self.project, &self.read_slice, &version, ctx),
        )
        .await?;
        if !request.ids.is_empty() {
            let wanted = |id: &str| request.ids.iter().any(|w| w.as_str() == id);
            document.requirements.retain(|r| wanted(r.id.as_str()));
            document.design.parameters.retain(|p| wanted(p.id.as_str()));
            document.design.decisions.retain(|d| wanted(d.id.as_str()));
            document
                .design
                .assumptions
                .retain(|a| wanted(a.id.as_str()));
        }
        Ok(Ok(SdrReadResult { document }))
    }

    /// Runs `request` as transition `id` on `base_version`, through
    /// validation, the checks and the gates. `approval` is a human's answer
    /// when the run resumes after this proposal asked.
    pub(crate) async fn propose(
        &self,
        request: SdrProposeRequest,
        id: TransitionId,
        base_version: SdrVersion,
        approval: Option<ApprovalAnswer>,
        provenance: &Provenance<'_>,
        ctx: &ExecutionContext,
    ) -> Result<Proposal, RuntimeError> {
        let transition = Transition {
            schema_version: TransitionSchemaVersion::X01,
            id: id.clone(),
            base_version: base_version.clone(),
            actor: Actor::Agent {
                agent_id: self.agent.clone(),
            },
            reason: request.reason,
            final_: request.final_,
            operations: request.operations,
        };
        let proposed = self.sdr.propose(self.project.clone(), transition);
        let validation = bounded(
            ctx,
            self.io_timeout,
            "validating the proposal",
            self.sdr.validate(proposed, ctx),
        )
        .await?;
        let validated = match validation {
            Validation::Valid(validated) => validated,
            Validation::Invalid(invalid) => {
                let reasons = invalid.problems.iter().map(ToString::to_string).collect();
                let result = self.rejected(&id, &base_version, reasons, &[], ctx).await?;
                return Ok(Proposal::Rejected {
                    result,
                    evidence: Vec::new(),
                });
            }
        };

        let evidence = bounded(
            ctx,
            self.io_timeout,
            "running the checks",
            self.evaluator.evaluate(&validated, ctx),
        )
        .await?;
        let decision = bounded(
            ctx,
            self.io_timeout,
            "deciding on the commit",
            self.gates.decide(
                GateSubject::SdrCommit {
                    base: validated.base(),
                    transition: validated.transition(),
                    evidence: &evidence,
                },
                approval,
                ctx,
            ),
        )
        .await?;

        match decision.outcome.decision {
            GateDecision::Allow => {
                let final_ = validated.transition().final_;
                let document = validated.document().clone();
                let info = CommitInfo {
                    evidence_ids: decision.outcome.evidence_ids,
                    harness: harness(provenance),
                };
                // Decided: commit even if the run is being cancelled. The
                // manager checks for cancellation only before it writes.
                let record = timed(
                    self.io_timeout,
                    "committing the transition",
                    self.sdr.commit(validated, info, ctx),
                )
                .await?;
                self.recorder
                    .append(
                        EventPayload::SdrTransition {
                            transition_id: id.clone(),
                            outcome: TransitionOutcome::Committed,
                            base_version: base_version.clone(),
                            new_version: Some(record.version.clone()),
                            reasons: Vec::new(),
                        },
                        ctx,
                    )
                    .await?;
                Ok(Proposal::Committed {
                    result: SdrProposeResult {
                        transition_id: id,
                        outcome: TransitionOutcome::Committed,
                        base_version,
                        new_version: Some(record.version),
                        reasons: Vec::new(),
                        checks: checks(&evidence),
                    },
                    final_,
                    document: Box::new(document),
                    evidence,
                })
            }
            GateDecision::Ask => {
                let approval_id = decision.approval_id.ok_or_else(|| {
                    RuntimeError::Internal("an ask decision without an approval id".to_owned())
                })?;
                Ok(Proposal::Ask {
                    approval_id,
                    evidence,
                })
            }
            GateDecision::Deny | GateDecision::Retry | GateDecision::RejectTransition => {
                let rejection = self.sdr.reject(validated, decision.outcome.reasons);
                let result = self
                    .rejected(&id, &base_version, rejection.reasons, &evidence, ctx)
                    .await?;
                Ok(Proposal::Rejected { result, evidence })
            }
        }
    }

    async fn head(&self, ctx: &ExecutionContext) -> Result<SdrVersion, RuntimeError> {
        bounded(
            ctx,
            self.io_timeout,
            "reading the SDR head",
            self.sdr.head(&self.project, ctx),
        )
        .await?
        .ok_or_else(|| RuntimeError::Component {
            operation: "reading the SDR head",
            source: Error::new(
                ErrorCode::NotFound,
                format!("project {} has no SDR", self.project.as_str()),
            ),
        })
    }

    /// Records a rejection and returns its result.
    async fn rejected(
        &self,
        id: &TransitionId,
        base_version: &SdrVersion,
        reasons: Vec<String>,
        evidence: &[Evidence],
        ctx: &ExecutionContext,
    ) -> Result<SdrProposeResult, RuntimeError> {
        let reasons: Vec<NonEmptyString> = reasons.iter().filter_map(|r| r.parse().ok()).collect();
        self.recorder
            .append(
                EventPayload::SdrTransition {
                    transition_id: id.clone(),
                    outcome: TransitionOutcome::Rejected,
                    base_version: base_version.clone(),
                    new_version: None,
                    reasons: reasons.clone(),
                },
                ctx,
            )
            .await?;
        Ok(SdrProposeResult {
            transition_id: id.clone(),
            outcome: TransitionOutcome::Rejected,
            base_version: base_version.clone(),
            new_version: None,
            reasons,
            checks: checks(evidence),
        })
    }
}

fn harness(provenance: &Provenance<'_>) -> HarnessVersions {
    HarnessVersions {
        harness_build: provenance.harness_build.clone(),
        model_ids: provenance.model_ids.to_vec(),
        prompts: provenance.prompts.to_vec(),
        skills: Vec::new(),
        agent_specs: vec![provenance.agent_spec.clone()],
    }
}

/// The check findings of an evaluation, as the model sees them.
fn checks(evidence: &[Evidence]) -> Vec<SdrProposeCheck> {
    evidence
        .iter()
        .filter_map(|e| match &e.body {
            EvidenceBody::Check {
                check_id,
                verdict,
                message,
                ..
            } => Some(SdrProposeCheck {
                evidence_id: e.id.clone(),
                check_id: check_id.clone(),
                verdict: *verdict,
                subject_ids: e.subject_ids.clone(),
                message: message.clone(),
            }),
            EvidenceBody::ToolObservation { .. } => None,
        })
        .collect()
}

/// `tr-` and 32 hex digits of a hash of the run and the tool call's step:
/// the same call always proposes the same transition id, so a resumed or
/// recovered run can tell whether it already committed.
pub(crate) fn transition_id(run: &RunId, call: &StepId) -> Result<TransitionId, RuntimeError> {
    let digest = sha2::Sha256::digest(format!("{}\u{1f}{}", run.as_str(), call.as_str()));
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("tr-{hex}")
        .parse()
        .map_err(|_| RuntimeError::Internal("a generated transition id is invalid".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versioned(id: &str) -> VersionedHash {
        VersionedHash {
            id: id.parse().expect("id"),
            version: Some("0.1.0".parse().expect("version")),
            sha256: format!("sha256:{}", "a".repeat(64)).parse().expect("hash"),
        }
    }

    #[test]
    fn a_commit_records_the_build_the_models_the_prompts_and_the_agent_spec() {
        let build: NonEmptyString = "build-7".parse().expect("text");
        let models: Vec<NonEmptyString> = vec!["claude-haiku-5-5".parse().expect("text")];
        let prompts = [versioned("system.base"), versioned("orchestrator.main")];
        let spec = versioned("main_orchestrator");
        let recorded = harness(&Provenance {
            harness_build: &build,
            model_ids: &models,
            prompts: &prompts,
            agent_spec: &spec,
        });
        assert_eq!(recorded.harness_build, build);
        assert_eq!(recorded.model_ids, models);
        assert_eq!(recorded.prompts, prompts.to_vec());
        assert_eq!(recorded.agent_specs, vec![spec]);
        assert!(recorded.skills.is_empty());
    }

    #[test]
    fn transition_ids_are_stable_per_run_and_call() {
        let run: RunId = "run-1".parse().expect("id");
        let other: RunId = "run-2".parse().expect("id");
        let call: StepId = "step-3.1".parse().expect("id");
        let next: StepId = "step-3.2".parse().expect("id");
        let id = transition_id(&run, &call).expect("id");
        assert!(id.as_str().starts_with("tr-") && id.as_str().len() == 35);
        assert_eq!(id, transition_id(&run, &call).expect("id"));
        assert_ne!(id, transition_id(&run, &next).expect("id"));
        assert_ne!(id, transition_id(&other, &call).expect("id"));
    }
}
