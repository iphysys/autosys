//! Startup of the gateway with the committed `config/models/routing.yaml`:
//! profile resolution, the refusal of a profile without tool calling for an
//! agent with tools, and missing environment variables. No call is made.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

mod support;

use contracts::ErrorCode;
use model_gateway::{ModelGateway, PROFILE_OVERRIDE_ENV};
use serde_json::json;
use support::{agent, committed_routing, env, routing, test_ca_path};
use testkit::builders::id;

const PLINY_ENV: [(&str, &str); 2] = [
    ("PLINY_API_KEY", "pliny-test-key"),
    ("PLINY_BASE_URL", "https://pliny.example/api/llm"),
];

#[test]
fn the_committed_routing_defaults_to_claude_and_refuses_tools_on_pliny() {
    let routing = committed_routing();
    assert_eq!(routing.config().default_profile.as_str(), "haiku");
    assert!(
        routing
            .check_agent(&agent("designer", None, &["create_plate"]))
            .is_ok()
    );
    let refused = routing
        .check_agent(&agent("designer", Some("pliny"), &["create_plate"]))
        .expect_err("pliny has no tool calling");
    assert!(
        refused.message().contains("tool_calling: false"),
        "{refused}"
    );
}

#[test]
fn each_claude_model_is_an_option_on_the_same_provider() {
    let routing = committed_routing();
    for (name, model) in [
        ("haiku", "claude-haiku-5-5"),
        ("sonnet", "claude-sonnet-5-5"),
        ("opus", "claude-opus-5-5"),
    ] {
        let Some(contracts::types::ModelProfile::Anthropic {
            model: configured,
            tool_calling,
            ..
        }) = routing.profile(name)
        else {
            panic!("`{name}` is not an anthropic profile");
        };
        assert_eq!(configured.as_str(), model);
        assert!(tool_calling, "{name}");
        assert!(routing.price(model).is_some(), "{model} has no price");

        let vars = env(&[(PROFILE_OVERRIDE_ENV, name), ("ANTHROPIC_API_KEY", "key")]);
        let gateway = ModelGateway::start(
            routing.clone(),
            &[agent("designer", None, &["create_plate"])],
            &vars,
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(gateway.default_profile().as_str(), name);
        assert_eq!(
            gateway
                .agent_profile(&id("designer"))
                .expect("resolved")
                .as_str(),
            name
        );
    }
}

#[test]
fn pliny_is_refused_at_startup_for_an_agent_with_tools_that_names_it() {
    let vars = env(&[("ANTHROPIC_API_KEY", "key"), PLINY_ENV[0], PLINY_ENV[1]]);
    let result = ModelGateway::start(
        committed_routing(),
        &[agent(
            "designer",
            Some("pliny"),
            &["create_plate", "measure"],
        )],
        &vars,
    );
    let error = result.err().expect("refused");
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    let message = error.message();
    assert!(message.contains("agent `designer` has tools"), "{message}");
    assert!(message.contains("`pliny`"), "{message}");
    assert!(message.contains("tool_calling: false"), "{message}");
}

#[test]
fn pliny_is_refused_at_startup_when_the_override_gives_it_to_an_agent_with_tools() {
    let vars = env(&[(PROFILE_OVERRIDE_ENV, "pliny"), PLINY_ENV[0], PLINY_ENV[1]]);
    let error = ModelGateway::start(
        committed_routing(),
        &[agent("designer", None, &["create_plate"])],
        &vars,
    )
    .err()
    .expect("refused");
    assert!(error.message().contains("tool_calling: false"), "{error}");
}

#[test]
fn pliny_serves_an_agent_without_tools() {
    let vars = env(&[(PROFILE_OVERRIDE_ENV, "pliny"), PLINY_ENV[0], PLINY_ENV[1]]);
    let gateway = ModelGateway::start(committed_routing(), &[agent("writer", None, &[])], &vars)
        .expect("an agent without tools may use pliny");
    assert_eq!(gateway.default_profile().as_str(), "pliny");
    assert_eq!(
        gateway
            .agent_profile(&id("writer"))
            .expect("resolved")
            .as_str(),
        "pliny"
    );
    assert_eq!(
        gateway
            .agent_profile(&id("stranger"))
            .err()
            .map(|e| e.code()),
        Some(ErrorCode::NotFound)
    );
}

#[test]
fn agents_without_a_profile_get_the_default_and_others_keep_theirs() {
    let vars = env(&[("ANTHROPIC_API_KEY", "key"), PLINY_ENV[0], PLINY_ENV[1]]);
    let gateway = ModelGateway::start(
        committed_routing(),
        &[
            agent("designer", None, &["create_plate"]),
            agent("writer", Some("pliny"), &[]),
        ],
        &vars,
    )
    .expect("starts");
    assert_eq!(gateway.default_profile().as_str(), "haiku");
    assert_eq!(
        gateway
            .agent_profile(&id("designer"))
            .expect("resolved")
            .as_str(),
        "haiku"
    );
    assert_eq!(
        gateway
            .agent_profile(&id("writer"))
            .expect("resolved")
            .as_str(),
        "pliny"
    );
}

#[test]
fn a_missing_anthropic_key_fails_at_startup_naming_the_variable() {
    let error = ModelGateway::start(committed_routing(), &[], &env(&[]))
        .err()
        .expect("no key");
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    assert!(
        error
            .message()
            .contains("environment variable ANTHROPIC_API_KEY is not set"),
        "{error}"
    );
}

#[test]
fn a_blank_key_counts_as_missing() {
    let error = ModelGateway::start(committed_routing(), &[], &env(&[("ANTHROPIC_API_KEY", "")]))
        .err()
        .expect("blank key");
    assert!(error.message().contains("ANTHROPIC_API_KEY"), "{error}");
}

#[test]
fn missing_pliny_variables_fail_at_startup_naming_each() {
    let error = ModelGateway::start(
        committed_routing(),
        &[],
        &env(&[(PROFILE_OVERRIDE_ENV, "pliny")]),
    )
    .err()
    .expect("no pliny variables");
    let message = error.message();
    assert!(message.contains("PLINY_API_KEY"), "{message}");
    assert!(message.contains("PLINY_BASE_URL"), "{message}");
}

#[test]
fn profiles_not_in_use_need_no_variables() {
    let gateway = ModelGateway::start(
        committed_routing(),
        &[agent("designer", None, &["create_plate"])],
        &env(&[("ANTHROPIC_API_KEY", "key")]),
    );
    assert!(gateway.is_ok(), "{:?}", gateway.err());
}

#[test]
fn an_unknown_override_lists_the_profiles() {
    let error = ModelGateway::start(
        committed_routing(),
        &[],
        &env(&[(PROFILE_OVERRIDE_ENV, "gpt"), ("ANTHROPIC_API_KEY", "key")]),
    )
    .err()
    .expect("unknown profile");
    let message = error.message();
    assert!(message.contains(PROFILE_OVERRIDE_ENV), "{message}");
    assert!(message.contains("haiku, sonnet, opus, pliny"), "{message}");
}

#[test]
fn a_missing_ca_bundle_file_fails_at_startup_naming_the_variable() {
    let vars = env(&[
        (PROFILE_OVERRIDE_ENV, "pliny"),
        PLINY_ENV[0],
        PLINY_ENV[1],
        ("INTERNAL_CA_BUNDLE", "/no/such/internal-ca.pem"),
    ]);
    let error = ModelGateway::start(committed_routing(), &[], &vars)
        .err()
        .expect("missing bundle");
    assert!(error.message().contains("INTERNAL_CA_BUNDLE"), "{error}");
}

#[test]
fn a_ca_bundle_is_trusted_in_addition_to_the_system_roots() {
    let ca = test_ca_path();
    let vars = env(&[
        (PROFILE_OVERRIDE_ENV, "pliny"),
        PLINY_ENV[0],
        PLINY_ENV[1],
        ("INTERNAL_CA_BUNDLE", ca.as_str()),
    ]);
    let gateway = ModelGateway::start(committed_routing(), &[], &vars);
    assert!(gateway.is_ok(), "{:?}", gateway.err());
}

#[test]
fn a_plain_http_pliny_base_url_fails_at_startup_naming_the_variable() {
    let vars = env(&[
        (PROFILE_OVERRIDE_ENV, "pliny"),
        PLINY_ENV[0],
        ("PLINY_BASE_URL", "http://pliny.example/api/llm"),
    ]);
    let error = ModelGateway::start(committed_routing(), &[], &vars)
        .err()
        .expect("plain http");
    assert!(error.message().contains("PLINY_BASE_URL"), "{error}");
    assert!(error.message().contains("https"), "{error}");
}

fn profile(name: &str, headers: serde_json::Value) -> serde_json::Value {
    json!({
        "provider": "openai_compatible", "name": name, "model": "m", "tool_calling": false,
        "base_url_env": "BASE", "api_key_env": "KEY", "headers": headers,
        "max_output_tokens": 100, "request_timeout_seconds": 10,
        "retry": { "max_attempts": 1, "initial_backoff_ms": 0, "max_backoff_ms": 0 }
    })
}

#[test]
fn credential_headers_duplicate_names_and_an_unknown_default_are_rejected() {
    let error = routing(json!({
        "schema_version": "0.1",
        "default_profile": "missing",
        "profiles": [
            profile("internal", json!({ "Authorization": "Bearer x" })),
            profile("internal", json!({ "anthropic-beta": "x" })),
        ],
        "prices": [
            { "model": "m", "input_usd_per_mtok": 1.0, "output_usd_per_mtok": 1.0 },
            { "model": "m", "input_usd_per_mtok": 2.0, "output_usd_per_mtok": 2.0 }
        ]
    }))
    .expect_err("inconsistent routing");
    let message = error.message();
    assert!(message.contains("header `Authorization`"), "{message}");
    assert!(message.contains("header `anthropic-beta`"), "{message}");
    assert!(
        message.contains("profile `internal` is defined twice"),
        "{message}"
    );
    assert!(message.contains("default_profile `missing`"), "{message}");
    assert!(message.contains("model `m` is priced twice"), "{message}");
}
