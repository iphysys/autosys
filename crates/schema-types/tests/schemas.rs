//! Checks the schemas in `schemas/` and their example instances.
//!
//! Examples live in `schemas/<area>/examples/<stem>[.<case>].json` and must
//! validate against `schemas/<area>/<stem>.schema.json`; files under
//! `examples/invalid/` must be rejected. Every valid example must also
//! deserialize into its generated Rust type and serialize back unchanged,
//! which shows typify mapped the schema faithfully.

// Test code: clippy's allow-expect-in-tests covers `#[test]` functions only,
// not the helpers of an integration test.
#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::{Draft, Registry};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Local base URI so relative `$ref`s resolve from the registry, never the network.
const BASE_URI: &str = "file:///autosys/schemas/";

type RoundTrip = fn(&Value) -> Result<Value, String>;

/// Every schema file with a root type, and the Rust type generated for it.
const ROOT_TYPES: &[(&str, RoundTrip)] = &[
    (
        "api/approval-answer.schema.json",
        round_trip::<schema_types::ApprovalAnswerRequest>,
    ),
    (
        "api/cancel.schema.json",
        round_trip::<schema_types::CancelRequest>,
    ),
    (
        "api/error.schema.json",
        round_trip::<schema_types::ApiError>,
    ),
    (
        "api/run-request.schema.json",
        round_trip::<schema_types::RunRequest>,
    ),
    ("api/run.schema.json", round_trip::<schema_types::Run>),
    (
        "api/sdr-version.schema.json",
        round_trip::<schema_types::SdrVersionView>,
    ),
    (
        "artifacts/artifact.schema.json",
        round_trip::<schema_types::ArtifactMetadata>,
    ),
    (
        "benchmarks/benchmark-problem.schema.json",
        round_trip::<schema_types::BenchmarkProblem>,
    ),
    (
        "config/agent.schema.json",
        round_trip::<schema_types::AgentSpec>,
    ),
    (
        "config/api.schema.json",
        round_trip::<schema_types::ApiSettings>,
    ),
    (
        "config/autonomy-policy.schema.json",
        round_trip::<schema_types::AutonomyPolicy>,
    ),
    (
        "config/context-budgets.schema.json",
        round_trip::<schema_types::ContextBudgets>,
    ),
    (
        "config/context-policy.schema.json",
        round_trip::<schema_types::ContextPolicy>,
    ),
    (
        "config/gates-policy.schema.json",
        round_trip::<schema_types::GatesPolicy>,
    ),
    (
        "config/model-providers/openai-compatible.schema.json",
        round_trip::<schema_types::OpenAiCompatibleProviderSettings>,
    ),
    (
        "config/model-routing.schema.json",
        round_trip::<schema_types::ModelRouting>,
    ),
    (
        "config/runtime.schema.json",
        round_trip::<schema_types::RuntimeSettings>,
    ),
    (
        "config/tool-overlay.schema.json",
        round_trip::<schema_types::ToolOverlay>,
    ),
    (
        "config/worker.schema.json",
        round_trip::<schema_types::WorkerSettings>,
    ),
    (
        "events/event.schema.json",
        round_trip::<schema_types::Event>,
    ),
    (
        "jobs/continuation.schema.json",
        round_trip::<schema_types::ContinuationRecord>,
    ),
    ("jobs/job.schema.json", round_trip::<schema_types::Job>),
    (
        "jobs/step-record.schema.json",
        round_trip::<schema_types::StepRecord>,
    ),
    (
        "prompts/prompt-header.schema.json",
        round_trip::<schema_types::PromptHeader>,
    ),
    (
        "replay/model-call.schema.json",
        round_trip::<schema_types::ModelCallRecord>,
    ),
    (
        "reports/baseline.schema.json",
        round_trip::<schema_types::Baseline>,
    ),
    (
        "reports/run-report.schema.json",
        round_trip::<schema_types::RunReport>,
    ),
    (
        "results/result.schema.json",
        round_trip::<schema_types::TaskResult>,
    ),
    (
        "sdr/design.schema.json",
        round_trip::<schema_types::DesignDefinition>,
    ),
    (
        "sdr/evidence.schema.json",
        round_trip::<schema_types::Evidence>,
    ),
    (
        "sdr/requirement.schema.json",
        round_trip::<schema_types::Requirement>,
    ),
    (
        "sdr/system.schema.json",
        round_trip::<schema_types::SdrDocument>,
    ),
    (
        "sdr/transition.schema.json",
        round_trip::<schema_types::Transition>,
    ),
    (
        "sdr/version.schema.json",
        round_trip::<schema_types::SdrVersionRecord>,
    ),
    ("tasks/task.schema.json", round_trip::<schema_types::Task>),
    (
        "tools/build123d/create_plate_request.schema.json",
        round_trip::<schema_types::Build123dCreatePlateRequest>,
    ),
    (
        "tools/build123d/create_plate_result.schema.json",
        round_trip::<schema_types::Build123dCreatePlateResult>,
    ),
    (
        "tools/build123d/measure_request.schema.json",
        round_trip::<schema_types::Build123dMeasureRequest>,
    ),
    (
        "tools/build123d/measure_result.schema.json",
        round_trip::<schema_types::Build123dMeasureResult>,
    ),
    (
        "tools/build123d/part.schema.json",
        round_trip::<schema_types::Build123dPart>,
    ),
    (
        "tools/sdr/sdr_propose_request.schema.json",
        round_trip::<schema_types::SdrProposeRequest>,
    ),
    (
        "tools/sdr/sdr_propose_result.schema.json",
        round_trip::<schema_types::SdrProposeResult>,
    ),
    (
        "tools/sdr/sdr_read_request.schema.json",
        round_trip::<schema_types::SdrReadRequest>,
    ),
    (
        "tools/sdr/sdr_read_result.schema.json",
        round_trip::<schema_types::SdrReadResult>,
    ),
    (
        "tools/tool-result.schema.json",
        round_trip::<schema_types::ToolResultContent>,
    ),
];

fn round_trip<T: DeserializeOwned + Serialize>(instance: &Value) -> Result<Value, String> {
    let typed: T = serde_json::from_value(instance.clone()).map_err(|e| e.to_string())?;
    serde_json::to_value(&typed).map_err(|e| e.to_string())
}

fn schemas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas")
}

/// Path relative to `schemas/` with `/` separators.
fn rel(path: &Path) -> String {
    path.strip_prefix(schemas_dir())
        .expect("path is under schemas/")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("readable directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).expect("readable file");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: invalid JSON: {e}", rel(path)))
}

/// All schema files, keyed by path relative to `schemas/`.
fn load_schemas() -> BTreeMap<String, Value> {
    let mut paths = Vec::new();
    files_under(&schemas_dir(), &mut paths);
    paths
        .iter()
        .filter(|p| !rel(p).contains("/examples/"))
        .filter(|p| p.to_string_lossy().ends_with(".schema.json"))
        .map(|p| (rel(p), read_json(p)))
        .collect()
}

struct Example {
    rel_path: String,
    schema: String,
    valid: bool,
    instance: Value,
}

fn load_examples() -> Vec<Example> {
    let mut paths = Vec::new();
    files_under(&schemas_dir(), &mut paths);
    paths.sort();
    paths
        .iter()
        .filter(|p| rel(p).contains("/examples/"))
        .map(|p| {
            let rel_path = rel(p);
            let (area, rest) = rel_path
                .split_once("/examples/")
                .expect("example path contains /examples/");
            let (valid, file_name) = match rest.strip_prefix("invalid/") {
                Some(name) => (false, name),
                None => (true, rest),
            };
            assert!(
                file_name.ends_with(".json") && !file_name.contains('/'),
                "{rel_path}: examples must be .json files directly in examples/ or examples/invalid/"
            );
            let stem = file_name.split('.').next().expect("file name has a stem");
            Example {
                schema: format!("{area}/{stem}.schema.json"),
                rel_path,
                valid,
                instance: read_json(p),
            }
        })
        .collect()
}

/// JSON equality that treats `2` and `2.0` as equal, since `number` fields deserialize to `f64`.
fn same_json(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(xs), Value::Array(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| same_json(x, y))
        }
        (Value::Object(xs), Value::Object(ys)) => {
            xs.len() == ys.len()
                && xs
                    .iter()
                    .all(|(k, x)| ys.get(k).is_some_and(|y| same_json(x, y)))
        }
        _ => a == b,
    }
}

#[test]
fn every_schema_is_valid_draft_2020_12() {
    let failures: Vec<String> = load_schemas()
        .iter()
        .filter_map(|(path, schema)| {
            jsonschema::meta::validate(schema)
                .err()
                .map(|e| format!("{path}: {e}"))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "invalid schemas:\n{}",
        failures.join("\n")
    );
}

#[test]
fn every_root_schema_has_a_rust_type_and_examples() {
    let schemas = load_schemas();
    let examples = load_examples();
    let roots: Vec<&String> = schemas
        .iter()
        .filter(|(_, s)| s.get("title").is_some())
        .map(|(p, _)| p)
        .collect();

    let mapped: Vec<&str> = ROOT_TYPES.iter().map(|(p, _)| *p).collect();
    let roots_as_str: Vec<&str> = roots.iter().map(|p| p.as_str()).collect();
    assert_eq!(
        roots_as_str, mapped,
        "ROOT_TYPES must list exactly the schema files that have a root type"
    );

    let missing: Vec<&str> = roots_as_str
        .iter()
        .filter(|root| !examples.iter().any(|e| e.valid && e.schema == **root))
        .copied()
        .collect();
    assert!(
        missing.is_empty(),
        "schemas without a valid example: {missing:?}"
    );
}

#[test]
fn examples_validate_and_invalid_examples_are_rejected() {
    let schemas = load_schemas();
    let registry = Registry::new()
        .extend(
            schemas
                .iter()
                .map(|(path, schema)| (format!("{BASE_URI}{path}"), schema.clone())),
        )
        .expect("schema URIs are valid")
        .prepare()
        .expect("registry builds from local schemas");

    let mut failures = Vec::new();
    let examples = load_examples();
    assert!(!examples.is_empty(), "no examples found");
    for example in &examples {
        let Some(schema) = schemas.get(&example.schema) else {
            failures.push(format!(
                "{}: no schema at {}",
                example.rel_path, example.schema
            ));
            continue;
        };
        let validator = jsonschema::options()
            .with_draft(Draft::Draft202012)
            .with_base_uri(format!("{BASE_URI}{}", example.schema))
            .with_registry(&registry)
            .should_validate_formats(true)
            .build(schema)
            .unwrap_or_else(|e| panic!("{}: cannot compile: {e}", example.schema));
        let errors: Vec<String> = validator
            .iter_errors(&example.instance)
            .map(|e| format!("{} at {}", e, e.instance_path()))
            .collect();
        match (example.valid, errors.is_empty()) {
            (true, false) => failures.push(format!("{}: {}", example.rel_path, errors.join("; "))),
            (false, true) => failures.push(format!(
                "{}: invalid example was accepted",
                example.rel_path
            )),
            _ => {}
        }
    }
    assert!(
        failures.is_empty(),
        "example failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn examples_round_trip_through_rust_types() {
    let mut failures = Vec::new();
    for example in load_examples().iter().filter(|e| e.valid) {
        let Some((_, round_trip)) = ROOT_TYPES.iter().find(|(p, _)| *p == example.schema) else {
            failures.push(format!(
                "{}: no Rust type for {}",
                example.rel_path, example.schema
            ));
            continue;
        };
        match round_trip(&example.instance) {
            Ok(back) if same_json(&back, &example.instance) => {}
            Ok(back) => failures.push(format!(
                "{}: changed on round trip:\n  before: {}\n  after:  {}",
                example.rel_path, example.instance, back
            )),
            Err(e) => failures.push(format!("{}: does not deserialize: {e}", example.rel_path)),
        }
    }
    assert!(
        failures.is_empty(),
        "round-trip failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn embedded_schema_files_match_the_files_on_disk() {
    let embedded: BTreeMap<String, Value> = schema_types::SCHEMA_FILES
        .iter()
        .map(|(path, text)| {
            let schema = serde_json::from_str(text)
                .unwrap_or_else(|e| panic!("{path}: embedded text is not JSON: {e}"));
            ((*path).to_owned(), schema)
        })
        .collect();
    assert_eq!(
        embedded,
        load_schemas(),
        "SCHEMA_FILES differs from schemas/"
    );
}
