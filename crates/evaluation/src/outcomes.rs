//! Scores of a benchmark problem's expected outcomes (ADR-0025, ADR-0035).
//!
//! [`score`] decides each typed check of each outcome from what the harness
//! recorded: the run report, the event log and the committed SDR versions.
//! Requirement status uses the rule of the `success` stop
//! (`contracts::requirement_status`) over check evidence, and nothing reads
//! text the model wrote (blueprint section 9.4). A check is `undecided` when
//! the record lacks what it needs; only `pass` passes.

use std::collections::BTreeMap;

use contracts::types::{
    BenchmarkProblem, CheckScore, CheckScoreKind, Event, EventPayload, NonEmptyString,
    OutcomeCheck, OutcomeScore, RequirementStatus, RunReport, ScoreResult, SdrVersionView,
};
use contracts::{ConsideredFinding, Error, ErrorCode, requirement_status};

use crate::expression;
use crate::units::Unit;

/// What a run left to score it by.
#[derive(Clone, Copy, Debug)]
pub struct RunRecord<'a> {
    /// The final report.
    pub report: &'a RunReport,
    pub events: &'a [Event],
    /// Every committed version, from 0 to the report's final version.
    pub versions: &'a [SdrVersionView],
}

/// One score per expected outcome of `problem`, in the problem's order.
/// Fails only if a score cannot be written, which no record causes.
pub fn score(problem: &BenchmarkProblem, run: &RunRecord<'_>) -> Result<Vec<OutcomeScore>, Error> {
    problem
        .expected_outcomes
        .iter()
        .map(|outcome| {
            let checks = outcome
                .checks
                .iter()
                .map(|check| decide(check, run))
                .collect::<Result<_, _>>()?;
            Ok(outcome_score(outcome.id.clone(), checks))
        })
        .collect()
}

/// Every outcome of `problem` undecided for `reason`: a run that left no
/// final report to score. InvalidInput if `reason` is empty.
pub fn undecided(problem: &BenchmarkProblem, reason: &str) -> Result<Vec<OutcomeScore>, Error> {
    problem
        .expected_outcomes
        .iter()
        .map(|outcome| {
            let checks = outcome
                .checks
                .iter()
                .map(|check| check_score(kind(check), ScoreResult::Undecided, reason))
                .collect::<Result<_, _>>()?;
            Ok(outcome_score(outcome.id.clone(), checks))
        })
        .collect()
}

/// Whether every outcome passed.
pub fn all_passed(scores: &[OutcomeScore]) -> bool {
    !scores.is_empty() && scores.iter().all(|s| s.result == ScoreResult::Pass)
}

fn outcome_score(outcome_id: contracts::ids::Identifier, checks: Vec<CheckScore>) -> OutcomeScore {
    let result = if checks.iter().all(|c| c.result == ScoreResult::Pass) {
        ScoreResult::Pass
    } else if checks.iter().any(|c| c.result == ScoreResult::Fail) {
        ScoreResult::Fail
    } else {
        ScoreResult::Undecided
    };
    OutcomeScore {
        outcome_id,
        result,
        checks,
    }
}

fn decide(check: &OutcomeCheck, run: &RunRecord<'_>) -> Result<CheckScore, Error> {
    let kind = kind(check);
    let (result, reason) = match check {
        OutcomeCheck::StopReason { allowed } => stop_reason(allowed, run),
        OutcomeCheck::ParametersPresent { parameter_ids } => parameters_present(parameter_ids, run),
        OutcomeCheck::RequirementsPassing { requirement_ids } => {
            requirements_passing(requirement_ids, run)
        }
        OutcomeCheck::RequirementNeverPassing { requirement_id } => {
            requirement_never_passing(requirement_id, run)
        }
        OutcomeCheck::ParameterMatchesReference {
            expression,
            parameter_id,
            relative_tolerance,
            unit,
            variables,
        } => parameter_matches_reference(
            &Reference {
                expression: expression.as_str(),
                parameter_id: parameter_id.as_str(),
                unit: unit.as_str(),
                tolerance: (relative_tolerance.value, relative_tolerance.unit.as_str()),
                variables: variables
                    .iter()
                    .map(|v| (v.parameter_id.as_str(), v.unit.as_str()))
                    .collect(),
            },
            run,
        ),
        OutcomeCheck::ApprovalAnswered { approval_rule } => {
            approval_answered(approval_rule.as_str(), run)
        }
    };
    check_score(kind, result, &reason)
}

fn kind(check: &OutcomeCheck) -> CheckScoreKind {
    match check {
        OutcomeCheck::StopReason { .. } => CheckScoreKind::StopReason,
        OutcomeCheck::ParametersPresent { .. } => CheckScoreKind::ParametersPresent,
        OutcomeCheck::RequirementsPassing { .. } => CheckScoreKind::RequirementsPassing,
        OutcomeCheck::RequirementNeverPassing { .. } => CheckScoreKind::RequirementNeverPassing,
        OutcomeCheck::ParameterMatchesReference { .. } => CheckScoreKind::ParameterMatchesReference,
        OutcomeCheck::ApprovalAnswered { .. } => CheckScoreKind::ApprovalAnswered,
    }
}

fn check_score(
    kind: CheckScoreKind,
    result: ScoreResult,
    reason: &str,
) -> Result<CheckScore, Error> {
    let reason = NonEmptyString::try_from(reason.to_owned())
        .map_err(|_| Error::new(ErrorCode::InvalidInput, "a check score needs a reason"))?;
    Ok(CheckScore {
        kind,
        result,
        reason,
    })
}

type Decided = (ScoreResult, String);

fn pass(reason: String) -> Decided {
    (ScoreResult::Pass, reason)
}

fn fail(reason: String) -> Decided {
    (ScoreResult::Fail, reason)
}

fn undecided_because(reason: String) -> Decided {
    (ScoreResult::Undecided, reason)
}

fn stop_reason(allowed: &[contracts::types::StopReason], run: &RunRecord<'_>) -> Decided {
    let stopped = run.report.stop_reason;
    let allowed_list: Vec<String> = allowed.iter().map(ToString::to_string).collect();
    if allowed.contains(&stopped) {
        pass(format!("the stop reason is {stopped}"))
    } else {
        fail(format!(
            "the stop reason is {stopped}, not one of {}",
            allowed_list.join(", ")
        ))
    }
}

/// The view of the report's final version.
fn final_view<'a>(run: &RunRecord<'a>) -> Result<&'a SdrVersionView, Decided> {
    let version = run.report.final_sdr_version.0;
    run.versions
        .iter()
        .find(|v| v.document.version.0 == version)
        .ok_or_else(|| undecided_because(format!("final version {version} was not read")))
}

fn parameters_present(ids: &[contracts::types::ElementId], run: &RunRecord<'_>) -> Decided {
    let view = match final_view(run) {
        Ok(view) => view,
        Err(undecided) => return undecided,
    };
    let missing: Vec<&str> = ids
        .iter()
        .map(|id| id.as_str())
        .filter(|id| {
            !view
                .document
                .design
                .parameters
                .iter()
                .any(|p| p.id.as_str() == *id)
        })
        .collect();
    let version = view.document.version.0;
    if missing.is_empty() {
        pass(format!(
            "version {version} has every parameter, each with a unit"
        ))
    } else {
        fail(format!("version {version} lacks {}", missing.join(", ")))
    }
}

/// The status of `requirement_id` at `view`, by the harness's own rule, or
/// `None` if the version has no such requirement.
fn status_at(view: &SdrVersionView, requirement_id: &str) -> Option<RequirementStatus> {
    let requirement = view
        .document
        .requirements
        .iter()
        .find(|r| r.id.as_str() == requirement_id)?;
    let findings = ConsideredFinding::all(&view.evidence);
    Some(requirement_status(requirement, &findings, None).status)
}

fn requirements_passing(ids: &[contracts::types::ElementId], run: &RunRecord<'_>) -> Decided {
    let view = match final_view(run) {
        Ok(view) => view,
        Err(undecided) => return undecided,
    };
    let version = view.document.version.0;
    let mut not_passing = Vec::new();
    for id in ids {
        match status_at(view, id.as_str()) {
            Some(RequirementStatus::Pass) => {}
            Some(status) => not_passing.push(format!("{} is {status}", id.as_str())),
            None => {
                return undecided_because(format!(
                    "version {version} has no requirement {}",
                    id.as_str()
                ));
            }
        }
    }
    if not_passing.is_empty() {
        pass(format!(
            "at version {version} the check evidence passes every requirement"
        ))
    } else {
        fail(format!("at version {version} {}", not_passing.join(", ")))
    }
}

fn requirement_never_passing(id: &contracts::types::ElementId, run: &RunRecord<'_>) -> Decided {
    let last = run.report.final_sdr_version.0;
    let by_version: BTreeMap<u64, &SdrVersionView> = run
        .versions
        .iter()
        .map(|v| (v.document.version.0, v))
        .collect();
    for version in 0..=last {
        let Some(view) = by_version.get(&version) else {
            return undecided_because(format!("version {version} was not read"));
        };
        if status_at(view, id.as_str()) == Some(RequirementStatus::Pass) {
            return fail(format!("version {version} shows {} passing", id.as_str()));
        }
    }
    pass(format!(
        "no version from 0 to {last} shows {} passing",
        id.as_str()
    ))
}

struct Reference<'a> {
    expression: &'a str,
    parameter_id: &'a str,
    unit: &'a str,
    /// The value and its unit, such as 0.5 %.
    tolerance: (f64, &'a str),
    /// Each variable's parameter id and the unit the expression takes it in.
    variables: Vec<(&'a str, &'a str)>,
}

fn parameter_matches_reference(reference: &Reference<'_>, run: &RunRecord<'_>) -> Decided {
    let view = match final_view(run) {
        Ok(view) => view,
        Err(undecided) => return undecided,
    };
    let in_unit = |id: &str, unit: &str| -> Result<f64, Decided> {
        let parameter = view
            .document
            .design
            .parameters
            .iter()
            .find(|p| p.id.as_str() == id)
            .ok_or_else(|| {
                undecided_because(format!(
                    "version {} has no parameter {id}",
                    view.document.version.0
                ))
            })?;
        convert(parameter.value.value, parameter.value.unit.as_str(), unit)
            .map_err(|e| undecided_because(format!("{id}: {e}")))
    };

    let mut values = BTreeMap::new();
    for (id, unit) in &reference.variables {
        match in_unit(id, unit) {
            Ok(value) => {
                values.insert(*id, value);
            }
            Err(undecided) => return undecided,
        }
    }
    let parsed = match expression::parse(reference.expression) {
        Ok(parsed) => parsed,
        Err(e) => return undecided_because(format!("the reference does not parse: {e}")),
    };
    let Some(expected) = parsed.evaluate(&values) else {
        return undecided_because("the reference is not a finite number".to_owned());
    };
    let actual = match in_unit(reference.parameter_id, reference.unit) {
        Ok(actual) => actual,
        Err(undecided) => return undecided,
    };
    let (tolerance_value, tolerance_unit) = reference.tolerance;
    let tolerance = match convert(tolerance_value, tolerance_unit, "1") {
        Ok(tolerance) => tolerance,
        Err(e) => return undecided_because(format!("relative tolerance: {e}")),
    };
    if expected == 0.0 {
        return undecided_because("the reference is 0, so no relative difference".to_owned());
    }
    let relative = ((actual - expected) / expected).abs();
    let summary = format!(
        "{} is {actual} {unit}, the reference {expected} {unit}: {:.3} % apart, allowed {tolerance_value} {tolerance_unit}",
        reference.parameter_id,
        relative * 100.0,
        unit = reference.unit,
    );
    if relative <= tolerance {
        pass(summary)
    } else {
        fail(summary)
    }
}

fn convert(value: f64, from: &str, to: &str) -> Result<f64, String> {
    let from = Unit::parse(from).map_err(|e| e.to_string())?;
    let to = Unit::parse(to).map_err(|e| e.to_string())?;
    from.convert(value, &to).map_err(|e| e.to_string())
}

fn approval_answered(rule: &str, run: &RunRecord<'_>) -> Decided {
    let requested: Vec<&str> = run
        .events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::ApprovalRequested {
                approval_id, rules, ..
            } if rules.iter().any(|r| r.as_str() == rule) => Some(approval_id.as_str()),
            _ => None,
        })
        .collect();
    if requested.is_empty() {
        return fail(format!("no approval named rule {rule}"));
    }
    let answered = run.events.iter().find_map(|e| match &e.payload {
        EventPayload::ApprovalAnswered {
            approval_id,
            answer,
            ..
        } if requested.contains(&approval_id.as_str()) => Some((approval_id.as_str(), *answer)),
        _ => None,
    });
    match answered {
        Some((approval, answer)) => pass(format!(
            "approval {approval} of rule {rule} was answered {answer}"
        )),
        None => fail(format!(
            "approval {} of rule {rule} was never answered",
            requested.join(", ")
        )),
    }
}
