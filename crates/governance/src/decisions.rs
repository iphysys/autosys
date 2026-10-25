//! Runs the gates for a subject, records every decision in the event log, and
//! combines them into one.

use std::collections::HashSet;
use std::sync::Arc;

use contracts::ids::{ApprovalId, EvidenceId, Identifier, JobId, RunId};
use contracts::types::{Actor, ApprovalAnswer, EventPayload, GateDecision, Sha256, Timestamp};
use contracts::{
    Clock, Error, ErrorCode, EventLog, ExecutionContext, Gate, GateOutcome, GateRequest,
    GateSubject, NewEvent, canonical_sha256,
};
use sha2::Digest;

/// The combined decision on one subject.
#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub outcome: GateOutcome,
    /// Set when the decision is `ask`: the approval the run now waits for.
    pub approval_id: Option<ApprovalId>,
}

/// Runs every gate registered for a subject's kind, in registration order.
///
/// - Each gate's decision is written to the event log as `gate_decided`.
/// - A gate that fails counts as `deny`: gates fail closed (blueprint 2.8).
/// - The combined decision is the most restrictive: `deny`, then
///   `reject_transition`, `retry`, `ask`, `allow`. With no gate for the
///   subject's kind, it is `deny`.
/// - On `ask`, an `approval_requested` event is written. The approval id is
///   derived from the job, the step and the subject's content (ADR-0031):
///   a step run again after a crash, or a resumed run, asks for the same
///   approval, and a different proposal for a new one.
pub struct GateRunner {
    gates: Vec<Arc<dyn Gate>>,
    events: Arc<dyn EventLog>,
    clock: Arc<dyn Clock>,
}

impl GateRunner {
    /// InvalidInput if two gates share an id.
    pub fn new(
        gates: Vec<Arc<dyn Gate>>,
        events: Arc<dyn EventLog>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, Error> {
        let mut seen = HashSet::new();
        for gate in &gates {
            if !seen.insert(gate.id().as_str().to_owned()) {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    format!("gate {} is registered twice", gate.id().as_str()),
                ));
            }
        }
        Ok(Self {
            gates,
            events,
            clock,
        })
    }

    /// Decides on `subject`. `approval` is a human answer already given for
    /// it, when the run resumes after asking. Needs a run in `ctx`.
    pub async fn decide(
        &self,
        subject: GateSubject<'_>,
        approval: Option<ApprovalAnswer>,
        ctx: &ExecutionContext,
    ) -> Result<Decision, Error> {
        let run_id = ctx.run_id().cloned().ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidInput,
                "gate decisions need a run in the context",
            )
        })?;
        let subject_id = subject_id(&subject)?;
        let mut decided: Vec<(Identifier, GateOutcome)> = Vec::new();
        for gate in self
            .gates
            .iter()
            .filter(|g| g.subject_kind() == subject.kind())
        {
            ctx.ensure_not_cancelled()?;
            let request = GateRequest { subject, approval };
            let gate_outcome = match gate.decide(request, ctx).await {
                Ok(o) => o,
                Err(e) if e.code() == ErrorCode::Cancelled => return Err(e),
                Err(e) => GateOutcome {
                    decision: GateDecision::Deny,
                    reasons: vec![format!("the gate failed, so it denies: {e}")],
                    evidence_ids: Vec::new(),
                },
            };
            self.record(
                &run_id,
                ctx,
                EventPayload::GateDecided {
                    gate: gate.id().clone(),
                    subject_id: subject_id.clone(),
                    decision: gate_outcome.decision,
                    reasons: non_empty(&gate_outcome.reasons),
                    evidence_ids: gate_outcome.evidence_ids.clone(),
                },
            )
            .await?;
            decided.push((gate.id().clone(), gate_outcome));
        }

        let combined = combine(&decided);
        let approval_id = if combined.decision == GateDecision::Ask {
            let asking = decided
                .iter()
                .find(|(_, o)| o.decision == GateDecision::Ask)
                .map(|(id, _)| id.clone())
                .ok_or_else(|| Error::new(ErrorCode::Internal, "ask without an asking gate"))?;
            let job_id = ctx.job_id().ok_or_else(|| {
                Error::new(
                    ErrorCode::Internal,
                    "an approval needs a job in the context",
                )
            })?;
            let approval_id = approval_id(job_id, &subject_id, &subject_sha256(&subject)?)?;
            let summary = combined.reasons.join("; ");
            self.record(
                &run_id,
                ctx,
                EventPayload::ApprovalRequested {
                    approval_id: approval_id.clone(),
                    gate: asking,
                    subject_id: subject_id.clone(),
                    summary: summary
                        .parse()
                        .or_else(|_| "approval required".parse())
                        .map_err(|_| Error::new(ErrorCode::Internal, "invalid approval summary"))?,
                },
            )
            .await?;
            Some(approval_id)
        } else {
            None
        };
        Ok(Decision {
            outcome: combined,
            approval_id,
        })
    }

    async fn record(
        &self,
        run_id: &RunId,
        ctx: &ExecutionContext,
        payload: EventPayload,
    ) -> Result<(), Error> {
        self.events
            .append(
                NewEvent {
                    run_id: run_id.clone(),
                    occurred_at: Timestamp(self.clock.now()),
                    correlation_id: ctx.correlation_id().clone(),
                    actor: Actor::System {
                        component: "governance"
                            .parse()
                            .map_err(|_| Error::new(ErrorCode::Internal, "invalid component id"))?,
                    },
                    payload,
                },
                ctx.write_authority(),
            )
            .await
            .map(|_| ())
    }
}

/// How restrictive a decision is; higher wins.
fn severity(decision: GateDecision) -> u8 {
    match decision {
        GateDecision::Allow => 0,
        GateDecision::Ask => 1,
        GateDecision::Retry => 2,
        GateDecision::RejectTransition => 3,
        GateDecision::Deny => 4,
    }
}

/// The most restrictive decision, with the reasons of every gate that made
/// it and the evidence of all gates.
fn combine(decided: &[(Identifier, GateOutcome)]) -> GateOutcome {
    let Some(decision) = decided
        .iter()
        .map(|(_, o)| o.decision)
        .max_by_key(|d| severity(*d))
    else {
        return GateOutcome {
            decision: GateDecision::Deny,
            reasons: vec!["no gate decides this kind of subject".to_owned()],
            evidence_ids: Vec::new(),
        };
    };
    let reasons = decided
        .iter()
        .filter(|(_, o)| o.decision == decision)
        .flat_map(|(id, o)| {
            o.reasons
                .iter()
                .map(move |r| format!("{}: {r}", id.as_str()))
        })
        .collect();
    let mut seen = HashSet::new();
    let evidence_ids: Vec<EvidenceId> = decided
        .iter()
        .flat_map(|(_, o)| o.evidence_ids.iter())
        .filter(|id| seen.insert(id.as_str().to_owned()))
        .cloned()
        .collect();
    GateOutcome {
        decision,
        reasons,
        evidence_ids,
    }
}

fn subject_id(subject: &GateSubject<'_>) -> Result<Identifier, Error> {
    let id = match subject {
        GateSubject::SdrCommit { transition, .. } => transition.id.as_str(),
        GateSubject::ToolCall { request, .. } => request.step_id.as_str(),
    };
    id.parse()
        .map_err(|_| Error::new(ErrorCode::Internal, format!("invalid subject id {id}")))
}

/// The canonical sha256 of what is decided on: the tool's name and
/// arguments, or the whole transition.
fn subject_sha256(subject: &GateSubject<'_>) -> Result<Sha256, Error> {
    let value = match subject {
        GateSubject::ToolCall { request, .. } => serde_json::json!({
            "tool": request.tool.as_str(),
            "arguments": request.arguments,
        }),
        GateSubject::SdrCommit { transition, .. } => serde_json::to_value(transition)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("a transition: {e}")))?,
    };
    canonical_sha256(&value)
}

/// `appr-` and 32 hex digits of a hash of the job, the subject (a tool call's
/// step id, or a transition id, which derives from the step that proposed
/// it) and the subject's content.
fn approval_id(job: &JobId, subject: &Identifier, content: &Sha256) -> Result<ApprovalId, Error> {
    let digest = sha2::Sha256::digest(format!(
        "{}\u{1f}{}\u{1f}{}",
        job.as_str(),
        subject.as_str(),
        content.as_str()
    ));
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("appr-{hex}")
        .parse()
        .map_err(|_| Error::new(ErrorCode::Internal, "generated approval id is invalid"))
}

fn non_empty(reasons: &[String]) -> Vec<contracts::types::NonEmptyString> {
    reasons.iter().filter_map(|r| r.parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decided(decision: GateDecision, gate: &str, evidence: &[&str]) -> (Identifier, GateOutcome) {
        (
            gate.parse().unwrap(),
            GateOutcome {
                decision,
                reasons: vec![format!("{gate} says so")],
                evidence_ids: evidence.iter().map(|e| e.parse().unwrap()).collect(),
            },
        )
    }

    #[test]
    fn the_most_restrictive_decision_wins() {
        use GateDecision::*;
        let order = [Allow, Ask, Retry, RejectTransition, Deny];
        for (i, lower) in order.iter().enumerate() {
            for higher in &order[i..] {
                let combined = combine(&[decided(*lower, "a", &[]), decided(*higher, "b", &[])]);
                assert_eq!(combined.decision, *higher, "{lower:?} vs {higher:?}");
            }
        }
    }

    #[test]
    fn reasons_come_from_the_deciding_gates_and_evidence_from_all() {
        let combined = combine(&[
            decided(GateDecision::Allow, "a", &["ev-1"]),
            decided(GateDecision::RejectTransition, "b", &["ev-2", "ev-1"]),
        ]);
        assert_eq!(combined.reasons, vec!["b: b says so"]);
        let ids: Vec<&str> = combined.evidence_ids.iter().map(|e| e.as_str()).collect();
        assert_eq!(ids, ["ev-1", "ev-2"]);
    }

    #[test]
    fn no_gate_means_deny() {
        assert_eq!(combine(&[]).decision, GateDecision::Deny);
    }

    #[test]
    fn approval_ids_follow_the_job_the_subject_and_its_content() {
        let job: JobId = "job-1".parse().unwrap();
        let other_job: JobId = "job-2".parse().unwrap();
        let tr1: Identifier = "tr-1".parse().unwrap();
        let tr2: Identifier = "tr-2".parse().unwrap();
        let content: Sha256 = format!("sha256:{}", "a".repeat(64)).parse().unwrap();
        let changed: Sha256 = format!("sha256:{}", "b".repeat(64)).parse().unwrap();
        let id = |job, subject, content| approval_id(job, subject, content).unwrap();
        assert_eq!(
            id(&job, &tr1, &content),
            id(&job, &tr1, &content),
            "a re-run asks again"
        );
        assert_ne!(
            id(&job, &tr1, &content),
            id(&job, &tr2, &content),
            "another step"
        );
        assert_ne!(
            id(&job, &tr1, &content),
            id(&job, &tr1, &changed),
            "another proposal"
        );
        assert_ne!(
            id(&job, &tr1, &content),
            id(&other_job, &tr1, &content),
            "another job"
        );
    }
}
