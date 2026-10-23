//! Benchmark problems: the dev benchmarks in `evals/benchmarks/` validate,
//! encode the source specification's numbers, and broken problems are
//! reported. Needs no database.

#![allow(clippy::expect_used, reason = "integration tests")]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use contracts::types::{AutonomyPolicy, BenchmarkProblem, OutcomeCheck};
use infra::benchmarks::{BenchmarkLoader, expression};
use infra::config::ConfigLoader;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn loader() -> BenchmarkLoader {
    BenchmarkLoader::new().expect("benchmark schema compiles")
}

fn approval_rules() -> BTreeSet<String> {
    let policy: AutonomyPolicy = ConfigLoader::new()
        .expect("config schemas compile")
        .load(&repo().join("config/policies/autonomy.yaml"))
        .expect("autonomy policy");
    policy.approvals.iter().map(|r| r.id.to_string()).collect()
}

fn dev_problem(id: &str) -> BenchmarkProblem {
    loader()
        .load(&repo().join(format!("evals/benchmarks/dev/{id}/problem.yaml")))
        .expect("dev problem loads")
}

#[test]
fn the_dev_benchmarks_are_valid() {
    let problems = loader()
        .validate_dir(&repo().join("evals/benchmarks"), &approval_rules())
        .expect("evals/benchmarks/ is readable");
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The mass reference of part_design_001, evaluated on the draft's worked
/// examples, gives the masses the draft quotes (to one decimal, as written).
#[test]
fn the_reference_mass_reproduces_the_drafts_worked_examples() {
    let problem = dev_problem("part_design_001");
    let reference = problem
        .expected_outcomes
        .iter()
        .flat_map(|o| &o.checks)
        .find_map(|check| match check {
            OutcomeCheck::ParameterMatchesReference { expression, .. } => Some(expression),
            _ => None,
        })
        .expect("part_design_001 has a mass reference");
    let parsed = expression::parse(reference.as_str()).expect("the reference parses");
    for (length, width, thickness, quoted) in [
        (120.0, 80.0, 6.0, 153.4),
        (120.0, 70.0, 6.0, 133.9),
        (110.0, 70.0, 6.0, 122.6),
        (112.0, 72.0, 6.0, 128.5),
    ] {
        let values = BTreeMap::from([("L", length), ("W", width), ("T", thickness)]);
        let mass = parsed.evaluate(&values).expect("finite");
        assert_eq!(
            (mass * 10.0).round() / 10.0,
            quoted,
            "{length} x {width} x {thickness}"
        );
    }
}

/// part_design_002 is part_design_001 with only R4 changed.
#[test]
fn the_infeasible_problem_differs_only_in_r4() {
    let solvable = dev_problem("part_design_001");
    let infeasible = dev_problem("part_design_002");
    assert_eq!(solvable.givens, infeasible.givens);
    assert_eq!(solvable.design_variables, infeasible.design_variables);
    assert_eq!(solvable.derived_parameters, infeasible.derived_parameters);
    assert_eq!(solvable.budgets, infeasible.budgets);
    assert_eq!(solvable.requirements[..3], infeasible.requirements[..3]);
    assert_eq!(solvable.requirements.len(), 4);
    assert_eq!(infeasible.requirements.len(), 4);
    assert_ne!(solvable.requirements[3], infeasible.requirements[3]);
}

fn write_problem(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

fn problems_for(edit: impl Fn(&str) -> String) -> Vec<String> {
    let text = fs::read_to_string(repo().join("evals/benchmarks/dev/part_design_001/problem.yaml"))
        .expect("readable");
    let dir = tempfile::tempdir().expect("temp dir");
    write_problem(dir.path(), "dev/part_design_001/problem.yaml", &edit(&text));
    loader()
        .validate_dir(dir.path(), &approval_rules())
        .expect("readable")
}

#[test]
fn an_unchanged_copy_is_valid() {
    assert_eq!(problems_for(str::to_owned), Vec::<String>::new());
}

#[test]
fn undeclared_references_are_reported() {
    let problems = problems_for(|t| t.replace("[R1, R2, R3, R4]", "[R1, R2, R3, R5]"));
    assert!(
        problems[0].contains("requirement R5 is not declared"),
        "{problems:#?}"
    );

    let problems = problems_for(|t| t.replace("[L, W, T, mass]", "[L, W, T, weight]"));
    assert!(
        problems[0].contains("parameter weight is not declared"),
        "{problems:#?}"
    );

    let problems = problems_for(|t| t.replace("* 3.25^2) * T\"", "* 3.25^2) * D\""));
    assert!(problems[0].contains("expression uses D"), "{problems:#?}");

    let problems = problems_for(|t| t.replace("approve_final_design", "approve_everything"));
    assert!(
        problems[0].contains("approval rule approve_everything"),
        "{problems:#?}"
    );
}

#[test]
fn malformed_expressions_and_duplicates_are_reported() {
    let problems = problems_for(|t| t.replace("* 3.25^2) * T\"", "* 3.25^2 * T\""));
    assert!(problems[0].contains("expression: "), "{problems:#?}");

    let problems = problems_for(|t| t.replace("  - id: R4\n", "  - id: R3\n"));
    assert!(
        problems[0].contains("R3 is declared twice"),
        "{problems:#?}"
    );
}

#[test]
fn misplaced_files_and_mismatched_names_are_reported() {
    let text = fs::read_to_string(repo().join("evals/benchmarks/dev/part_design_001/problem.yaml"))
        .expect("readable");
    let dir = tempfile::tempdir().expect("temp dir");
    write_problem(dir.path(), "dev/renamed/problem.yaml", &text);
    write_problem(dir.path(), "dev/part_design_001/notes.md", "notes\n");
    write_problem(dir.path(), "problem.yaml", &text);
    let problems = loader()
        .validate_dir(dir.path(), &approval_rules())
        .expect("readable");
    assert_eq!(problems.len(), 3, "{problems:#?}");
    assert!(
        problems
            .iter()
            .any(|p| p.contains("id is part_design_001 but the directory is renamed"))
    );
    assert_eq!(
        problems
            .iter()
            .filter(|p| p.contains("not a benchmark problem"))
            .count(),
        2,
        "{problems:#?}"
    );
}
