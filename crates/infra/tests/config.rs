//! Configuration loading: the files in `config/` validate, and bad YAML is
//! rejected with the file and the path of the problem. Needs no database.

#![allow(clippy::expect_used, reason = "integration tests")]

use std::fs;
use std::path::PathBuf;

use contracts::ErrorCode;
use contracts::types::{
    AgentSpec, AgentSpecOnRepairExhausted, ApprovalTrigger, AutonomyPolicy, ContextBudgets,
    ContextPolicy, GatesPolicy, RuntimeSettings, ToolOverlay,
};
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

#[test]
fn the_default_context_profile_total_equals_the_ceiling() {
    let budgets: ContextBudgets = loader()
        .load(&config_dir().join("context.yaml"))
        .expect("context profiles");
    let policy: ContextPolicy = loader()
        .load(&config_dir().join("policies/context.yaml"))
        .expect("context policy");
    let default = budgets
        .profiles
        .iter()
        .find(|p| p.name.as_str() == "default")
        .expect("a profile named default");
    // ADR-0029: the ceiling starts at the default profile's total. Whether
    // every profile fits it is checked by `cargo xtask validate-config`.
    assert_eq!(default.total_tokens, policy.max_total_tokens);
}

const CONTEXT_PROFILE: &str = r#"
schema_version: "0.1"
profiles:
  - name: default
    total_tokens: 24000
    bytes_per_token: 3
    history_window_events: 200
    io_timeout_seconds: 10
    sections:
      instructions: 3000
      task_contract: 1500
      job_state: 500
      design_state: 8000
      run_history: 3000
      untrusted_content: 6000
      open_turn: 6000
"#;

#[test]
fn a_context_profile_with_an_unknown_section_is_rejected() {
    loader()
        .parse::<ContextBudgets>(CONTEXT_PROFILE, "test.yaml")
        .expect("the profile is valid as written");

    let toolbox = CONTEXT_PROFILE.replace(
        "      open_turn: 6000\n",
        "      open_turn: 6000\n      toolbox: 2000\n",
    );
    let message = rejected::<ContextBudgets>(&toolbox);
    assert!(message.starts_with("test.yaml: "), "{message}");
    assert!(message.contains("toolbox"), "{message}");

    let missing = CONTEXT_PROFILE.replace("      job_state: 500\n", "");
    assert!(rejected::<ContextBudgets>(&missing).contains("job_state"));

    let zero = CONTEXT_PROFILE.replace("bytes_per_token: 3", "bytes_per_token: 0");
    assert!(rejected::<ContextBudgets>(&zero).contains("/profiles/0/bytes_per_token"));
}

#[test]
fn a_context_policy_without_a_ceiling_is_rejected() {
    let message = rejected::<ContextPolicy>("schema_version: \"0.1\"\n");
    assert!(message.contains("max_total_tokens"), "{message}");
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

#[test]
fn overlay_schema_paths_must_name_schemas_that_exist() {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("tools")).expect("mkdir");
    let typo = OVERLAY.replace(
        "    requires_approval: false\n",
        "    requires_approval: false\n    \
         request_schema: tools/build123d/create_plate_request.schema.json\n    \
         result_schema: tools/build123d/create_plate_reslt.schema.json\n",
    );
    fs::write(dir.path().join("tools/cad.yaml"), typo).expect("write");
    let problems = loader().validate_dir(dir.path()).expect("readable");
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(
        problems[0].contains("result_schema tools/build123d/create_plate_reslt.schema.json"),
        "{problems:#?}"
    );
    assert!(!problems[0].contains("request_schema"), "{problems:#?}");
}

#[test]
fn the_build123d_tools_are_read_only_without_side_effects_or_approval() {
    let overlay: ToolOverlay = loader()
        .load(&config_dir().join("tools/build123d.yaml"))
        .expect("build123d overlay");
    let names: Vec<&str> = overlay.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["create_plate", "measure"]);
    for tool in &overlay.tools {
        assert!(!tool.external_side_effects, "{}", tool.name.as_str());
        assert!(tool.idempotent, "{}", tool.name.as_str());
        assert!(!tool.requires_approval, "{}", tool.name.as_str());
        assert!(tool.request_schema.is_some() && tool.result_schema.is_some());
    }
}

#[test]
fn the_runtime_settings_load_and_a_zero_timeout_is_rejected() {
    let settings: RuntimeSettings = loader()
        .load(&config_dir().join("runtime.yaml"))
        .expect("runtime settings");
    assert_eq!(settings.io_timeout_seconds.get(), 10);

    let message = rejected::<RuntimeSettings>("schema_version: \"0.1\"\nio_timeout_seconds: 0\n");
    assert!(message.contains("/io_timeout_seconds"), "{message}");
    let unknown = rejected::<RuntimeSettings>(
        "schema_version: \"0.1\"\nio_timeout_seconds: 10\nmax_tokens: 5\n",
    );
    assert!(unknown.contains("max_tokens"), "{unknown}");
}

#[test]
fn the_main_orchestrator_has_the_approved_tools_and_limits() {
    let spec: AgentSpec = loader()
        .load(&config_dir().join("agents/main_orchestrator.yaml"))
        .expect("main orchestrator spec");
    assert_eq!(spec.id.as_str(), "main_orchestrator");
    assert_eq!(spec.prompt.as_str(), "orchestrator/main.md");
    assert_eq!(spec.model_profile, None, "it uses the routing default");
    let tools: Vec<&str> = spec.tools.iter().map(|t| t.as_str()).collect();
    assert_eq!(
        tools,
        ["sdr_read", "sdr_propose", "create_plate", "measure"]
    );
    assert!(spec.skills.is_empty());
    assert_eq!(spec.on_repair_exhausted, AgentSpecOnRepairExhausted::Fail);
    let limits = &spec.limits;
    assert_eq!(
        [
            limits.max_tokens.get(),
            limits.max_tool_calls.get(),
            limits.max_steps.get(),
            limits.max_wall_time_seconds.get(),
            limits.max_repair_attempts.get(),
        ],
        [1_000_000, 30, 40, 600, 3]
    );
}
