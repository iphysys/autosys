//! Live smoke tests against the real providers. Ignored by default: they need
//! real keys in the environment and cost money. Run them by hand to check
//! that the hand-written recordings still match the live wire format:
//!
//! ```text
//! cargo test -p synth-model-gateway --test live -- --ignored --test-threads=1
//! ```
//!
//! `live_claude_*` needs `ANTHROPIC_API_KEY` (and `ANTHROPIC_WORKSPACE_ID` for
//! a key not scoped to a workspace); `live_pliny_text` needs `PLINY_API_KEY`
//! and `PLINY_BASE_URL`, and `INTERNAL_CA_BUNDLE` if the endpoint uses an
//! internal CA.
//!
//! `live_claude_tool_round_trip` uses the default profile, haiku; choose another
//! Claude model with `SYNTH_MODEL_PROFILE=sonnet` or `SYNTH_MODEL_PROFILE=opus`.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

mod support;

use std::collections::HashMap;

use contracts::{ContentBlock, Message, ModelStopReason, Role};
use model_gateway::{ModelGateway, PROFILE_OVERRIDE_ENV};
use support::{committed_routing, ctx, request};
use testkit::builders::id;

/// The process environment, with `overrides` applied.
fn process_env(overrides: &[(&str, &str)]) -> HashMap<String, String> {
    let mut vars: HashMap<String, String> = std::env::vars().collect();
    for (k, v) in overrides {
        vars.insert((*k).to_owned(), (*v).to_owned());
    }
    vars
}

#[tokio::test]
#[ignore = "calls the Claude API; needs ANTHROPIC_API_KEY and costs money"]
async fn live_claude_tool_round_trip() {
    let gateway = ModelGateway::start(committed_routing(), &[], &process_env(&[]))
        .expect("set ANTHROPIC_API_KEY");
    // The default profile, or the Claude model SYNTH_MODEL_PROFILE names.
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
    assert!(call.cost_usd.is_some());

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
        .expect("follow-up call with the thinking blocks sent back");
    assert_ne!(answer.response.stop_reason, ModelStopReason::Refusal);
}

#[tokio::test]
#[ignore = "calls Pliny; needs PLINY_API_KEY and PLINY_BASE_URL"]
async fn live_pliny_text() {
    let vars = process_env(&[(PROFILE_OVERRIDE_ENV, "pliny")]);
    let gateway =
        ModelGateway::start(committed_routing(), &[], &vars).expect("set the PLINY_ variables");
    let mut text_only = request("", "Reply with the single word OK.");
    text_only.tools.clear();
    text_only.max_output_tokens = 256;
    let call = gateway
        .complete(&id("pliny"), text_only, &ctx())
        .await
        .expect("pliny call");
    assert!(call.response.usage.output_tokens > 0, "{:?}", call.response);
}
