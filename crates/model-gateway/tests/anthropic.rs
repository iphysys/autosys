//! The Anthropic adapter against recorded Messages API responses served from
//! a loopback server: the `ModelProvider` contract, the tool-call round trip,
//! headers, retries, timeouts, cancellation and the mapping edge cases.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use contracts::types::ModelEffort;
use contracts::{ContentBlock, ErrorCode, ModelProvider, ModelStopReason};
use model_gateway::providers::{
    ANTHROPIC_VERSION, AnthropicProvider, FALLBACK_BETA, ProviderAdapter,
};
use model_gateway::{ModelGateway, cost_usd};
use serde_json::{Value, json};
use support::replay::{Replay, Reply, recording};
use support::{
    ANTHROPIC_TEST_KEY, anthropic_settings, committed_routing, ctx, env, request, retry,
};
use testkit::builders::id;
use testkit::contract;

const MODEL: &str = "claude-opus-5-5";

fn overloaded() -> Reply {
    Reply::json(
        529,
        json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } }),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passes_the_model_provider_contract_with_a_recorded_text_response() {
    let server = Replay::start(recording("anthropic/text.json")).await;
    let url = server.base_url();
    contract::model_provider(
        || AnthropicProvider::new(anthropic_settings(&url)).expect("provider"),
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
async fn a_tool_call_round_trip_sends_the_thinking_block_back_unchanged() {
    let server = Replay::start(recording("anthropic/tool-round-trip.json")).await;
    let url = server.base_url();
    let follow_up = contract::model_provider_tool_round_trip(
        || AnthropicProvider::new(anthropic_settings(&url)).expect("provider"),
        request(MODEL, "Create a 100 mm plate."),
    )
    .await;

    // The assistant turn the suite sent back holds the opaque thinking block.
    assert!(matches!(
        &follow_up.messages[1].content[0],
        ContentBlock::Opaque { provider, .. } if provider.as_str() == "anthropic"
    ));

    let captured = server.captured();
    assert_eq!(captured.len(), 2);
    let first_reply = &recording("anthropic/tool-round-trip.json")[0].body;
    let second = &captured[1].body;
    // The assistant turn goes back exactly as Claude produced it.
    assert_eq!(second["messages"][1]["role"], "assistant");
    assert_eq!(second["messages"][1]["content"], first_reply["content"]);
    // One tool_result per call, answering the call's id.
    assert_eq!(
        second["messages"][2],
        json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_test_01",
                "content": "result of toolu_test_01",
                "is_error": false
            }]
        })
    );
}

#[tokio::test]
async fn requests_carry_the_documented_headers_and_body() {
    let server = Replay::start(recording("anthropic/text.json")).await;
    let mut settings = anthropic_settings(&server.base_url());
    settings.workspace_id = Some("wrkspc_test".to_owned());
    settings.effort = Some(ModelEffort::High);
    settings.refusal_fallback = true;
    settings.headers = vec![("X-Trace-Tag".to_owned(), "model-gateway-test".to_owned())];
    let provider = AnthropicProvider::new(settings).expect("provider");
    provider
        .complete(&request(MODEL, "Size the plate."), &ctx())
        .await
        .expect("recorded response");

    let captured = &server.captured()[0];
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.path, "/v1/messages");
    assert_eq!(
        captured.header("authorization"),
        Some(format!("Bearer {ANTHROPIC_TEST_KEY}").as_str())
    );
    assert_eq!(captured.header("x-api-key"), None);
    assert_eq!(
        captured.header("anthropic-version"),
        Some(ANTHROPIC_VERSION)
    );
    assert_eq!(
        captured.header("anthropic-workspace-id"),
        Some("wrkspc_test")
    );
    assert_eq!(captured.header("anthropic-beta"), Some(FALLBACK_BETA));
    assert_eq!(captured.header("x-trace-tag"), Some("model-gateway-test"));
    assert_eq!(captured.header("content-type"), Some("application/json"));

    let body = &captured.body;
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["max_tokens"], 1024);
    assert_eq!(body["system"], "You design mechanical parts.");
    assert_eq!(body["output_config"], json!({ "effort": "high" }));
    assert_eq!(body["fallbacks"], "default");
    assert_eq!(body["tools"][0]["name"], "create_plate");
    assert!(body["tools"][0]["input_schema"].is_object());
    for removed in ["temperature", "top_p", "top_k", "tool_choice", "thinking"] {
        assert!(body.get(removed).is_none(), "sent `{removed}`");
    }
}

#[tokio::test]
async fn optional_headers_are_absent_unless_configured() {
    let server = Replay::start(recording("anthropic/text.json")).await;
    let provider =
        AnthropicProvider::new(anthropic_settings(&server.base_url())).expect("provider");
    provider
        .complete(&request(MODEL, "Size the plate."), &ctx())
        .await
        .expect("recorded response");
    let captured = &server.captured()[0];
    assert_eq!(captured.header("anthropic-workspace-id"), None);
    assert_eq!(captured.header("anthropic-beta"), None);
    assert!(captured.body.get("output_config").is_none());
    assert!(captured.body.get("fallbacks").is_none());
}

#[tokio::test]
async fn a_rate_limit_is_retried_after_the_wait_the_provider_asks_for() {
    let mut replies = vec![
        Reply::json(
            429,
            json!({ "type": "error", "error": { "type": "rate_limit_error", "message": "Slow down" } }),
        )
        .with_header("retry-after", "0"),
    ];
    replies.extend(recording("anthropic/text.json"));
    let server = Replay::start(replies).await;
    let provider =
        AnthropicProvider::new(anthropic_settings(&server.base_url())).expect("provider");
    let call = provider
        .call(&request(MODEL, "Size the plate."), &ctx())
        .await
        .expect("second attempt succeeds");
    assert_eq!(call.attempts, 2);
    assert_eq!(call.request_id.as_deref(), Some("req_test_text"));
    assert_eq!(server.captured().len(), 2);
}

#[tokio::test]
async fn overload_on_every_attempt_fails_after_max_attempts() {
    let server = Replay::start(vec![overloaded(), overloaded(), overloaded(), overloaded()]).await;
    let provider =
        AnthropicProvider::new(anthropic_settings(&server.base_url())).expect("provider");
    let error = provider
        .complete(&request(MODEL, "Size the plate."), &ctx())
        .await
        .expect_err("always overloaded");
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert!(error.retryable());
    assert!(error.message().contains("overloaded_error"), "{error}");
    assert_eq!(server.captured().len(), 3, "max_attempts is 3");
}

#[tokio::test]
async fn a_bad_request_is_not_retried() {
    let server = Replay::start(vec![
        Reply::json(
            400,
            json!({ "type": "error", "error": { "type": "invalid_request_error", "message": "max_tokens: too large" } }),
        ),
        overloaded(),
    ])
    .await;
    let provider =
        AnthropicProvider::new(anthropic_settings(&server.base_url())).expect("provider");
    let error = provider
        .complete(&request(MODEL, "Size the plate."), &ctx())
        .await
        .expect_err("bad request");
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    assert_eq!(server.captured().len(), 1);
}

#[tokio::test]
async fn an_authentication_error_never_shows_the_key() {
    let server = Replay::start(vec![Reply::json(
        401,
        json!({ "type": "error", "error": { "type": "authentication_error", "message": "invalid x-api-key" } }),
    )])
    .await;
    let provider =
        AnthropicProvider::new(anthropic_settings(&server.base_url())).expect("provider");
    let error = provider
        .complete(&request(MODEL, "Size the plate."), &ctx())
        .await
        .expect_err("rejected key");
    assert_eq!(error.code(), ErrorCode::PermissionDenied);
    assert!(!format!("{error} {error:?}").contains(ANTHROPIC_TEST_KEY));
    assert!(!format!("{provider:?}").contains(ANTHROPIC_TEST_KEY));
    assert!(!format!("{:?}", anthropic_settings("https://x.example")).contains(ANTHROPIC_TEST_KEY));
}

#[tokio::test]
async fn a_slow_reply_times_out() {
    let mut slow = recording("anthropic/text.json");
    slow[0].delay = Duration::from_secs(3);
    let server = Replay::start(slow).await;
    let mut settings = anthropic_settings(&server.base_url());
    settings.timeout = Duration::from_millis(500);
    settings.retry = retry(1);
    let provider = AnthropicProvider::new(settings).expect("provider");
    let error = provider
        .complete(&request(MODEL, "Size the plate."), &ctx())
        .await
        .expect_err("no reply in time");
    assert_eq!(error.code(), ErrorCode::Timeout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_ends_a_request_in_flight() {
    let mut slow = recording("anthropic/text.json");
    slow[0].delay = Duration::from_secs(30);
    let server = Replay::start(slow).await;
    let provider =
        AnthropicProvider::new(anthropic_settings(&server.base_url())).expect("provider");
    let ctx = ctx();
    let canceller = ctx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        canceller.cancel();
    });
    let started = Instant::now();
    let error = provider
        .complete(&request(MODEL, "Size the plate."), &ctx)
        .await
        .expect_err("cancelled");
    assert_eq!(error.code(), ErrorCode::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_ends_a_backoff() {
    let server = Replay::start(vec![overloaded(), overloaded()]).await;
    let mut settings = anthropic_settings(&server.base_url());
    settings.retry.initial_backoff_ms = 60_000;
    settings.retry.max_backoff_ms = 60_000;
    let provider = AnthropicProvider::new(settings).expect("provider");
    let ctx = ctx();
    let canceller = ctx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        canceller.cancel();
    });
    let started = Instant::now();
    let error = provider
        .complete(&request(MODEL, "Size the plate."), &ctx)
        .await
        .expect_err("cancelled");
    assert_eq!(error.code(), ErrorCode::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        server.captured().len(),
        1,
        "no attempt after the cancelled wait"
    );
}

#[tokio::test]
async fn a_refusal_maps_to_the_refusal_stop_reason() {
    let server = Replay::start(recording("anthropic/refusal.json")).await;
    let provider =
        AnthropicProvider::new(anthropic_settings(&server.base_url())).expect("provider");
    let response = provider
        .complete(&request(MODEL, "Size the plate."), &ctx())
        .await
        .expect("a refusal is a response");
    assert_eq!(response.stop_reason, ModelStopReason::Refusal);
    assert!(response.content.is_empty());
}

#[tokio::test]
async fn a_tool_call_cut_off_at_max_tokens_is_dropped() {
    let server = Replay::start(recording("anthropic/max-tokens.json")).await;
    let provider =
        AnthropicProvider::new(anthropic_settings(&server.base_url())).expect("provider");
    let response = provider
        .complete(&request(MODEL, "Size the plate."), &ctx())
        .await
        .expect("a cut-off response");
    assert_eq!(response.stop_reason, ModelStopReason::MaxTokens);
    assert!(
        !response
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall { .. })),
        "{:?}",
        response.content
    );
}

#[tokio::test]
async fn an_invalid_tool_name_from_the_model_is_invalid_input() {
    let mut replies = recording("anthropic/tool-round-trip.json");
    replies.truncate(1);
    replies[0].body["content"][2]["name"] = json!("create plate; rm -rf");
    let server = Replay::start(replies).await;
    let provider =
        AnthropicProvider::new(anthropic_settings(&server.base_url())).expect("provider");
    let error = provider
        .complete(&request(MODEL, "Size the plate."), &ctx())
        .await
        .expect_err("invalid tool name");
    assert_eq!(error.code(), ErrorCode::InvalidInput);
}

#[tokio::test]
async fn a_fallback_is_costed_per_model_and_reports_the_model_that_answered() {
    let server = Replay::start(recording("anthropic/fallback.json")).await;
    let mut settings = anthropic_settings(&server.base_url());
    settings.refusal_fallback = true;
    let provider: Arc<dyn ProviderAdapter> =
        Arc::new(AnthropicProvider::new(settings).expect("provider"));

    // An Opus profile: Opus models have a server-side fallback; Haiku does not.
    let routing = support::routing(json!({
        "schema_version": "0.1",
        "default_profile": "claude",
        "profiles": [{
            "provider": "anthropic", "name": "claude", "model": "claude-opus-5-5",
            "tool_calling": true, "base_url": "https://api.anthropic.com",
            "api_key_env": "ANTHROPIC_API_KEY", "refusal_fallback": "default",
            "max_output_tokens": 16000, "request_timeout_seconds": 600,
            "retry": { "max_attempts": 3, "initial_backoff_ms": 1000, "max_backoff_ms": 30000 }
        }],
        "prices": [
            { "model": "claude-opus-5-5", "input_usd_per_mtok": 4.0, "output_usd_per_mtok": 20.0 },
            { "model": "claude-opus-4-8", "input_usd_per_mtok": 5.0, "output_usd_per_mtok": 25.0 }
        ]
    }))
    .expect("consistent routing");
    let gateway = ModelGateway::with_providers(
        routing.clone(),
        &[],
        &env(&[]),
        BTreeMap::from([("claude".to_owned(), provider)]),
    )
    .expect("gateway");
    let mut unresolved = request("set-by-the-gateway", "Size the plate.");
    unresolved.max_output_tokens = 1024;
    let call = gateway
        .complete(&id("claude"), unresolved, &ctx())
        .await
        .expect("fallback answer");

    // The gateway sends the profile's model; the answer names the fallback.
    assert_eq!(server.captured()[0].body["model"], "claude-opus-5-5");
    assert_eq!(call.response.model_id, "claude-opus-4-8");
    assert!(matches!(
        &call.response.content[0],
        ContentBlock::Opaque { data, .. } if data.get("type") == Some(&Value::from("fallback"))
    ));
    assert_eq!(call.usage_by_model.len(), 2);
    assert_eq!(call.response.usage.input_tokens, 900);
    assert_eq!(call.response.usage.output_tokens, 200);
    // 500 input tokens at $4/MTok, then 400 input and 200 output at $5 and $25.
    let expected = 500.0 * 4.0 / 1e6 + 400.0 * 5.0 / 1e6 + 200.0 * 25.0 / 1e6;
    let cost = call.cost_usd.expect("both models are priced");
    assert!((cost - expected).abs() < 1e-12, "{cost} != {expected}");
    assert_eq!(cost_usd(&routing, &call.usage_by_model), Some(cost));
}

#[tokio::test]
async fn the_gateway_refuses_more_output_tokens_than_the_profile_allows() {
    let server = Replay::start(Vec::new()).await;
    let provider: Arc<dyn ProviderAdapter> =
        Arc::new(AnthropicProvider::new(anthropic_settings(&server.base_url())).expect("provider"));
    let gateway = ModelGateway::with_providers(
        committed_routing(),
        &[],
        &env(&[]),
        BTreeMap::from([("haiku".to_owned(), provider)]),
    )
    .expect("gateway");
    let mut too_long = request(MODEL, "Size the plate.");
    too_long.max_output_tokens = 16_001;
    let error = gateway
        .complete(&id("haiku"), too_long, &ctx())
        .await
        .expect_err("over the profile limit");
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    assert!(server.captured().is_empty());

    let unknown = gateway
        .complete(&id("pliny"), request(MODEL, "Size the plate."), &ctx())
        .await
        .expect_err("pliny is not in use");
    assert_eq!(unknown.code(), ErrorCode::NotFound);
}
