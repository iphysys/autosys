//! When a run may stop with `success`, what is still unmet otherwise, and
//! the result a run returns (blueprint section 5.4, ADR-0030).
//!
//! A run succeeds right after a `final: true` transition commits through
//! every commit gate, approval included, provided every requirement the run
//! must satisfy has at least one finding from that commit's checks and all
//! of those findings are `pass`. The requirements it must satisfy are the
//! must-level ones of the committed version and every id the task names.
//! `unverified` never counts, and a model's claim counts for nothing.

use contracts::ids::{ElementId, SdrVersion};
use contracts::types::{
    ErrorInfo, Evidence, EvidenceBody, Job, NonEmptyString, Requirement, RequirementPriority,
    StopReason, TaskResult, TaskResultSchemaVersion, TransitionId, Verdict,
};

/// The must-level requirements of `requirements`, then the task's other
/// requirement ids, each once.
pub(crate) fn required(requirements: &[Requirement], task_ids: &[ElementId]) -> Vec<ElementId> {
    let mut ids: Vec<ElementId> = requirements
        .iter()
        .filter(|r| r.priority == RequirementPriority::Must)
        .map(|r| r.id.clone())
        .collect();
    for id in task_ids {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    ids
}

/// The ids in `required` without a passing finding in `evidence`: no check
/// finding names them, or one that does is not `pass`.
pub(crate) fn unmet(required: &[ElementId], evidence: &[Evidence]) -> Vec<ElementId> {
    required
        .iter()
        .filter(|id| {
            let verdicts: Vec<Verdict> = evidence
                .iter()
                .filter(|e| e.subject_ids.contains(id))
                .filter_map(|e| match &e.body {
                    EvidenceBody::Check { verdict, .. } => Some(*verdict),
                    EvidenceBody::ToolObservation { .. } => None,
                })
                .collect();
            verdicts.is_empty() || verdicts.iter().any(|v| *v != Verdict::Pass)
        })
        .cloned()
        .collect()
}

/// What is still unmet at `version`, for the job's `pending_work`: each
/// required requirement without a passing check, and a final commit when
/// none is missing but the design is not committed as final.
pub(crate) fn pending_work(
    unmet: &[ElementId],
    version: &SdrVersion,
    final_committed: bool,
) -> Vec<NonEmptyString> {
    let mut work: Vec<String> = unmet
        .iter()
        .map(|id| {
            format!(
                "requirement {}: no passing check at sdr version {}",
                id.as_str(),
                version.0
            )
        })
        .collect();
    if work.is_empty() && !final_committed {
        work.push(format!(
            "final design: not committed at sdr version {}",
            version.0
        ));
    }
    work.iter().filter_map(|w| w.parse().ok()).collect()
}

/// What the run returns. Every field but `summary` is the harness's; the
/// summary is left out, since nothing decides yet which model text would be
/// one.
pub(crate) struct ResultParts<'a> {
    pub(crate) job: &'a Job,
    pub(crate) stop_reason: StopReason,
    pub(crate) committed: &'a [TransitionId],
    pub(crate) last_committed: Option<SdrVersion>,
    pub(crate) evidence_ids: &'a [contracts::ids::EvidenceId],
    pub(crate) error: Option<ErrorInfo>,
}

pub(crate) fn task_result(parts: ResultParts<'_>) -> TaskResult {
    TaskResult {
        schema_version: TaskResultSchemaVersion::X01,
        task_id: parts.job.task.id.clone(),
        stop_reason: parts.stop_reason,
        sdr_version: parts.last_committed,
        committed_transition_ids: parts.committed.to_vec(),
        evidence_ids: parts.evidence_ids.to_vec(),
        artifact_ids: Vec::new(),
        usage: parts.job.budget_used.clone(),
        error: parts.error,
        summary: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn requirement(id: &str, priority: &str) -> Requirement {
        serde_json::from_value(json!({
            "id": id, "text": "t", "priority": priority, "evidence_ids": []
        }))
        .expect("requirement")
    }

    fn finding(subjects: &[&str], verdict: &str) -> Evidence {
        serde_json::from_value(json!({
            "schema_version": "0.1",
            "id": format!("ev-{}-{verdict}", subjects.join("-")),
            "run_id": "run-1",
            "created_at": "2026-01-01T00:00:00Z",
            "sdr_version": 1,
            "subject_ids": subjects,
            "body": {
                "kind": "check", "check_id": "c", "check_version": "0.1.0",
                "method": "deterministic", "inputs": [], "verdict": verdict
            }
        }))
        .expect("evidence")
    }

    fn ids(values: &[&str]) -> Vec<ElementId> {
        values.iter().map(|v| v.parse().expect("id")).collect()
    }

    fn strings(values: &[ElementId]) -> Vec<&str> {
        values.iter().map(|v| v.as_str()).collect()
    }

    #[test]
    fn must_requirements_and_the_tasks_ids_are_required_once_each() {
        let requirements = [
            requirement("R-1", "must"),
            requirement("R-2", "should"),
            requirement("R-3", "must"),
        ];
        let found = required(&requirements, &ids(&["R-2", "R-3", "R-9"]));
        assert_eq!(strings(&found), ["R-1", "R-3", "R-2", "R-9"]);
    }

    #[test]
    fn a_requirement_is_met_only_when_every_finding_on_it_passes() {
        let required = ids(&["R-1", "R-2", "R-3", "R-4"]);
        let evidence = [
            finding(&["R-1"], "pass"),
            finding(&["R-1", "p"], "pass"),
            finding(&["R-2"], "pass"),
            finding(&["R-2"], "unverified"),
            finding(&["R-3"], "fail"),
        ];
        // R-4 has no finding at all.
        assert_eq!(strings(&unmet(&required, &evidence)), ["R-2", "R-3", "R-4"]);
    }

    #[test]
    fn pending_work_names_what_is_unmet_or_the_missing_final_commit() {
        let text = |work: Vec<NonEmptyString>| -> Vec<String> {
            work.iter().map(|w| w.as_str().to_owned()).collect()
        };
        assert_eq!(
            text(pending_work(&ids(&["R4"]), &SdrVersion(3), false)),
            ["requirement R4: no passing check at sdr version 3"]
        );
        assert_eq!(
            text(pending_work(&[], &SdrVersion(3), false)),
            ["final design: not committed at sdr version 3"]
        );
        assert!(pending_work(&[], &SdrVersion(3), true).is_empty());
    }
}
