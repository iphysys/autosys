//! Configuration loading: the files in `config/` validate, and bad YAML is
//! rejected with the file and the path of the problem. Needs no database.

#![allow(clippy::expect_used, reason = "integration tests")]

use std::fs;
use std::path::PathBuf;

use contracts::types::{
    AgentSpec, AgentSpecOnRepairExhausted, ApiSettings, ApprovalTrigger, AutonomyPolicy,
    ContextBudgets, ContextPolicy, GatesPolicy, RuntimeSettings, ToolOverlay, WorkerSettings,
};
use contracts::{Environment, ErrorCode};
use infra::config::ConfigLoader;

fn config_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config")
}

/// The committed `config/` is production configuration.
fn loader() -> ConfigLoader {
    loader_for(Environment::Production)
}

fn loader_for(environment: Environment) -> ConfigLoader {
    ConfigLoader::new(environment).expect("config schemas compile")
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
    idempotency_test: tests/test_cad.py::test_create_part_twice_with_one_key
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

/// Not idempotent until the server has a test that repeats a call with one
/// idempotency key (ADR-0031).
#[test]
fn the_build123d_tools_are_read_only_without_side_effects_or_approval() {
    let overlay: ToolOverlay = loader()
        .load(&config_dir().join("tools/build123d.yaml"))
        .expect("build123d overlay");
    let names: Vec<&str> = overlay.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["create_plate", "measure"]);
    for tool in &overlay.tools {
        assert!(!tool.external_side_effects, "{}", tool.name.as_str());
        assert!(!tool.idempotent, "{}", tool.name.as_str());
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

// ---------------------------------------------------------------- worker ---

/// worker.yaml with the given intervals, and one tool server.
fn worker_yaml(lease: u64, heartbeat: u64, poll: u64, jitter: u64) -> String {
    [
        r#"schema_version: "0.1""#.to_owned(),
        format!("lease_ms: {lease}"),
        format!("heartbeat_ms: {heartbeat}"),
        format!("poll_interval_ms: {poll}"),
        format!("poll_jitter_ms: {jitter}"),
        "max_attempts: 3".to_owned(),
        "retry_backoff_min_ms: 5000".to_owned(),
        "retry_backoff_max_ms: 60000".to_owned(),
        "database: { max_connections: 4, acquire_timeout_ms: 1000, operation_timeout_ms: 1000 }"
            .to_owned(),
        "tool_servers:".to_owned(),
        r#"  - { server_id: build123d, url: "http://127.0.0.1:18123/mcp", connect_timeout_ms: 1000, call_timeout_ms: 1000 }"#
            .to_owned(),
        String::new(),
    ]
    .join("\n")
}

fn worker_problem(environment: Environment, yaml: &str) -> String {
    let error = loader_for(environment)
        .parse::<WorkerSettings>(yaml, "worker.yaml")
        .expect_err("the worker settings should be rejected");
    assert_eq!(error.code(), ErrorCode::InvalidInput, "{error}");
    error.message().to_owned()
}

#[test]
fn the_committed_worker_settings_hold_in_every_environment() {
    for environment in [
        Environment::Local,
        Environment::Test,
        Environment::Production,
    ] {
        let settings: WorkerSettings = loader_for(environment)
            .load(&config_dir().join("worker.yaml"))
            .unwrap_or_else(|e| panic!("{environment}: {e}"));
        assert_eq!(settings.lease_ms.get(), 30_000);
    }
}

#[test]
fn short_test_intervals_hold_only_in_the_test_environment() {
    let fast = worker_yaml(2_000, 500, 100, 10);
    loader_for(Environment::Test)
        .parse::<WorkerSettings>(&fast, "worker.yaml")
        .expect("short intervals are for tests");
    for environment in [Environment::Local, Environment::Production] {
        let message = worker_problem(environment, &fast);
        for field in [
            "lease_ms (2000)",
            "heartbeat_ms (500)",
            "poll_interval_ms (100)",
        ] {
            assert!(message.contains(field), "{environment}: {message}");
        }
        assert!(
            message.contains(&format!("the {environment} environment")),
            "{message}"
        );
    }
}

#[test]
fn the_worker_intervals_must_fit_each_other() {
    let test = Environment::Test;
    let message = worker_problem(test, &worker_yaml(3_000, 1_500, 100, 10));
    assert!(message.contains("at most a third of lease_ms"), "{message}");
    let message = worker_problem(test, &worker_yaml(3_000, 1_000, 1_000, 10));
    assert!(message.contains("shorter than heartbeat_ms"), "{message}");
    let message = worker_problem(test, &worker_yaml(3_000, 1_000, 100, 100));
    assert!(
        message.contains("smaller than poll_interval_ms"),
        "{message}"
    );
    let backoff = worker_yaml(3_000, 1_000, 100, 10)
        .replace("retry_backoff_max_ms: 60000", "retry_backoff_max_ms: 10");
    let message = worker_problem(test, &backoff);
    assert!(
        message.contains("at least retry_backoff_min_ms"),
        "{message}"
    );
    let twice = worker_yaml(3_000, 1_000, 100, 10).replace(
        "tool_servers:\n",
        "tool_servers:\n  - { server_id: build123d, url: \"http://127.0.0.1:1/mcp\", connect_timeout_ms: 1, call_timeout_ms: 1 }\n",
    );
    let message = worker_problem(test, &twice);
    assert!(message.contains("build123d is listed twice"), "{message}");
}

#[test]
fn an_idempotent_tool_needs_a_named_repeat_test() {
    let overlay = |extra: &str| {
        format!(
            "schema_version: \"0.1\"\nserver_id: build123d\ndescription: d\ntools:\n  - name: create_plate\n    access: read_only\n    external_side_effects: false\n    idempotent: true\n{extra}    concurrency_safe: true\n    timeout_seconds: 5\n    requires_approval: false\n"
        )
    };
    let message = rejected::<ToolOverlay>(&overlay(""));
    assert!(
        message.contains("create_plate: idempotent: true needs an idempotency_test"),
        "{message}"
    );
    loader()
        .parse::<ToolOverlay>(
            &overlay("    idempotency_test: tests/test_tools.py::test_repeat_with_one_key\n"),
            "build123d.yaml",
        )
        .expect("a named test satisfies the rule");
}

#[test]
fn the_committed_api_settings_listen_on_loopback() {
    let settings: ApiSettings = loader_for(Environment::Production)
        .load(&config_dir().join("api.yaml"))
        .unwrap_or_else(|e| panic!("{e}"));
    let listen: std::net::SocketAddr = settings.listen.parse().expect("a socket address");
    assert!(listen.ip().is_loopback(), "{listen}");
}

#[test]
fn an_api_listen_address_must_be_a_socket_address() {
    let yaml = fs::read_to_string(config_dir().join("api.yaml"))
        .expect("readable")
        .replace("listen: 127.0.0.1:8080", "listen: localhost");
    let error = loader_for(Environment::Production)
        .parse::<ApiSettings>(&yaml, "api.yaml")
        .expect_err("not a socket address");
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    assert!(error.message().contains("listen (localhost)"), "{error}");
}
