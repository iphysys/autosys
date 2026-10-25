use std::collections::HashSet;
use std::sync::Arc;

use contracts::ids::{EvidenceId, RunId};
use contracts::types::{
    Actor, EventPayload, Evidence, EvidenceBody, EvidenceSchemaVersion, Timestamp, Verdict,
};
use contracts::{
    Check, CheckFinding, CheckSubject, Clock, Error, ErrorCode, EventLog, EvidenceStore,
    ExecutionContext, NewEvent,
};
use sdr::ValidatedTransition;
use sha2::Digest;

/// Runs the configured checks on a validated transition and turns every
/// finding into a stored evidence record, with a `check_completed` event.
///
/// Checks evaluate the document the transition would commit. The evidence
/// records the transition's base version as its `sdr_version`: the would-be
/// version may never exist if a gate rejects the transition.
pub struct Evaluator {
    checks: Vec<Arc<dyn Check>>,
    evidence: Arc<dyn EvidenceStore>,
    events: Arc<dyn EventLog>,
    clock: Arc<dyn Clock>,
}

impl Evaluator {
    /// InvalidInput if two checks share an id.
    pub fn new(
        checks: Vec<Arc<dyn Check>>,
        evidence: Arc<dyn EvidenceStore>,
        events: Arc<dyn EventLog>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, Error> {
        let mut seen = HashSet::new();
        for check in &checks {
            let id = check.descriptor().id.as_str();
            if !seen.insert(id.to_owned()) {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    format!("check {id} is registered twice"),
                ));
            }
        }
        Ok(Self {
            checks,
            evidence,
            events,
            clock,
        })
    }

    /// The evidence for `validated`, in check order. A check that fails to
    /// run yields evidence with verdict `error`, so the failure is recorded
    /// rather than mistaken for a pass. Needs a run in `ctx`.
    pub async fn evaluate(
        &self,
        validated: &ValidatedTransition,
        ctx: &ExecutionContext,
    ) -> Result<Vec<Evidence>, Error> {
        let run_id = ctx.run_id().cloned().ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidInput,
                "evaluation needs a run in the context",
            )
        })?;
        let subject = CheckSubject::Document(validated.document());
        let mut all = Vec::new();
        for check in &self.checks {
            ctx.ensure_not_cancelled()?;
            let descriptor = check.descriptor();
            let findings = match check.evaluate(subject, ctx).await {
                Ok(findings) => findings,
                Err(e) if e.code() == ErrorCode::Cancelled => return Err(e),
                Err(e) => vec![CheckFinding {
                    subject_ids: Vec::new(),
                    inputs: Vec::new(),
                    verdict: Verdict::Error,
                    message: Some(format!("the check failed to run: {e}")),
                }],
            };
            let transition_id = validated.transition().id.as_str();
            for (index, found) in findings.into_iter().enumerate() {
                let now = self.clock.now();
                let id = evidence_id(&[
                    run_id.as_str(),
                    transition_id,
                    descriptor.id.as_str(),
                    &index.to_string(),
                    &now.to_rfc3339(),
                ])?;
                let record = Evidence {
                    schema_version: EvidenceSchemaVersion::X01,
                    id,
                    run_id: run_id.clone(),
                    created_at: Timestamp(now),
                    sdr_version: validated.base().version.clone(),
                    subject_ids: found.subject_ids,
                    body: EvidenceBody::Check {
                        check_id: descriptor.id.clone(),
                        check_version: descriptor.version.clone(),
                        method: descriptor.method,
                        inputs: found.inputs,
                        verdict: found.verdict,
                        message: found.message.and_then(|m| m.parse().ok()),
                    },
                };
                self.evidence.put(&record).await?;
                self.record_completion(&run_id, &record, ctx).await?;
                all.push(record);
            }
        }
        Ok(all)
    }

    async fn record_completion(
        &self,
        run_id: &RunId,
        record: &Evidence,
        ctx: &ExecutionContext,
    ) -> Result<(), Error> {
        let EvidenceBody::Check {
            check_id, verdict, ..
        } = &record.body
        else {
            return Ok(());
        };
        self.events
            .append(
                NewEvent {
                    run_id: run_id.clone(),
                    occurred_at: record.created_at.clone(),
                    correlation_id: ctx.correlation_id().clone(),
                    actor: system_actor()?,
                    payload: EventPayload::CheckCompleted {
                        check_id: check_id.clone(),
                        evidence_id: record.id.clone(),
                        verdict: *verdict,
                    },
                },
                ctx.write_authority(),
            )
            .await
            .map(|_| ())
    }
}

fn system_actor() -> Result<Actor, Error> {
    Ok(Actor::System {
        component: "evaluation"
            .parse()
            .map_err(|_| Error::new(ErrorCode::Internal, "invalid component id"))?,
    })
}

/// `ev-` and 32 hex digits of a hash over `parts`: unique per run,
/// transition, check, finding and time.
fn evidence_id(parts: &[&str]) -> Result<EvidenceId, Error> {
    let digest = sha2::Sha256::digest(parts.join("\u{1f}").as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("ev-{hex}")
        .parse()
        .map_err(|_| Error::new(ErrorCode::Internal, "generated evidence id is invalid"))
}
