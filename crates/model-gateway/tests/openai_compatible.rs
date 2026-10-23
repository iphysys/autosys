//! The OpenAI-compatible adapter (the Pliny platform) against recorded
//! chat-completions responses served from a loopback server.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

mod support;

use contracts::{ErrorCode, ModelProvider, ModelStopReason};
use model_gateway::providers::OpenAiCompatibleProvider;
use serde_json::json;
use support::replay::{Replay, Reply, recording};
use support::{OPENAI_TEST_KEY, ctx, openai_settings, request};
use testkit::contract;

const MODEL: &str = "snps-provider/nemotron-3-ultra-550b-a55";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passes_the_model_provider_contract_with_a_recorded_text_response() {
    let server = Replay::start(recording("openai_compatible/text.json")).await;
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
    let server = Replay::start(recording("openai_compatible/tool-calls.json")).await;
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
    let server = Replay::start(recording("openai_compatible/text.json")).await;
    let mut settings = openai_settings(&format!("{}/api/llm", server.base_url()));
    settings.headers = vec![(
        "X-TFY-LOGGING-CONFIG".to_owned(),
        r#"{"enabled":true}"#.to_owned(),
    )];
    let provider = OpenAiCompatibleProvider::new(settings).expect("provider");
    let call =
        model_gateway::ProviderAdapter::call(&provider, &request(MODEL, "Size the plate."), &ctx())
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
