//! Benchmark problems from `evals/benchmarks/` (blueprint section 14.7, ADR-0025).
//!
//! A file goes YAML → `schemas/benchmarks/benchmark-problem.schema.json` →
//! typed value, with the same strict YAML parser as configuration. Then come
//! the checks a schema cannot express: identifiers are unique, every
//! reference names something the problem declares, and every reference
//! expression parses. [`BenchmarkLoader::validate_dir`] also checks that each
//! problem sits at `<split>/<id>/problem.yaml` and that approval outcomes name
//! rules of the autonomy policy.

pub mod expression;

use std::collections::BTreeSet;
use std::path::Path;

use contracts::types::{BenchmarkProblem, OutcomeCheck};
use contracts::{Error, ErrorCode, SchemaSet};

use crate::config::{collect_files, read_text, relative_path, yaml_value};

/// The schema every problem file satisfies, relative to `schemas/`.
pub const SCHEMA: &str = "benchmarks/benchmark-problem.schema.json";

/// The file name of a problem inside its directory.
pub const PROBLEM_FILE: &str = "problem.yaml";

/// Validates and loads benchmark problems.
pub struct BenchmarkLoader {
    schemas: SchemaSet,
}

impl BenchmarkLoader {
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            schemas: SchemaSet::compile(&[SCHEMA])?,
        })
    }

    /// Reads and validates the problem at `path`.
    pub fn load(&self, path: &Path) -> Result<BenchmarkProblem, Error> {
        let origin = path.display().to_string();
        self.parse(&read_text(path)?, &origin)
    }

    /// Validates YAML text against the schema and the consistency rules.
    /// `origin` names it in errors, usually its path.
    pub fn parse(&self, text: &str, origin: &str) -> Result<BenchmarkProblem, Error> {
        let value = yaml_value(text, origin)?;
        let problems = self.schemas.errors(SCHEMA, &value)?;
        if !problems.is_empty() {
            return Err(invalid(format!(
                "{origin}: does not satisfy schemas/{SCHEMA}: {}",
                problems.join("; ")
            )));
        }
        let problem: BenchmarkProblem = serde_json::from_value(value).map_err(|e| {
            invalid(format!(
                "{origin}: does not match the benchmark problem type: {e}"
            ))
        })?;
        let inconsistent = consistency_problems(&problem);
        if !inconsistent.is_empty() {
            return Err(invalid(format!("{origin}: {}", inconsistent.join("; "))));
        }
        Ok(problem)
    }

    /// Validates every file under `benchmarks_dir` and returns one message
    /// per problem; empty means valid. A file anywhere but
    /// `<split>/<id>/problem.yaml` is a problem, so nothing goes unvalidated.
    /// `approval_rules` are the rule IDs of the autonomy policy. `Err` only
    /// if the directory cannot be read.
    pub fn validate_dir(
        &self,
        benchmarks_dir: &Path,
        approval_rules: &BTreeSet<String>,
    ) -> Result<Vec<String>, Error> {
        let mut files = Vec::new();
        collect_files(benchmarks_dir, &mut files)?;
        files.sort();
        let mut problems = Vec::new();
        for path in files {
            let relative = relative_path(benchmarks_dir, &path);
            let parts: Vec<&str> = relative.split('/').collect();
            let [split, id, PROBLEM_FILE] = parts.as_slice() else {
                problems.push(format!(
                    "{}: not a benchmark problem; problems live at <split>/<id>/{PROBLEM_FILE}",
                    path.display()
                ));
                continue;
            };
            let problem = match self.load(&path) {
                Ok(problem) => problem,
                Err(e) => {
                    problems.push(e.message().to_owned());
                    continue;
                }
            };
            let origin = path.display();
            if problem.split.to_string() != *split {
                problems.push(format!(
                    "{origin}: split is {} but the file is under {split}/",
                    problem.split
                ));
            }
            if problem.id.as_str() != *id {
                problems.push(format!(
                    "{origin}: id is {} but the directory is {id}",
                    problem.id.as_str()
                ));
            }
            for (at, rule) in approval_rule_references(&problem) {
                if !approval_rules.contains(rule) {
                    problems.push(format!(
                        "{origin}: {at}: approval rule {rule} is not in config/policies/autonomy.yaml"
                    ));
                }
            }
        }
        Ok(problems)
    }
}

fn invalid(message: String) -> Error {
    Error::new(ErrorCode::InvalidInput, message)
}

/// Where each check names an approval rule, as (location, rule ID).
fn approval_rule_references(problem: &BenchmarkProblem) -> Vec<(String, &str)> {
    let mut found = Vec::new();
    for (i, outcome) in problem.expected_outcomes.iter().enumerate() {
        for (j, check) in outcome.checks.iter().enumerate() {
            if let OutcomeCheck::ApprovalAnswered { approval_rule } = check {
                found.push((
                    format!("expected_outcomes/{i}/checks/{j}"),
                    approval_rule.as_str(),
                ));
            }
        }
    }
    found
}

/// What the schema cannot check: unique identifiers and references that
/// resolve. Messages start with the location in the file.
fn consistency_problems(problem: &BenchmarkProblem) -> Vec<String> {
    let mut problems = Vec::new();
    let mut unique = |what: &str, ids: Vec<&str>| -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        for id in ids {
            if !seen.insert(id.to_owned()) {
                problems.push(format!("{what}: {id} is declared twice"));
            }
        }
        seen
    };
    unique(
        "givens",
        problem.givens.iter().map(|g| g.id.as_str()).collect(),
    );
    let parameters = unique(
        "design_variables and derived_parameters",
        problem
            .design_variables
            .iter()
            .chain(&problem.derived_parameters)
            .map(|p| p.parameter_id.as_str())
            .collect(),
    );
    let requirements = unique(
        "requirements",
        problem.requirements.iter().map(|r| r.id.as_str()).collect(),
    );
    unique(
        "expected_outcomes",
        problem
            .expected_outcomes
            .iter()
            .map(|o| o.id.as_str())
            .collect(),
    );

    let parameter = |at: &str, id: &str, problems: &mut Vec<String>| {
        if !parameters.contains(id) {
            problems.push(format!("{at}: parameter {id} is not declared"));
        }
    };
    let requirement = |at: &str, id: &str, problems: &mut Vec<String>| {
        if !requirements.contains(id) {
            problems.push(format!("{at}: requirement {id} is not declared"));
        }
    };

    for (i, r) in problem.requirements.iter().enumerate() {
        parameter(
            &format!("requirements/{i}/numeric"),
            r.numeric.parameter_id.as_str(),
            &mut problems,
        );
    }
    for (i, outcome) in problem.expected_outcomes.iter().enumerate() {
        for (j, check) in outcome.checks.iter().enumerate() {
            let at = format!("expected_outcomes/{i}/checks/{j}");
            match check {
                OutcomeCheck::StopReason { .. } | OutcomeCheck::ApprovalAnswered { .. } => {}
                OutcomeCheck::ParametersPresent { parameter_ids } => {
                    for id in parameter_ids {
                        parameter(&at, id.as_str(), &mut problems);
                    }
                }
                OutcomeCheck::RequirementsPassing { requirement_ids } => {
                    for id in requirement_ids {
                        requirement(&at, id.as_str(), &mut problems);
                    }
                }
                OutcomeCheck::RequirementNeverPassing { requirement_id } => {
                    requirement(&at, requirement_id.as_str(), &mut problems);
                }
                OutcomeCheck::ParameterMatchesReference {
                    expression,
                    parameter_id,
                    variables,
                    ..
                } => {
                    parameter(&at, parameter_id.as_str(), &mut problems);
                    let mut names = BTreeSet::new();
                    for v in variables {
                        let id = v.parameter_id.as_str();
                        parameter(&at, id, &mut problems);
                        if id == "pi" {
                            problems.push(format!("{at}: a variable cannot be named pi"));
                        }
                        if !names.insert(id) {
                            problems.push(format!("{at}: variable {id} is listed twice"));
                        }
                    }
                    match expression::parse(expression.as_str()) {
                        Err(e) => problems.push(format!("{at}: expression: {e}")),
                        Ok(parsed) => {
                            for used in parsed.variables() {
                                if !names.contains(used) {
                                    problems.push(format!(
                                        "{at}: expression uses {used}, which is not in variables"
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    problems
}
