//! Scoring the dev problems' expected outcomes (ADR-0035) on fixture
//! records: every check kind passing and failing, undecided when the record
//! lacks what a check needs, and the spread of a set of runs.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "integration tests")]

use std::path::Path;

use contracts::types::{
    BaselineRun, BenchmarkProblem, Event, OutcomeScore, RunReport, ScoreResult, SdrVersionView,
};
use evaluation::baseline::{problem_sha256, spread, summarize};
use evaluation::outcomes::{RunRecord, all_passed, score, undecided};
use serde_json::{Value, json};
use testkit::builders::parse;

fn repo() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

fn problem(id: &str) -> BenchmarkProblem {
    let text =
        std::fs::read_to_string(repo().join(format!("evals/benchmarks/dev/{id}/problem.yaml")))
            .expect("readable");
    let value: Value = serde_saphyr::from_str(&text).expect("YAML");
    serde_json::from_value(value).expect("a problem")
}

fn example(path: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(repo().join(path)).expect("readable"))
        .expect("JSON")
}

/// A check finding about `requirement` with `verdict`, at `version`.
fn finding(requirement: &str, version: u64, verdict: &str) -> Value {
    let mut evidence = example("schemas/sdr/examples/evidence.check.json");
    evidence["id"] = json!(format!("ev-{requirement}-v{version}"));
    evidence["sdr_version"] = json!(version);
    evidence["subject_ids"] = json!([requirement]);
    evidence["body"]["verdict"] = json!(verdict);
    evidence
}

/// Version `version` of `problem`'s design, with `parameters` as (id, value,
/// unit) and a check finding per (requirement, verdict).
fn view(
    problem: &BenchmarkProblem,
    version: u64,
    parameters: &[(&str, f64, &str)],
    verdicts: &[(&str, &str)],
) -> SdrVersionView {
    let evidence: Vec<Value> = verdicts
        .iter()
        .map(|(requirement, verdict)| finding(requirement, version, verdict))
        .collect();
    let ids: Vec<Value> = evidence.iter().map(|e| e["id"].clone()).collect();
    let requirements: Vec<Value> = problem
        .requirements
        .iter()
        .map(|r| {
            json!({ "id": r.id, "text": r.text, "priority": r.priority, "numeric": r.numeric,
                "evidence_ids": [] })
        })
        .collect();
    let parameters: Vec<Value> = parameters
        .iter()
        .map(|(id, value, unit)| {
            json!({ "id": id, "value": { "value": value, "unit": unit },
                "source": { "kind": "user" }, "evidence_ids": [] })
        })
        .collect();
    let mut record = example("schemas/sdr/examples/version.json");
    record["project_id"] = json!("p1");
    record["version"] = json!(version);
    record["evidence_ids"] = json!(ids);
    if version == 0 {
        record.as_object_mut().unwrap().remove("parent_version");
    } else {
        record["parent_version"] = json!(version - 1);
    }
    let mut document = json!({ "schema_version": "0.1", "project_id": "p1", "version": version,
        "requirements": requirements,
        "design": { "parameters": parameters, "decisions": [], "assumptions": [] } });
    if version > 0 {
        document["parent_version"] = json!(version - 1);
    }
    parse(
        json!({ "schema_version": "0.1", "document": document, "record": record,
        "evidence": evidence }),
    )
}

fn report(stop_reason: &str, final_version: u64) -> RunReport {
    let mut report = example("schemas/reports/examples/run-report.json");
    report["stop_reason"] = json!(stop_reason);
    report["final_sdr_version"] = json!(final_version);
    parse(report)
}

fn approval_events(rules: &[&str], answered: bool) -> Vec<Event> {
    let mut requested = example("schemas/events/examples/event.approval_requested.rules.json");
    requested["payload"]["rules"] = json!(rules);
    let mut events: Vec<Event> = vec![parse(requested)];
    if answered {
        events.push(parse(example(
            "schemas/events/examples/event.approval_answered.json",
        )));
    }
    events
}

/// The mass of a 110 x 70 x 6 mm plate by the reference formula, in grams.
const MASS_G: f64 = 122.589_736_908_250_44;

fn passing_001() -> (BenchmarkProblem, Vec<SdrVersionView>) {
    let problem = problem("part_design_001");
    let v0 = view(&problem, 0, &[], &[]);
    let v1 = view(
        &problem,
        1,
        &[
            ("L", 110.0, "mm"),
            ("W", 70.0, "mm"),
            ("T", 6.0, "mm"),
            ("mass", MASS_G, "g"),
        ],
        &[
            ("R1", "pass"),
            ("R2", "pass"),
            ("R3", "pass"),
            ("R4", "pass"),
        ],
    );
    (problem, vec![v0, v1])
}

fn by_id<'a>(scores: &'a [OutcomeScore], id: &str) -> &'a OutcomeScore {
    scores
        .iter()
        .find(|s| s.outcome_id.as_str() == id)
        .unwrap_or_else(|| panic!("no outcome {id}"))
}

#[test]
fn a_passing_run_of_the_solvable_problem_passes_every_outcome() {
    let (problem, versions) = passing_001();
    let report = report("success", 1);
    let events = approval_events(&["approve_final_design"], true);
    let scores = score(
        &problem,
        &RunRecord {
            report: &report,
            events: &events,
            versions: &versions,
        },
    )
    .expect("scores");
    assert_eq!(scores.len(), 4);
    for s in &scores {
        assert_eq!(s.result, ScoreResult::Pass, "{s:#?}");
    }
    assert!(all_passed(&scores));
}

#[test]
fn each_check_fails_on_the_record_that_breaks_it() {
    let (problem, mut versions) = passing_001();
    let events = approval_events(&["approve_final_design"], false);
    // The final version lacks T, the mass is 1 % off and R3 fails.
    versions[1] = view(
        &problem,
        1,
        &[("L", 110.0, "mm"), ("W", 70.0, "mm"), ("mass", MASS_G, "g")],
        &[
            ("R1", "pass"),
            ("R2", "pass"),
            ("R3", "fail"),
            ("R4", "pass"),
        ],
    );
    let failure = report("failure", 1);
    let scores = score(
        &problem,
        &RunRecord {
            report: &failure,
            events: &events,
            versions: &versions,
        },
    )
    .expect("scores");
    assert_eq!(by_id(&scores, "stop_reason").result, ScoreResult::Fail);
    let final_sdr = by_id(&scores, "final_sdr");
    assert_eq!(final_sdr.result, ScoreResult::Fail);
    assert_eq!(
        final_sdr.checks[0].result,
        ScoreResult::Fail,
        "T is missing"
    );
    assert!(final_sdr.checks[0].reason.contains('T'));
    assert_eq!(final_sdr.checks[1].result, ScoreResult::Fail, "R3 fails");
    assert!(
        final_sdr.checks[1].reason.contains("R3 is fail"),
        "{:?}",
        final_sdr.checks[1]
    );
    // Without T the reference cannot be computed.
    assert_eq!(
        by_id(&scores, "tool_mass_sanity").result,
        ScoreResult::Undecided
    );
    assert_eq!(
        by_id(&scores, "final_commit_approved").result,
        ScoreResult::Fail,
        "asked but never answered"
    );

    let unrelated = approval_events(&["some_other_rule"], true);
    let scores = score(
        &problem,
        &RunRecord {
            report: &failure,
            events: &unrelated,
            versions: &versions,
        },
    )
    .expect("scores");
    assert_eq!(
        by_id(&scores, "final_commit_approved").result,
        ScoreResult::Fail
    );
}

#[test]
fn the_mass_reference_holds_within_half_a_percent() {
    let (problem, mut versions) = passing_001();
    let report = report("success", 1);
    let events = approval_events(&["approve_final_design"], true);
    let parameters = |mass: f64, unit: &'static str| {
        [
            ("L", 110.0, "mm"),
            ("W", 7.0, "cm"),
            ("T", 6.0, "mm"),
            ("mass", mass, unit),
        ]
    };
    let sanity = |versions: &[SdrVersionView]| {
        let scores = score(
            &problem,
            &RunRecord {
                report: &report,
                events: &events,
                versions,
            },
        )
        .expect("scores");
        by_id(&scores, "tool_mass_sanity").result
    };
    let all_pass = [
        ("R1", "pass"),
        ("R2", "pass"),
        ("R3", "pass"),
        ("R4", "pass"),
    ];
    // W in cm and mass in kg are converted before comparing.
    versions[1] = view(&problem, 1, &parameters(MASS_G / 1000.0, "kg"), &all_pass);
    assert_eq!(sanity(&versions), ScoreResult::Pass);
    versions[1] = view(&problem, 1, &parameters(MASS_G * 1.004, "g"), &all_pass);
    assert_eq!(
        sanity(&versions),
        ScoreResult::Pass,
        "0.4 % is within 0.5 %"
    );
    versions[1] = view(&problem, 1, &parameters(MASS_G * 1.006, "g"), &all_pass);
    assert_eq!(sanity(&versions), ScoreResult::Fail, "0.6 % is not");
    versions[1] = view(&problem, 1, &parameters(MASS_G, "mm"), &all_pass);
    assert_eq!(sanity(&versions), ScoreResult::Undecided, "a mass in mm");
}

#[test]
fn the_infeasible_problem_fails_if_any_version_shows_r4_passing() {
    let problem = problem("part_design_002");
    let heavy = [
        ("L", 110.0, "mm"),
        ("W", 70.0, "mm"),
        ("T", 6.0, "mm"),
        ("mass", MASS_G, "g"),
    ];
    let v0 = view(&problem, 0, &[], &[]);
    let v1 = view(&problem, 1, &heavy, &[("R3", "pass"), ("R4", "fail")]);
    let v2 = view(&problem, 2, &heavy, &[("R4", "pass")]);
    let budget = report("budget_exhausted", 1);
    let never = |report: &RunReport, versions: &[SdrVersionView]| {
        score(
            &problem,
            &RunRecord {
                report,
                events: &[],
                versions,
            },
        )
        .expect("scores")
    };

    let scores = never(&budget, &[v0.clone(), v1.clone()]);
    assert!(all_passed(&scores), "{scores:#?}");

    let claimed = report("failure", 2);
    let scores = never(&claimed, &[v0.clone(), v1.clone(), v2]);
    let r4 = by_id(&scores, "r4_never_passing");
    assert_eq!(r4.result, ScoreResult::Fail);
    assert!(r4.checks[0].reason.contains("version 2"), "{r4:?}");

    let gap = never(&claimed, &[v0, v1]);
    assert_eq!(
        by_id(&gap, "r4_never_passing").result,
        ScoreResult::Undecided,
        "version 2 was not read"
    );

    let success = report("success", 1);
    let scores = never(
        &success,
        &[view(&problem, 0, &[], &[]), view(&problem, 1, &heavy, &[])],
    );
    assert_eq!(by_id(&scores, "stop_reason").result, ScoreResult::Fail);
}

#[test]
fn a_run_without_a_report_is_undecided_throughout() {
    let problem = problem("part_design_001");
    let scores = undecided(&problem, "the run did not stop in time").expect("scores");
    assert_eq!(scores.len(), 4);
    for s in &scores {
        assert_eq!(s.result, ScoreResult::Undecided);
        assert!(s.checks.iter().all(|c| c.reason.contains("did not stop")));
    }
    assert!(!all_passed(&scores));
}

#[test]
fn a_summary_counts_outcomes_and_reports_spread() {
    let (problem, versions) = passing_001();
    let report = report("success", 1);
    let events = approval_events(&["approve_final_design"], true);
    let passed = score(
        &problem,
        &RunRecord {
            report: &report,
            events: &events,
            versions: &versions,
        },
    )
    .expect("scores");
    let run = |id: &str, outcomes: Vec<OutcomeScore>, tokens: Option<u64>, cost: Option<f64>| {
        let mut run = json!({ "run_id": id, "outcomes": outcomes, "model_ids": ["m"] });
        if let Some(tokens) = tokens {
            run["usage"] = json!({ "input_tokens": tokens, "output_tokens": 10, "tool_calls": 4,
                "steps": 5, "wall_time_seconds": 30.0 });
            run["stop_reason"] = json!("success");
        }
        if let Some(cost) = cost {
            run["cost_usd"] = json!(cost);
        }
        parse::<BaselineRun>(run)
    };
    let runs = vec![
        run("run-a", passed.clone(), Some(1000), Some(0.01)),
        run("run-b", passed, Some(3000), None),
        run(
            "run-c",
            undecided(&problem, "timed out").expect("scores"),
            None,
            None,
        ),
    ];
    let summary = summarize(&problem, &runs);
    assert_eq!(summary.runs, 3);
    assert_eq!(summary.all_outcomes_passed, 2);
    let stop = &summary.outcomes[0];
    assert_eq!((stop.passed, stop.failed, stop.undecided), (2, 0, 1));
    assert!((stop.pass_rate - 2.0 / 3.0).abs() < 1e-12);
    let tokens = summary.input_tokens.expect("two runs with usage");
    assert_eq!(tokens.n.get(), 2);
    assert_eq!(
        (tokens.mean, tokens.min, tokens.max),
        (2000.0, 1000.0, 3000.0)
    );
    assert!(
        (tokens.std_dev - 2f64.sqrt() * 1000.0).abs() < 1e-9,
        "{tokens:?}"
    );
    assert_eq!(summary.cost_usd.expect("one priced run").n.get(), 1);

    assert!(spread(&[]).is_none());
    assert_eq!(spread(&[4.0]).expect("one").std_dev, 0.0);
    let hash = problem_sha256(&problem).expect("a hash");
    assert_eq!(hash, problem_sha256(&problem).expect("a hash"), "stable");
    assert_ne!(
        hash,
        problem_sha256(&self::problem("part_design_002")).expect("a hash")
    );
}
