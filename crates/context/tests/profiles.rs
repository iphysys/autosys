//! Context profiles (ADR-0029): the committed config is valid, and a profile
//! that breaks the ceiling or its own total is rejected with every problem.

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use context::{ContextError, ContextProfiles, DEFAULT_PROFILE};
use serde_json::json;
use support::*;

fn rejected(result: Result<ContextProfiles, ContextError>) -> Vec<String> {
    match result {
        Err(ContextError::InvalidProfile { problems }) => problems,
        other => panic!("expected InvalidProfile, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn the_committed_config_is_valid_and_starts_at_the_ceiling() {
    let (budgets, policy) = committed_config();
    let profiles = ContextProfiles::new(budgets, &policy).expect("valid");
    assert_eq!(profiles.names().collect::<Vec<_>>(), ["default"]);
    let default = profiles.resolve(None).expect("default");
    assert_eq!(default.name.as_str(), DEFAULT_PROFILE);
    assert_eq!(profiles.ceiling(), 24_000);
    assert_eq!(default.total_tokens.get(), profiles.ceiling());
    assert_eq!(default.bytes_per_token.get(), 3);
    assert_eq!(default.history_window_events.get(), 200);
    assert_eq!(default.io_timeout_seconds.get(), 10);
    // The caps of the fixed sections fit the total; the trimmable ones give way.
    let s = &default.sections;
    let fixed = s.instructions.get()
        + s.task_contract.get()
        + s.job_state.get()
        + s.design_state.get()
        + s.open_turn.get();
    assert_eq!(fixed, 19_000);
    assert!(fixed <= default.total_tokens.get());
}

#[test]
fn the_sections_of_a_profile_are_exactly_the_context_sections() {
    // The schema lists the sections twice, as an enum and as the fields of a
    // profile. They must agree; the compiler checks the code, this checks the schema.
    let names = [
        "instructions",
        "task_contract",
        "job_state",
        "design_state",
        "run_history",
        "untrusted_content",
        "open_turn",
    ];
    for name in names {
        let section: contracts::types::ContextSectionName =
            serde_json::from_value(json!(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(section.to_string(), name);
    }
    let sections = serde_json::to_value(&default_profile().sections).expect("json");
    let mut keys: Vec<&str> = sections
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    let mut expected = names.to_vec();
    expected.sort_unstable();
    assert_eq!(keys, expected);
}

#[test]
fn fixed_caps_above_the_total_are_rejected() {
    // 3000 + 1500 + 500 + 8000 + 6000 = 19000 against a total of 18000.
    let problems = rejected(profiles_from(
        vec![profile_json("default", 18_000, json!({}))],
        24_000,
    ));
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("profile `default`"), "{problems:?}");
    assert!(problems[0].contains("19000"), "{problems:?}");
    assert!(problems[0].contains("18000"), "{problems:?}");
    assert!(
        problems[0].contains("open_turn"),
        "the fixed sections are named: {problems:?}"
    );
}

#[test]
fn a_total_above_the_ceiling_is_rejected() {
    let problems = rejected(profiles_from(
        vec![profile_json("default", 30_000, json!({}))],
        24_000,
    ));
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("30000") && problems[0].contains("ceiling 24000"));
    // The ceiling is a ceiling: equal is fine.
    profiles_from(vec![profile_json("default", 24_000, json!({}))], 24_000)
        .expect("at the ceiling");
    // Lowering below it is what the improvement loop may do.
    profiles_from(
        vec![profile_json(
            "default",
            20_000,
            json!({ "design_state": 5000 }),
        )],
        24_000,
    )
    .expect("below the ceiling");
}

#[test]
fn a_missing_default_and_a_duplicate_are_rejected() {
    let problems = rejected(profiles_from(
        vec![profile_json("small", 24_000, json!({}))],
        24_000,
    ));
    assert_eq!(problems, ["there is no profile named `default`"]);

    let problems = rejected(profiles_from(
        vec![
            profile_json("default", 24_000, json!({})),
            profile_json("default", 24_000, json!({})),
        ],
        24_000,
    ));
    assert_eq!(problems, ["profile `default` is defined more than once"]);
}

#[test]
fn every_problem_is_reported_at_once() {
    let problems = rejected(profiles_from(
        vec![
            profile_json("a", 30_000, json!({})),
            profile_json("b", 10_000, json!({})),
        ],
        24_000,
    ));
    // `a` is above the ceiling, `b` cannot hold its fixed sections, and there is no default.
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(
        problems
            .iter()
            .any(|p| p.contains("profile `a`") && p.contains("ceiling"))
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("profile `b`") && p.contains("fixed sections"))
    );
    assert!(problems.iter().any(|p| p.contains("`default`")));
}

#[test]
fn profiles_resolve_by_name_with_default_as_the_fallback() {
    let profiles = profiles_from(
        vec![
            profile_json("default", 24_000, json!({})),
            profile_json(
                "lean",
                12_000,
                json!({
                    "instructions": 2000, "task_contract": 1000, "job_state": 400,
                    "design_state": 4000, "run_history": 1500, "untrusted_content": 2000,
                    "open_turn": 3000
                }),
            ),
        ],
        24_000,
    )
    .expect("valid");
    assert_eq!(
        profiles.resolve(None).expect("default").name.as_str(),
        "default"
    );
    assert_eq!(
        profiles
            .resolve(Some("lean"))
            .expect("lean")
            .total_tokens
            .get(),
        12_000
    );
    match profiles.resolve(Some("huge")) {
        Err(ContextError::UnknownProfile { name, available }) => {
            assert_eq!(name, "huge");
            assert_eq!(available, ["default", "lean"]);
        }
        other => panic!(
            "expected UnknownProfile, got {:?}",
            other.map(|p| p.name.clone())
        ),
    }
}
