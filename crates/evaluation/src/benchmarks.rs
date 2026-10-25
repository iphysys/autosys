//! A benchmark problem as a run request (ADR-0034).
//!
//! [`run_request`] builds the `RunRequest` a client sends to start a run on a
//! problem. It copies what the task needs and nothing that grades it: the
//! problem's `notes`, `expected_outcomes`, `feasibility` and `status` never
//! reach the agent (ADR-0025). It is a pure function, so the CLI's request
//! file, the evaluation runs and the improvement loop build the same request
//! from the same problem.

use contracts::types::{
    AgentSpec, BenchmarkProblem, Budget, NonEmptyString, Requirement, RunRequest,
    RunRequestSchemaVersion,
};
use contracts::{Error, ErrorCode};

use crate::units::Unit;

/// The request that starts a run of `agent` on `problem`.
///
/// The budget is the agent's limits, narrowed by the problem's budgets; a
/// problem never widens them. InvalidInput if the problem's wall time is not
/// a time.
pub fn run_request(problem: &BenchmarkProblem, agent: &AgentSpec) -> Result<RunRequest, Error> {
    Ok(RunRequest {
        schema_version: RunRequestSchemaVersion::X01,
        agent_id: agent.id.clone(),
        name: text(problem.id.as_str().to_owned())?,
        description: description(problem)?,
        goal: agent.goal.clone(),
        acceptance_criteria: problem
            .requirements
            .iter()
            .map(|r| {
                text(format!(
                    "{}: {} ({})",
                    r.id.as_str(),
                    r.text.as_str(),
                    r.quantity.as_str()
                ))
            })
            .collect::<Result<_, _>>()?,
        requirements: problem
            .requirements
            .iter()
            .map(|r| Requirement {
                id: r.id.clone(),
                text: r.text.clone(),
                priority: r.priority,
                numeric: Some(r.numeric.clone()),
                evidence_ids: Vec::new(),
            })
            .collect(),
        budget: Some(budget(problem, &agent.limits)?),
    })
}

/// The problem's description, then its givens as the specification words
/// them.
fn description(problem: &BenchmarkProblem) -> Result<NonEmptyString, Error> {
    let mut description = problem.description.to_string();
    if !problem.givens.is_empty() {
        description.push_str("\n\nGivens:");
        for given in &problem.givens {
            description.push_str("\n- ");
            description.push_str(&given.text);
        }
    }
    text(description)
}

fn budget(problem: &BenchmarkProblem, limits: &Budget) -> Result<Budget, Error> {
    let budgets = &problem.budgets;
    Ok(Budget {
        max_tokens: limits.max_tokens,
        max_steps: limits.max_steps,
        max_tool_calls: limits.max_tool_calls.min(budgets.max_tool_calls),
        max_wall_time_seconds: limits
            .max_wall_time_seconds
            .min(wall_time_seconds(problem)?),
        max_repair_attempts: limits
            .max_repair_attempts
            .min(budgets.max_repair_attempts_per_step),
    })
}

/// The problem's wall time in whole seconds, rounded down so the run never
/// gets more than the problem allows, and at least one.
fn wall_time_seconds(problem: &BenchmarkProblem) -> Result<std::num::NonZeroU64, Error> {
    let wall_time = &problem.budgets.max_wall_time;
    let invalid = |reason: String| {
        Error::new(
            ErrorCode::InvalidInput,
            format!("{}: budgets.max_wall_time: {reason}", problem.id.as_str()),
        )
    };
    let seconds = Unit::parse(&wall_time.unit)
        .and_then(|unit| unit.convert(wall_time.value, &Unit::parse("s")?))
        .map_err(|e| invalid(e.to_string()))?;
    if !seconds.is_finite() || seconds < 1.0 {
        return Err(invalid(format!("{seconds} s is not a positive time")));
    }
    // Saturates beyond u64::MAX seconds; the agent's limit is smaller anyway.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "finite and at least 1, checked above"
    )]
    let whole = seconds.floor() as u64;
    std::num::NonZeroU64::new(whole).ok_or_else(|| invalid("rounds to zero seconds".to_owned()))
}

fn text(value: String) -> Result<NonEmptyString, Error> {
    NonEmptyString::try_from(value)
        .map_err(|e| Error::new(ErrorCode::InvalidInput, format!("not a request field: {e}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "tests")]

    use std::path::{Path, PathBuf};

    use contracts::SchemaSet;
    use serde_json::Value;

    use super::*;

    const RUN_REQUEST: &str = "api/run-request.schema.json";

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn yaml<T: serde::de::DeserializeOwned>(path: &str) -> T {
        let text = std::fs::read_to_string(repo_root().join(path))
            .unwrap_or_else(|e| panic!("{path}: {e}"));
        let value: Value = serde_saphyr::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_value(value).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn agent() -> AgentSpec {
        yaml("config/agents/main_orchestrator.yaml")
    }

    fn problems() -> Vec<(String, BenchmarkProblem)> {
        ["part_design_001", "part_design_002"]
            .into_iter()
            .map(|id| {
                let path = format!("evals/benchmarks/dev/{id}/problem.yaml");
                (path.clone(), yaml(&path))
            })
            .collect()
    }

    #[test]
    fn every_dev_problem_becomes_a_valid_request() {
        let schemas = SchemaSet::compile(&[RUN_REQUEST]).expect("the run request schema");
        for (path, problem) in problems() {
            let request = run_request(&problem, &agent()).expect("a request");
            let value = serde_json::to_value(&request).expect("serializable");
            let problems = schemas.errors(RUN_REQUEST, &value).expect("checkable");
            assert!(problems.is_empty(), "{path}: {problems:?}");
            assert_eq!(request.agent_id, agent().id);
            assert_eq!(request.name.as_str(), problem.id.as_str());
            assert_eq!(request.requirements.len(), problem.requirements.len());
            for (sent, specified) in request.requirements.iter().zip(&problem.requirements) {
                assert_eq!(sent.id, specified.id);
                assert_eq!(sent.numeric.as_ref(), Some(&specified.numeric));
                assert!(sent.evidence_ids.is_empty());
            }
            for given in &problem.givens {
                assert!(
                    request.description.contains(given.text.as_str()),
                    "{path}: given {} is missing",
                    given.id.as_str()
                );
            }
        }
    }

    #[test]
    fn nothing_that_grades_the_run_reaches_the_task() {
        for (path, problem) in problems() {
            let request = run_request(&problem, &agent()).expect("a request");
            let sent = serde_json::to_string(&request).expect("serializable");
            if let Some(notes) = &problem.notes {
                assert!(!sent.contains(notes.as_str()), "{path}: notes leaked");
            }
            for outcome in &problem.expected_outcomes {
                assert!(
                    !sent.contains(outcome.statement.as_str()),
                    "{path}: outcome {} leaked",
                    outcome.id.as_str()
                );
                let checks = serde_json::to_value(&outcome.checks).expect("serializable");
                for expression in checks
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["expression"].as_str())
                {
                    assert!(!sent.contains(expression), "{path}: a reference leaked");
                }
            }
        }
    }

    #[test]
    fn the_problem_narrows_the_agents_budget() {
        let agent = agent();
        for (path, problem) in problems() {
            let budget = run_request(&problem, &agent)
                .expect("a request")
                .budget
                .expect("a budget");
            let limits = &agent.limits;
            assert!(budget.max_tool_calls <= limits.max_tool_calls, "{path}");
            assert!(
                budget.max_tool_calls <= problem.budgets.max_tool_calls,
                "{path}"
            );
            assert!(
                budget.max_wall_time_seconds <= limits.max_wall_time_seconds,
                "{path}"
            );
            assert!(
                budget.max_repair_attempts <= limits.max_repair_attempts,
                "{path}"
            );
            assert!(
                budget.max_repair_attempts <= problem.budgets.max_repair_attempts_per_step,
                "{path}"
            );
            assert_eq!(budget.max_tokens, limits.max_tokens, "{path}");
            assert_eq!(budget.max_steps, limits.max_steps, "{path}");
        }
    }

    #[test]
    fn wall_time_is_converted_to_whole_seconds() {
        let mut problem = problems().remove(0).1;
        let mut agent = agent();
        agent.limits.max_wall_time_seconds = std::num::NonZeroU64::new(86_400).expect("nonzero");
        let seconds = |problem: &BenchmarkProblem| {
            run_request(problem, &agent)
                .map(|r| r.budget.expect("a budget").max_wall_time_seconds.get())
        };

        problem.budgets.max_wall_time = quantity(10.0, "min");
        assert_eq!(seconds(&problem).expect("minutes"), 600);
        problem.budgets.max_wall_time = quantity(1.5, "h");
        assert_eq!(seconds(&problem).expect("hours"), 5400);
        problem.budgets.max_wall_time = quantity(2.9, "s");
        assert_eq!(seconds(&problem).expect("rounded down"), 2);

        problem.budgets.max_wall_time = quantity(10.0, "mm");
        let error = seconds(&problem).expect_err("not a time");
        assert_eq!(error.code(), ErrorCode::InvalidInput);
        problem.budgets.max_wall_time = quantity(0.5, "s");
        assert_eq!(
            seconds(&problem).expect_err("under a second").code(),
            ErrorCode::InvalidInput
        );
    }

    fn quantity(value: f64, unit: &str) -> contracts::types::Quantity {
        serde_json::from_value(serde_json::json!({ "value": value, "unit": unit }))
            .expect("a quantity")
    }
}
