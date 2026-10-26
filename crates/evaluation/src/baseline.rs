//! The summary of repeated benchmark runs (blueprint section 16.4,
//! ADR-0035): how often each outcome passed, and the spread of what the runs
//! used, never a single score.

use std::num::NonZeroU64;

use contracts::ids::Identifier;
use contracts::types::{
    BaselineRun, BaselineSummary, BenchmarkProblem, OutcomeSummary, ScoreResult, Sha256, Spread,
};
use contracts::{Error, ErrorCode, canonical_sha256};

use crate::outcomes::all_passed;

/// The summary of `runs` of `problem`.
pub fn summarize(problem: &BenchmarkProblem, runs: &[BaselineRun]) -> BaselineSummary {
    let outcome_ids: Vec<&Identifier> = problem.expected_outcomes.iter().map(|o| &o.id).collect();
    let outcomes = outcome_ids
        .into_iter()
        .map(|id| {
            let results: Vec<ScoreResult> = runs
                .iter()
                .map(|run| {
                    run.outcomes
                        .iter()
                        .find(|o| &o.outcome_id == id)
                        .map_or(ScoreResult::Undecided, |o| o.result)
                })
                .collect();
            let count =
                |wanted: ScoreResult| results.iter().filter(|r| **r == wanted).count() as u64;
            let passed = count(ScoreResult::Pass);
            OutcomeSummary {
                outcome_id: id.clone(),
                passed,
                failed: count(ScoreResult::Fail),
                undecided: count(ScoreResult::Undecided),
                pass_rate: if runs.is_empty() {
                    0.0
                } else {
                    passed as f64 / runs.len() as f64
                },
            }
        })
        .collect();
    let used = |field: fn(&contracts::types::Usage) -> f64| -> Option<Spread> {
        spread(
            &runs
                .iter()
                .filter_map(|r| r.usage.as_ref().map(field))
                .collect::<Vec<_>>(),
        )
    };
    BaselineSummary {
        runs: runs.len() as u64,
        all_outcomes_passed: runs.iter().filter(|r| all_passed(&r.outcomes)).count() as u64,
        outcomes,
        input_tokens: used(|u| u.input_tokens as f64),
        output_tokens: used(|u| u.output_tokens as f64),
        tool_calls: used(|u| u.tool_calls as f64),
        wall_time_seconds: used(|u| u.wall_time_seconds),
        cost_usd: spread(&runs.iter().filter_map(|r| r.cost_usd).collect::<Vec<_>>()),
    }
}

/// The mean, sample standard deviation (n - 1; 0 for one value), minimum
/// and maximum of `values`; `None` for none.
pub fn spread(values: &[f64]) -> Option<Spread> {
    let n = NonZeroU64::new(values.len() as u64)?;
    let count = values.len() as f64;
    let mean = values.iter().sum::<f64>() / count;
    let std_dev = if values.len() < 2 {
        0.0
    } else {
        (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (count - 1.0)).sqrt()
    };
    Some(Spread {
        n,
        mean,
        std_dev,
        min: values.iter().copied().fold(f64::INFINITY, f64::min),
        max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    })
}

/// The canonical sha256 of `problem` as loaded, so a score names the exact
/// benchmark it was made on (blueprint section 9.4).
pub fn problem_sha256(problem: &BenchmarkProblem) -> Result<Sha256, Error> {
    let value = serde_json::to_value(problem)
        .map_err(|e| Error::new(ErrorCode::Internal, format!("a problem as JSON: {e}")))?;
    canonical_sha256(&value)
}
