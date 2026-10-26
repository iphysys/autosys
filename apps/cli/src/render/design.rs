//! The human views of `sdr show`, `artifact show` and the summary of `run
//! export` (ADR-0036).

use api_client::types::{ArtifactMetadata, DesignDefinition, RunReport, SdrVersionView};

use super::report::status_cell;
use super::{MAX_WIDTH, Zone, sanitize, table};

/// One committed version: what committed it and why, then its parameters
/// and decisions.
pub(crate) fn sdr_lines(view: &SdrVersionView, zone: Zone) -> Vec<String> {
    let document = &view.document;
    let parent = match &document.parent_version {
        Some(parent) => format!("  (parent {parent})"),
        None => String::new(),
    };
    let mut lines = vec![
        format!(
            "{}  SDR version {}{parent}",
            sanitize(document.project_id.as_str(), MAX_WIDTH),
            document.version
        ),
        format!(
            "committed: {}",
            zone.format(&view.record.committed_at, "%Y-%m-%d %H:%M:%S %:z")
        ),
        format!(
            "reason:    {}",
            sanitize(view.record.reason.as_str(), MAX_WIDTH)
        ),
        String::new(),
    ];
    lines.extend(design_lines(&document.design));
    lines
}

/// The design's parameters with their values and units, then its
/// decisions.
fn design_lines(design: &DesignDefinition) -> Vec<String> {
    let mut lines = Vec::new();
    if design.parameters.is_empty() {
        lines.push("parameters: none".to_owned());
    } else {
        let rows: Vec<Vec<String>> = design
            .parameters
            .iter()
            .map(|p| {
                vec![
                    p.id.as_str().to_owned(),
                    p.value.value.to_string(),
                    p.value.unit.as_str().to_owned(),
                ]
            })
            .collect();
        lines.extend(table(&["parameter", "value", "unit"], &rows));
    }
    lines.push(String::new());
    if design.decisions.is_empty() {
        lines.push("decisions: none".to_owned());
    } else {
        let rows: Vec<Vec<String>> = design
            .decisions
            .iter()
            .map(|d| vec![d.id.as_str().to_owned(), d.choice.as_str().to_owned()])
            .collect();
        lines.extend(table(&["decision", "choice"], &rows));
    }
    lines
}

/// An artifact's metadata: what it is, where it came from and how to
/// check its bytes.
pub(crate) fn artifact_lines(artifact: &ArtifactMetadata, zone: Zone) -> Vec<String> {
    let evidence = if artifact.evidence_ids.is_empty() {
        "-".to_owned()
    } else {
        artifact
            .evidence_ids
            .iter()
            .map(|e| e.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    vec![
        format!(
            "{}  {}  ({})",
            artifact.id.as_str(),
            sanitize(artifact.format.as_str(), MAX_WIDTH),
            sanitize(artifact.media_type.as_str(), MAX_WIDTH)
        ),
        format!(
            "run:       {}  (project {})",
            artifact.run_id.as_str(),
            sanitize(artifact.project_id.as_str(), MAX_WIDTH)
        ),
        format!(
            "made by:   {} {} at {}, from SDR version {}",
            sanitize(artifact.tool.as_str(), MAX_WIDTH),
            sanitize(artifact.tool_version.as_str(), MAX_WIDTH),
            artifact.step_id.as_str(),
            artifact.source_sdr_version
        ),
        format!(
            "created:   {}",
            zone.format(&artifact.created_at, "%Y-%m-%d %H:%M:%S %:z")
        ),
        format!("size:      {} bytes", artifact.size_bytes),
        format!("sha256:    {}", artifact.sha256.as_str()),
        format!("evidence:  {}", sanitize(&evidence, MAX_WIDTH)),
    ]
}

/// What `run export` prints after the files: how the run stopped, the
/// requirement statuses, and the design of the last version exported.
pub(crate) fn export_summary(report: &RunReport, last: Option<&SdrVersionView>) -> Vec<String> {
    let finality = if report.final_ {
        "the report is final"
    } else {
        "the report is not final yet"
    };
    let mut lines = vec![
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
                ]
            })
            .collect();
        lines.extend(table(&["requirement", "priority", "status"], &rows));
    }
    if let Some(view) = last {
        lines.push(String::new());
        lines.extend(design_lines(&view.document.design));
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

    fn example_version() -> SdrVersionView {
        serde_json::from_str(include_str!(
            "../../../../schemas/api/examples/sdr-version.json"
        ))
        .unwrap()
    }

    #[test]
    fn the_example_version_renders_as_its_golden_text() {
        assert_eq!(
            sdr_lines(&example_version(), utc()).join("\n"),
            "\
example-project  SDR version 2  (parent 1)
committed: 2026-10-22 09:04:14 +00:00
reason:    Record the measured mass and the ribbed-section decision.

parameter  value  unit
part.mass  0.212  kg

decisions: none"
        );
    }

    #[test]
    fn decisions_are_listed_by_id() {
        let mut view = example_version();
        view.document.design.decisions = serde_json::from_value(serde_json::json!([{
            "id": "dec-rib",
            "choice": "Ribbed\nsection",
            "rationale": "Stiffer for its mass.",
            "related_ids": []
        }]))
        .unwrap();
        let lines = sdr_lines(&view, utc());
        assert_eq!(
            lines[lines.len() - 2..],
            ["decision  choice", "dec-rib   Ribbed section"]
        );
    }

    #[test]
    fn the_example_artifact_renders_as_its_golden_text() {
        let artifact: ArtifactMetadata = serde_json::from_str(include_str!(
            "../../../../schemas/artifacts/examples/artifact.json"
        ))
        .unwrap();
        assert_eq!(
            artifact_lines(&artifact, utc()).join("\n"),
            "\
art-part-step-1  step  (model/step)
run:       run-0001  (project example-project)
made by:   create_part 0.1.0 at step-0006, from SDR version 2
created:   2026-10-22 09:04:10 +00:00
size:      0 bytes
sha256:    sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
evidence:  ev-tool-measure-1"
        );
    }

    #[test]
    fn the_export_summary_has_the_statuses_and_the_last_design() {
        let report: RunReport = serde_json::from_str(include_str!(
            "../../../../schemas/reports/examples/run-report.json"
        ))
        .unwrap();
        assert_eq!(
            export_summary(&report, Some(&example_version())).join("\n"),
            "\
stopped:  success  (the report is final)
design:   SDR version 3

requirement  priority  status
req-mass     must      pass
req-finish   should    unverified (undecided)

parameter  value  unit
part.mass  0.212  kg

decisions: none"
        );
        assert_eq!(export_summary(&report, None).len(), 6);
    }
}
