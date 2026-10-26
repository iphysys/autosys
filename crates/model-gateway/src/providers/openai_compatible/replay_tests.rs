//! The `openai_compatible` provider (the Pliny platform): its adapter against
//! recorded chat-completions responses served from a loopback server, its
//! profile in the committed `routing.yaml`, and an ignored live smoke test.
//! Deleted with the provider.

use std::time::Duration;

use contracts::{
    ContentBlock, Environment, ErrorCode, Message, ModelProvider, ModelStopReason, Role,
};
use serde_json::json;
use testkit::builders::agent_spec as agent;
use testkit::contract;

use super::{OpenAiCompatibleProvider, OpenAiCompatibleSettings};
use crate::env::{ProcessEnv, Secret};
use crate::gateway::ModelGateway;
use crate::providers::ProviderAdapter;
use crate::routing::PROFILE_OVERRIDE_ENV;
use crate::test_support::replay::{Replay, Reply, recording};
use crate::test_support::{committed_routing, ctx, env, request, retry};

const MODEL: &str = "snps-provider/nemotron-3-ultra-550b-a55";
const OPENAI_TEST_KEY: &str = "pliny-test-key-never-logged";
const PLINY_ENV: [(&str, &str); 2] = [
    ("PLINY_API_KEY", "pliny-test-key"),
    ("PLINY_BASE_URL", "https://pliny.example/api/llm"),
];

fn rec(name: &str) -> Vec<Reply> {
    recording(&format!(
        "src/providers/openai_compatible/recordings/{name}"
    ))
}

fn openai_settings(base_url: &str) -> OpenAiCompatibleSettings {
    OpenAiCompatibleSettings {
        base_url: base_url.to_owned(),
        base_url_origin: "PLINY_BASE_URL".to_owned(),
        api_key: Secret::new(OPENAI_TEST_KEY),
        headers: Vec::new(),
        timeout: Duration::from_secs(5),
        retry: retry(3),
        ca_bundle: None,
        environment: Environment::Test,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passes_the_model_provider_contract_with_a_recorded_text_response() {
    let server = Replay::start(rec("text.json")).await;
    let url = server.base_url();
    contract::model_provider(
        || OpenAiCompatibleProvider::new(openai_settings(&url)).expect("provider"),
        request(MODEL, "Size the plate."),
    )
    .await;
    assert_eq!(
        server.remaining(),
        0,
        "the recorded response was not requested"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maps_a_tool_call_round_trip() {
    let server = Replay::start(rec("tool-calls.json")).await;
    let url = server.base_url();
    contract::model_provider_tool_round_trip(
        || OpenAiCompatibleProvider::new(openai_settings(&url)).expect("provider"),
        request(MODEL, "Create a 100 mm plate."),
    )
    .await;

    let captured = server.captured();
    let messages = &captured[1].body["messages"];
    assert_eq!(
        messages[0],
        json!({ "role": "system", "content": "You design mechanical parts." })
    );
    assert_eq!(
        messages[1],
        json!({ "role": "user", "content": "Create a 100 mm plate." })
    );
    assert_eq!(
        messages[2],
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_test_01",
                "type": "function",
                "function": { "name": "create_plate", "arguments": "{\"length_mm\":100}" }
            }]
        })
    );
    assert_eq!(
        messages[3],
        json!({ "role": "tool", "tool_call_id": "call_test_01", "content": "result of call_test_01" })
    );
}

#[tokio::test]
async fn requests_carry_the_key_the_profile_headers_and_the_base_path() {
    let server = Replay::start(rec("text.json")).await;
    let mut settings = openai_settings(&format!("{}/api/llm", server.base_url()));
    settings.headers = vec![(
        "X-TFY-LOGGING-CONFIG".to_owned(),
        r#"{"enabled":true}"#.to_owned(),
    )];
    let provider = OpenAiCompatibleProvider::new(settings).expect("provider");
    let call = ProviderAdapter::call(&provider, &request(MODEL, "Size the plate."), &ctx())
        .await
        .expect("recorded response");
    assert_eq!(call.request_id.as_deref(), Some("pliny-test-text"));
    assert_eq!(call.response.stop_reason, ModelStopReason::EndTurn);
    assert_eq!(call.response.usage.input_tokens, 80);
    assert_eq!(call.response.usage.output_tokens, 12);
    assert_eq!(call.usage_by_model[0].model, MODEL);

    let captured = &server.captured()[0];
    assert_eq!(captured.path, "/api/llm/chat/completions");
    assert_eq!(
        captured.header("authorization"),
        Some(format!("Bearer {OPENAI_TEST_KEY}").as_str())
    );
    assert_eq!(
        captured.header("x-tfy-logging-config"),
        Some(r#"{"enabled":true}"#)
    );
    assert_eq!(captured.body["model"], MODEL);
    assert_eq!(captured.body["stream"], false);
    assert_eq!(captured.body["max_tokens"], 1024);
    assert_eq!(captured.body["tools"][0]["type"], "function");
    assert_eq!(
        captured.body["tools"][0]["function"]["name"],
        "create_plate"
    );
}

#[tokio::test]
async fn server_errors_are_retried_and_reported_without_the_key() {
    let unavailable = || {
        Reply::json(
            503,
            json!({ "error": { "type": "server_error", "message": "upstream unavailable" } }),
        )
    };
    let server = Replay::start(vec![unavailable(), unavailable(), unavailable()]).await;
    let provider =
        OpenAiCompatibleProvider::new(openai_settings(&server.base_url())).expect("provider");
    let error = provider
        .complete(&request(MODEL, "Size the plate."), &ctx())
        .await
        .expect_err("always unavailable");
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(server.captured().len(), 3);
    assert!(!format!("{error} {error:?} {provider:?}").contains(OPENAI_TEST_KEY));
}

#[tokio::test]
async fn a_plain_http_base_url_is_refused_naming_its_variable() {
    let error = OpenAiCompatibleProvider::new(openai_settings("http://pliny.example/api"))
        .expect_err("plain http");
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    assert!(error.message().contains("PLINY_BASE_URL"), "{error}");
}

// The committed routing.yaml: the pliny profile

#[test]
fn pliny_is_the_default_profile() {
    let gateway = ModelGateway::start(committed_routing(), &[], &env(&PLINY_ENV))
        .expect("the PLINY_ variables are set");
    assert_eq!(gateway.default_profile().as_str(), "pliny");
}

#[test]
fn pliny_serves_an_agent_with_tools() {
    let tools = ["create_plate", "measure"];
    ModelGateway::start(
        committed_routing(),
        &[agent("designer", Some("pliny"), &tools)],
        &env(&PLINY_ENV),
    )
    .expect("a pinned model_profile: pliny");

    let overridden = env(&[(PROFILE_OVERRIDE_ENV, "pliny"), PLINY_ENV[0], PLINY_ENV[1]]);
    ModelGateway::start(
        committed_routing(),
        &[agent("designer", None, &tools)],
        &overridden,
    )
    .expect("SYNTH_MODEL_PROFILE=pliny");
}

#[test]
fn pliny_serves_an_agent_without_tools() {
    ModelGateway::start(
        committed_routing(),
        &[agent("writer", None, &[])],
        &env(&PLINY_ENV),
    )
    .expect("an agent without tools may use pliny");
}

#[test]
fn missing_pliny_variables_fail_at_startup_naming_each() {
    let error = ModelGateway::start(committed_routing(), &[], &env(&[]))
        .err()
        .expect("no pliny variables");
    let message = error.message();
    assert!(
        message.contains("PLINY_API_KEY, PLINY_BASE_URL are not set"),
        "{message}"
    );
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

// Live

#[tokio::test]
#[ignore = "calls Pliny; needs PLINY_API_KEY and PLINY_BASE_URL"]
async fn live_pliny_text() {
    let mut vars: std::collections::HashMap<String, String> = std::env::vars().collect();
    vars.insert(PROFILE_OVERRIDE_ENV.to_owned(), "pliny".to_owned());
    let gateway =
        ModelGateway::start(committed_routing(), &[], &vars).expect("set the PLINY_ variables");
    let mut text_only = request("", "Reply with the single word OK.");
    text_only.tools.clear();
    text_only.max_output_tokens = 256;
    let call = gateway
        .complete(gateway.default_profile(), text_only, &ctx())
        .await
        .expect("pliny call");
    assert!(call.response.usage.output_tokens > 0, "{:?}", call.response);
}

#[tokio::test]
#[ignore = "calls Pliny; needs PLINY_API_KEY and PLINY_BASE_URL"]
async fn live_pliny_tool_round_trip() {
    // The default profile, pliny, or the profile SYNTH_MODEL_PROFILE names.
    let gateway = ModelGateway::start(committed_routing(), &[], &ProcessEnv)
        .expect("set the PLINY_ variables");
    let profile = gateway.default_profile().clone();
    let mut first = request("", "Create a plate 100 mm long with the create_plate tool.");
    first.max_output_tokens = 4000;
    let call = gateway
        .complete(&profile, first.clone(), &ctx())
        .await
        .expect("first call");
    assert_eq!(
        call.response.stop_reason,
        ModelStopReason::ToolUse,
        "{:?}",
        call.response
    );
    // Pliny's model has no price.
    assert!(call.cost_usd.is_none());

    let mut follow_up = first;
    follow_up.messages.push(Message {
        role: Role::Assistant,
        content: call.response.content.clone(),
    });
    follow_up.messages.push(Message {
        role: Role::User,
        content: call
            .response
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall { id, .. } => Some(ContentBlock::ToolResult {
                    call_id: id.clone(),
                    content: r#"{"part": "plate-1", "mass_kg": 0.42}"#.to_owned(),
                    is_error: false,
                }),
                _ => None,
            })
            .collect(),
    });
    let answer = gateway
        .complete(&profile, follow_up, &ctx())
        .await
        .expect("follow-up call with the tool result");
    assert_ne!(answer.response.stop_reason, ModelStopReason::Refusal);
}
