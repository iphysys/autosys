//! Budgets (blueprint section 5.4, ADR-0030). The effective budget is the
//! task's, which the factory checked against the agent's limits.
//!
//! - Tokens: input plus output, from each response's usage.
//! - Tool calls: every tool call the loop handles, the SDR tools and denied
//!   calls included.
//! - Steps: one per model call.
//! - Wall time: see [`crate::cancellation::Deadline`].

use contracts::types::{Budget, Usage};

/// Which limit a run used up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Exhausted {
    Tokens,
    ToolCalls,
    Steps,
    WallTime,
}

impl Exhausted {
    /// The code of the `error` event and of the result's error, as in
    /// `schemas/results/examples/result.budget-exhausted.json`.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Exhausted::Tokens => "budget.tokens_exhausted",
            Exhausted::ToolCalls => "budget.tool_calls_exhausted",
            Exhausted::Steps => "budget.steps_exhausted",
            Exhausted::WallTime => "budget.wall_time_exhausted",
        }
    }

    pub(crate) fn message(self, budget: &Budget, used: &Usage) -> String {
        match self {
            Exhausted::Tokens => format!(
                "The token budget of {} is used up: {} used.",
                budget.max_tokens,
                tokens_used(used)
            ),
            Exhausted::ToolCalls => format!(
                "The tool-call budget of {} is used up.",
                budget.max_tool_calls
            ),
            Exhausted::Steps => format!("The step budget of {} is used up.", budget.max_steps),
            Exhausted::WallTime => format!(
                "The wall-time budget of {} s is used up: {:.1} s used.",
                budget.max_wall_time_seconds, used.wall_time_seconds
            ),
        }
    }
}

pub(crate) fn tokens_used(used: &Usage) -> u64 {
    used.input_tokens.saturating_add(used.output_tokens)
}

/// What stops a run before its next model call, steps first. With no tool
/// call left the model could neither act nor commit, so the call is not
/// made.
pub(crate) fn before_step(budget: &Budget, used: &Usage) -> Option<Exhausted> {
    if used.steps >= budget.max_steps.get() {
        Some(Exhausted::Steps)
    } else if tokens_used(used) >= budget.max_tokens.get() {
        Some(Exhausted::Tokens)
    } else if used.tool_calls >= budget.max_tool_calls.get() {
        Some(Exhausted::ToolCalls)
    } else {
        None
    }
}

/// The most output the next call may ask for, given the estimate of its
/// context: `min(profile max, max_tokens − used − estimate)`. `None` when
/// `used + estimate` already reaches `max_tokens`.
pub(crate) fn output_limit(
    budget: &Budget,
    used: &Usage,
    estimate: u64,
    profile_max: u64,
) -> Option<u32> {
    let left = budget
        .max_tokens
        .get()
        .checked_sub(tokens_used(used).saturating_add(estimate))
        .filter(|left| *left > 0)?;
    Some(u32::try_from(left.min(profile_max)).unwrap_or(u32::MAX))
}

/// Whether `failures` since the last commit exceed the repair bound. The
/// bound counts repairs, so the run fails on the first failure beyond it.
pub(crate) fn repairs_exhausted(budget: &Budget, failures: u64) -> bool {
    failures > budget.max_repair_attempts.get()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn budget() -> Budget {
        serde_json::from_value(json!({
            "max_tokens": 1000,
            "max_tool_calls": 5,
            "max_steps": 3,
            "max_wall_time_seconds": 60,
            "max_repair_attempts": 2
        }))
        .expect("budget")
    }

    fn used(input: u64, output: u64, steps: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            tool_calls: 0,
            steps,
            wall_time_seconds: 0.0,
        }
    }

    #[test]
    fn steps_and_tokens_stop_a_run_before_its_next_call() {
        assert_eq!(before_step(&budget(), &used(0, 0, 0)), None);
        assert_eq!(
            before_step(&budget(), &used(0, 0, 3)),
            Some(Exhausted::Steps)
        );
        assert_eq!(
            before_step(&budget(), &used(900, 100, 1)),
            Some(Exhausted::Tokens)
        );
        let spent_calls = Usage {
            tool_calls: 5,
            ..used(0, 0, 1)
        };
        assert_eq!(
            before_step(&budget(), &spent_calls),
            Some(Exhausted::ToolCalls)
        );
    }

    #[test]
    fn the_output_limit_is_what_the_budget_and_the_profile_leave() {
        let b = budget();
        assert_eq!(output_limit(&b, &used(100, 50, 1), 200, 16_000), Some(650));
        assert_eq!(output_limit(&b, &used(0, 0, 0), 10, 300), Some(300));
        assert_eq!(output_limit(&b, &used(500, 0, 1), 500, 300), None);
        assert_eq!(output_limit(&b, &used(800, 0, 1), 500, 300), None);
    }

    #[test]
    fn the_repair_bound_fails_on_the_first_failure_beyond_it() {
        assert!(!repairs_exhausted(&budget(), 2));
        assert!(repairs_exhausted(&budget(), 3));
    }

    #[test]
    fn codes_follow_the_result_example() {
        let codes: Vec<&str> = [
            Exhausted::Tokens,
            Exhausted::ToolCalls,
            Exhausted::Steps,
            Exhausted::WallTime,
        ]
        .into_iter()
        .map(Exhausted::code)
        .collect();
        assert_eq!(
            codes,
            [
                "budget.tokens_exhausted",
                "budget.tool_calls_exhausted",
                "budget.steps_exhausted",
                "budget.wall_time_exhausted"
            ]
        );
        for code in codes {
            assert!(code.parse::<contracts::ids::Identifier>().is_ok(), "{code}");
        }
    }
}
