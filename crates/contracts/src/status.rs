//! A requirement's status, derived from check findings (ADR-0033). One rule
//! serves both the `success` stop (ADR-0030) and the run report, so the two
//! can never disagree about what passed.
//!
//! - A counted `fail` makes the requirement fail.
//! - Otherwise it passes when at least one finding counts and every counted
//!   finding is `pass`.
//! - Otherwise it is unverified, with the reason. A check that failed to run
//!   (`error`) is not evidence against the design, so it never fails one.
//!
//! A finding does not count when an element it relied on is stale, or when it
//! was made below the required fidelity. `waived` is never derived here: it
//! comes only from an approval-gate record (none in Increment 1).

use schema_types::{
    Evidence, EvidenceBody, Fidelity, IgnoredReason, ReportFinding, Requirement, RequirementStatus,
    RequirementStatusRow, UnverifiedReason, Verdict,
};

/// Evidence about a requirement, with what decides whether it counts.
#[derive(Clone, Copy, Debug)]
pub struct ConsideredFinding<'a> {
    pub evidence: &'a Evidence,
    /// The fidelity it was made at. Evidence records none until Increment 2.
    pub fidelity: Option<Fidelity>,
    /// Whether an element it relied on is stale. Never, until Increment 2.
    pub stale: bool,
}

impl<'a> ConsideredFinding<'a> {
    /// Every record of `evidence` as made in Increment 1: no fidelity, not
    /// stale.
    pub fn all(evidence: &'a [Evidence]) -> Vec<Self> {
        evidence
            .iter()
            .map(|evidence| Self {
                evidence,
                fidelity: None,
                stale: false,
            })
            .collect()
    }

    fn ignored(&self, required: Option<Fidelity>) -> Option<IgnoredReason> {
        if self.stale {
            return Some(IgnoredReason::Stale);
        }
        match (required, self.fidelity) {
            (Some(required), Some(made)) if made >= required => None,
            (Some(_), _) => Some(IgnoredReason::BelowFidelity),
            (None, _) => None,
        }
    }
}

/// The status of `requirement` from `findings`. Only check findings that
/// name the requirement are considered; tool observations and findings about
/// other elements are left out of the row. With `required` set, a finding
/// below it, or of unknown fidelity, does not count.
pub fn requirement_status(
    requirement: &Requirement,
    findings: &[ConsideredFinding<'_>],
    required: Option<Fidelity>,
) -> RequirementStatusRow {
    let considered: Vec<(Verdict, ReportFinding)> = findings
        .iter()
        .filter(|f| f.evidence.subject_ids.contains(&requirement.id))
        .filter_map(|f| match &f.evidence.body {
            EvidenceBody::Check { verdict, .. } => {
                let ignored_reason = f.ignored(required);
                Some((
                    *verdict,
                    ReportFinding {
                        evidence_id: f.evidence.id.clone(),
                        verdict: *verdict,
                        used: ignored_reason.is_none(),
                        ignored_reason,
                    },
                ))
            }
            EvidenceBody::ToolObservation { .. } => None,
        })
        .collect();
    let counted: Vec<Verdict> = considered
        .iter()
        .filter(|(_, f)| f.used)
        .map(|(v, _)| *v)
        .collect();

    let (status, reason) = if counted.contains(&Verdict::Fail) {
        (RequirementStatus::Fail, None)
    } else if !counted.is_empty() && counted.iter().all(|v| *v == Verdict::Pass) {
        (RequirementStatus::Pass, None)
    } else {
        (
            RequirementStatus::Unverified,
            Some(unverified(&considered, &counted)),
        )
    };
    RequirementStatusRow {
        requirement_id: requirement.id.clone(),
        priority: requirement.priority,
        status,
        reason,
        evidence: considered.into_iter().map(|(_, f)| f).collect(),
    }
}

/// Why nothing decided the requirement, given that no counted finding
/// failed and not all of them passed.
fn unverified(considered: &[(Verdict, ReportFinding)], counted: &[Verdict]) -> UnverifiedReason {
    if counted.is_empty() {
        let ignored = considered.iter().filter_map(|(_, f)| f.ignored_reason);
        let mut reasons: Vec<IgnoredReason> = ignored.collect();
        reasons.dedup();
        return if reasons.is_empty() {
            UnverifiedReason::NoEvidence
        } else if reasons.contains(&IgnoredReason::Stale) {
            UnverifiedReason::Stale
        } else {
            UnverifiedReason::BelowFidelity
        };
    }
    if counted.contains(&Verdict::Error) {
        UnverifiedReason::CheckError
    } else {
        UnverifiedReason::Undecided
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn requirement(id: &str) -> Requirement {
        serde_json::from_value(json!({
            "id": id, "text": "Mass at most 1 kg.", "priority": "must", "evidence_ids": []
        }))
        .expect("a requirement")
    }

    fn check(id: &str, subject: &str, verdict: &str) -> Evidence {
        serde_json::from_value(json!({
            "schema_version": "0.1", "id": id, "run_id": "run-1",
            "created_at": "2026-10-25T10:00:00Z", "sdr_version": 1, "subject_ids": [subject],
            "body": { "kind": "check", "check_id": "requirement.numeric_threshold",
                "check_version": "0.1.0", "method": "deterministic", "inputs": [],
                "verdict": verdict }
        }))
        .expect("check evidence")
    }

    fn observation(id: &str, subject: &str) -> Evidence {
        serde_json::from_value(json!({
            "schema_version": "0.1", "id": id, "run_id": "run-1",
            "created_at": "2026-10-25T10:00:00Z", "sdr_version": 1, "subject_ids": [subject],
            "body": { "kind": "tool_observation", "tool": "measure", "tool_version": "0.1.0",
                "step_id": "step-1", "outputs": [], "artifact_ids": [] }
        }))
        .expect("a tool observation")
    }

    fn status_of(verdicts: &[&str]) -> (RequirementStatus, Option<UnverifiedReason>) {
        let evidence: Vec<Evidence> = verdicts
            .iter()
            .enumerate()
            .map(|(i, v)| check(&format!("ev-{i}"), "R1", v))
            .collect();
        let row = requirement_status(&requirement("R1"), &ConsideredFinding::all(&evidence), None);
        (row.status, row.reason)
    }

    #[test]
    fn verdicts_decide_the_status() {
        use RequirementStatus::{Fail, Pass, Unverified};
        use UnverifiedReason::{CheckError, NoEvidence, Undecided};
        let table: &[(&[&str], RequirementStatus, Option<UnverifiedReason>)] = &[
            (&[], Unverified, Some(NoEvidence)),
            (&["pass"], Pass, None),
            (&["pass", "pass"], Pass, None),
            (&["fail"], Fail, None),
            (&["pass", "fail"], Fail, None),
            (&["error", "fail"], Fail, None),
            (&["unverified", "fail"], Fail, None),
            (&["error"], Unverified, Some(CheckError)),
            (&["pass", "error"], Unverified, Some(CheckError)),
            (&["unverified", "error"], Unverified, Some(CheckError)),
            (&["unverified"], Unverified, Some(Undecided)),
            (&["pass", "unverified"], Unverified, Some(Undecided)),
        ];
        for (verdicts, status, reason) in table {
            assert_eq!(status_of(verdicts), (*status, *reason), "{verdicts:?}");
        }
    }

    #[test]
    fn only_check_findings_about_the_requirement_are_listed() {
        let evidence = vec![
            check("ev-1", "R1", "pass"),
            check("ev-2", "R2", "fail"),
            observation("ev-3", "R1"),
        ];
        let row = requirement_status(&requirement("R1"), &ConsideredFinding::all(&evidence), None);
        assert_eq!(row.status, RequirementStatus::Pass);
        let ids: Vec<&str> = row
            .evidence
            .iter()
            .map(|f| f.evidence_id.as_str())
            .collect();
        assert_eq!(ids, ["ev-1"]);
        assert!(row.evidence[0].used && row.evidence[0].ignored_reason.is_none());
    }

    #[test]
    fn ignored_findings_are_listed_and_do_not_count() {
        let evidence = [check("ev-1", "R1", "fail"), check("ev-2", "R1", "pass")];
        let stale_fail = ConsideredFinding {
            evidence: &evidence[0],
            fidelity: Some(Fidelity::L2),
            stale: true,
        };
        let low_pass = ConsideredFinding {
            evidence: &evidence[1],
            fidelity: Some(Fidelity::L1),
            stale: false,
        };
        let row = requirement_status(
            &requirement("R1"),
            &[stale_fail, low_pass],
            Some(Fidelity::L2),
        );
        assert_eq!(row.status, RequirementStatus::Unverified);
        assert_eq!(row.reason, Some(UnverifiedReason::Stale));
        let reasons: Vec<_> = row
            .evidence
            .iter()
            .map(|f| (f.used, f.ignored_reason))
            .collect();
        assert_eq!(
            reasons,
            [
                (false, Some(IgnoredReason::Stale)),
                (false, Some(IgnoredReason::BelowFidelity))
            ]
        );

        let unknown = ConsideredFinding {
            evidence: &evidence[1],
            fidelity: None,
            stale: false,
        };
        let row = requirement_status(&requirement("R1"), &[unknown], Some(Fidelity::L0));
        assert_eq!(row.reason, Some(UnverifiedReason::BelowFidelity));
        let at = ConsideredFinding {
            fidelity: Some(Fidelity::L3),
            ..unknown
        };
        let row = requirement_status(&requirement("R1"), &[at], Some(Fidelity::L2));
        assert_eq!(
            row.status,
            RequirementStatus::Pass,
            "above the required fidelity counts"
        );
    }
}
