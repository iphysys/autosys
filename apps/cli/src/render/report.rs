//! The human views of `run report` and `run status` (ADR-0036).

use api_client::types::{
    JobStatus, ReportFinding, RequirementStatusRow, Run, RunReport, WakeCondition,
};

use super::{MAX_WIDTH, Zone, sanitize, state_name, table};

/// The run report: how the run stopped, the requirement table, the
/// artifacts and the budget used.
pub(crate) fn report_lines(report: &RunReport) -> Vec<String> {
    let finality = if report.final_ {
        "the report is final"
    } else {
        "the report is not final yet"
    };
    let mut lines = vec![
        format!(
            "{}  {}",
            report.run_id.as_str(),
            sanitize(report.project_id.as_str(), MAX_WIDTH)
        ),
        format!("stopped:  {}  ({finality})", report.stop_reason),
        format!("design:   SDR version {}", report.final_sdr_version),
        String::new(),
    ];
    if report.requirements.is_empty() {
        lines.push("requirements: none".to_owned());
    } else {
        let rows: Vec<Vec<String>> = report
            .requirements
            .iter()
            .map(|row| {
                vec![
                    row.requirement_id.as_str().to_owned(),
                    row.priority.to_string(),
                    status_cell(row),
                    evidence(&row.evidence),
                ]
            })
            .collect();
        lines.extend(table(
            &["requirement", "priority", "status", "evidence"],
            &rows,
        ));
    }
    lines.push(String::new());
    if report.artifacts.is_empty() {
        lines.push("artifacts: none".to_owned());
    } else {
        let rows: Vec<Vec<String>> = report
            .artifacts
            .iter()
            .map(|a| {
                vec![
                    a.artifact_id.as_str().to_owned(),
                    a.format.as_str().to_owned(),
                ]
            })
            .collect();
        lines.extend(table(&["artifact", "format"], &rows));
    }
    lines.push(String::new());
    if report.budget_used.is_empty() {
        lines.push("budget used: none".to_owned());
    } else {
        let rows: Vec<Vec<String>> = report
            .budget_used
            .iter()
            .map(|b| {
                let u = &b.usage;
                vec![
                    b.agent_id.as_str().to_owned(),
                    u.input_tokens.to_string(),
                    u.output_tokens.to_string(),
                    u.tool_calls.to_string(),
                    u.steps.to_string(),
                    format!("{:.0} s", u.wall_time_seconds),
                ]
            })
            .collect();
        lines.extend(table(
            &[
                "agent",
                "tokens in",
                "tokens out",
                "tool calls",
                "steps",
                "wall time",
            ],
            &rows,
        ));
    }
    lines
}

/// A requirement's status, with the reason when it has one.
pub(crate) fn status_cell(row: &RequirementStatusRow) -> String {
    match &row.reason {
        Some(reason) => format!("{} ({reason})", row.status),
        None => row.status.to_string(),
    }
}

/// A requirement's findings by evidence id; a finding that did not count
/// says so.
fn evidence(findings: &[ReportFinding]) -> String {
    if findings.is_empty() {
        return "-".to_owned();
    }
    findings
        .iter()
        .map(|f| {
            if f.used {
                f.evidence_id.as_str().to_owned()
            } else {
                format!("{} (not used)", f.evidence_id.as_str())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The run's status, the project, when it last changed and its last event;
/// for a run that waits, the approval it waits for.
pub(crate) fn status_lines(run: &Run, zone: Zone) -> Vec<String> {
    let state = state_name(&run.status);
    let (status, approval) = match &run.status {
        JobStatus::Waiting { wake_condition, .. } | JobStatus::Suspended { wake_condition, .. } => {
            let WakeCondition::ApprovalAnswered(approval) = wake_condition;
            (
                format!("{state}, waiting for approval {}", approval.as_str()),
                Some(approval.as_str()),
            )
        }
        JobStatus::Completed { stop_reason, .. } => (format!("{state}: {stop_reason}"), None),
        JobStatus::Failed { failure, .. } => (
            format!(
                "{state}: {}: {}: {}",
                failure.kind,
                failure.error.code.as_str(),
                failure.error.message.as_str()
            ),
            None,
        ),
        JobStatus::Queued { .. } | JobStatus::Running | JobStatus::Cancelled { .. } => {
            (state.to_owned(), None)
        }
    };
    let mut lines = vec![
        format!(
            "{}  {}",
            run.run_id.as_str(),
            sanitize(run.project_id.as_str(), MAX_WIDTH)
        ),
        format!("status:    {}", sanitize(&status, MAX_WIDTH)),
        format!(
            "updated:   {}",
            zone.format(&run.updated_at, "%Y-%m-%d %H:%M:%S %:z")
        ),
        format!("last seq:  {}", run.last_seq),
    ];
    if let Some(approval) = approval {
        lines.push(String::new());
        lines.push(format!(
            "answer it: synth run approve {} {approval} --answer approve|reject",
            run.run_id.as_str()
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;

    use super::*;

    fn utc() -> Zone {
        Zone::Fixed(FixedOffset::east_opt(0).unwrap())
    }

    #[test]
    fn the_example_report_renders_as_its_golden_text() {
        let report: RunReport = serde_json::from_str(include_str!(
            "../../../../schemas/reports/examples/run-report.json"
        ))
        .unwrap();
        assert_eq!(
            report_lines(&report).join("\n"),
            "\
run-0001  example-project
stopped:  success  (the report is final)
design:   SDR version 3

requirement  priority  status                  evidence
req-mass     must      pass                    ev-check-mass-1
req-finish   should    unverified (undecided)  ev-check-finish-1

artifact         format
art-part-step-1  step

agent              tokens in  tokens out  tool calls  steps  wall time
main_orchestrator  52000      4700        7           11     760 s"
        );
    }

    #[test]
    fn a_suspended_run_names_the_approval_it_waits_for() {
        let run: Run = serde_json::from_str(include_str!(
            "../../../../schemas/api/examples/run.suspended.json"
        ))
        .unwrap();
        assert_eq!(
            status_lines(&run, utc()).join("\n"),
            "\
run-0001  proj-0001
status:    suspended, waiting for approval appr-0001
updated:   2026-10-26 09:04:12 +00:00
last seq:  27

answer it: synth run approve run-0001 appr-0001 --answer approve|reject"
        );
    }

    #[test]
    fn a_queued_run_has_no_approval() {
        let run: Run =
            serde_json::from_str(include_str!("../../../../schemas/api/examples/run.json"))
                .unwrap();
        assert_eq!(
            status_lines(&run, utc()).join("\n"),
            "\
run-0001  proj-0001
status:    queued
updated:   2026-10-26 09:00:00 +00:00
last seq:  1"
        );
    }
}
