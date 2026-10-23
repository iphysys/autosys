//! Configuration loading: the files in `config/` validate, and bad YAML is
//! rejected with the file and the path of the problem. Needs no database.

#![allow(clippy::expect_used, reason = "integration tests")]

use std::fs;
use std::path::PathBuf;

use contracts::ErrorCode;
use contracts::types::{ApprovalTrigger, AutonomyPolicy, GatesPolicy, ToolOverlay};
use infra::config::ConfigLoader;

fn config_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config")
}

fn loader() -> ConfigLoader {
    ConfigLoader::new().expect("config schemas compile")
}

fn rejected<T: infra::config::ConfigFile + std::fmt::Debug>(yaml: &str) -> String {
    let error = loader()
        .parse::<T>(yaml, "test.yaml")
        .expect_err("the YAML should be rejected");
    assert_eq!(error.code(), ErrorCode::InvalidInput, "{error}");
    error.message().to_owned()
}

#[test]
fn the_repository_config_is_valid() {
    let problems = loader()
        .validate_dir(&config_dir())
        .expect("config/ is readable");
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn the_autonomy_policy_asks_before_the_final_commit() {
    let policy: AutonomyPolicy = loader()
        .load(&config_dir().join("policies/autonomy.yaml"))
        .expect("autonomy policy");
    assert!(policy.approvals.iter().any(|rule| matches!(
        rule.trigger,
        ApprovalTrigger::SdrCommit { final_only: true }
    )));
}

#[test]
fn the_gates_policy_requires_the_three_checks() {
    let policy: GatesPolicy = loader()
        .load(&config_dir().join("policies/gates.yaml"))
        .expect("gates policy");
    let checks: Vec<&str> = policy
        .sdr_commit
        .required_checks
        .iter()
        .map(|c| c.as_str())
        .collect();
    assert_eq!(checks, ["schema", "units", "requirement.numeric_threshold"]);
}

const OVERLAY: &str = r#"
schema_version: "0.1"
server_id: cad
description: Test overlay.
tools:
  - name: create_part
    access: mutating
    external_side_effects: false
    idempotent: true
    concurrency_safe: true
    timeout_seconds: 60
    requires_approval: false
"#;

#[test]
fn a_valid_overlay_loads() {
    let overlay: ToolOverlay = loader().parse(OVERLAY, "test.yaml").expect("overlay");
    assert_eq!(overlay.tools[0].name.as_str(), "create_part");
}

#[test]
fn schema_violations_name_the_file_and_the_path() {
    let missing_timeout = OVERLAY.replace("    timeout_seconds: 60\n", "");
    let message = rejected::<ToolOverlay>(&missing_timeout);
    assert!(message.starts_with("test.yaml: "), "{message}");
    assert!(message.contains("/tools/0"), "{message}");
    assert!(message.contains("timeout_seconds"), "{message}");

    let unknown_field = OVERLAY.replace("server_id: cad", "server_id: cad\nendpoint: http://x");
    assert!(rejected::<ToolOverlay>(&unknown_field).contains("endpoint"));

    let wrong_type = OVERLAY.replace("timeout_seconds: 60", "timeout_seconds: soon");
    assert!(rejected::<ToolOverlay>(&wrong_type).contains("/tools/0/timeout_seconds"));
}

#[test]
fn yaml_hazards_are_rejected() {
    let duplicate = OVERLAY.replace("server_id: cad", "server_id: cad\nserver_id: other");
    assert!(rejected::<ToolOverlay>(&duplicate).contains("invalid YAML"));

    // YAML 1.1 would read `on` as true; YAML 1.2 keeps it a string, so the
    // schema sees a string where it wants a boolean.
    let yes_no = OVERLAY.replace("requires_approval: false", "requires_approval: on");
    assert!(rejected::<ToolOverlay>(&yes_no).contains("requires_approval"));

    let laughs = "a: &a [x, x, x, x, x, x, x, x, x, x]\n\
                  b: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a, *a]\n\
                  c: &c [*b, *b, *b, *b, *b, *b, *b, *b, *b, *b]\n\
                  d: &d [*c, *c, *c, *c, *c, *c, *c, *c, *c, *c]\n\
                  e: &e [*d, *d, *d, *d, *d, *d, *d, *d, *d, *d]\n\
                  f: &f [*e, *e, *e, *e, *e, *e, *e, *e, *e, *e]\n\
                  g: &g [*f, *f, *f, *f, *f, *f, *f, *f, *f, *f]\n\
                  h: [*g, *g, *g, *g, *g, *g, *g, *g, *g, *g]\n";
    assert!(rejected::<ToolOverlay>(laughs).contains("invalid YAML"));
}

#[test]
fn a_file_in_no_known_location_is_a_problem() {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("policies")).expect("mkdir");
    fs::write(dir.path().join("policies/surprise.yaml"), "a: 1\n").expect("write");
    fs::create_dir_all(dir.path().join("tools")).expect("mkdir");
    fs::write(dir.path().join("tools/cad.yaml"), OVERLAY).expect("write");
    let problems = loader().validate_dir(dir.path()).expect("readable");
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(problems[0].contains("no schema applies to config/policies/surprise.yaml"));
}
