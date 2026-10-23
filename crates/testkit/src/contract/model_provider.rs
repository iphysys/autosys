use std::collections::HashSet;

use contracts::{
    ContentBlock, ErrorCode, Message, ModelProvider, ModelRequest, ModelResponse, ModelStopReason,
    Role,
};

use super::{assert_code, cancelled_ctx, ctx};

/// The `ModelProvider` contract. `make` builds a provider ready to answer
/// `request` once (for example with a recorded response).
pub async fn model_provider<P, F>(make: F, request: ModelRequest)
where
    P: ModelProvider,
    F: Fn() -> P,
{
    let provider = make();
    let response = provider
        .complete(&request, &ctx())
        .await
        .unwrap_or_else(|e| panic!("model_provider: request failed: {e}"));
    tool_calls_match_stop_reason(&response, "model_provider");

    assert_code(
        make().complete(&request, &cancelled_ctx()).await,
        ErrorCode::Cancelled,
        "model_provider: cancelled context",
    );
}

/// A tool-call round trip. `make` builds a provider ready to answer twice.
/// The response to `request` must ask for at least one tool call. The suite
/// then sends the assistant turn back as received, opaque blocks included,
/// followed by one result per call, and the second response must end the
/// turn without asking for another tool. Returns the follow-up request, so
/// a caller can check what the provider received.
pub async fn model_provider_tool_round_trip<P, F>(make: F, request: ModelRequest) -> ModelRequest
where
    P: ModelProvider,
    F: Fn() -> P,
{
    const CASE: &str = "model_provider_tool_round_trip";
    let provider = make();
    let first = provider
        .complete(&request, &ctx())
        .await
        .unwrap_or_else(|e| panic!("{CASE}: first request failed: {e}"));
    let call_ids = tool_calls_match_stop_reason(&first, CASE);
    assert!(
        !call_ids.is_empty(),
        "{CASE}: the first response asks for no tool call"
    );

    let mut follow_up = request.clone();
    follow_up.messages.push(Message {
        role: Role::Assistant,
        content: first.content.clone(),
    });
    follow_up.messages.push(Message {
        role: Role::User,
        content: call_ids
            .iter()
            .map(|id| ContentBlock::ToolResult {
                call_id: id.clone(),
                content: format!("result of {id}"),
                is_error: false,
            })
            .collect(),
    });

    let second = provider
        .complete(&follow_up, &ctx())
        .await
        .unwrap_or_else(|e| panic!("{CASE}: follow-up request failed: {e}"));
    tool_calls_match_stop_reason(&second, CASE);
    assert_ne!(
        second.stop_reason,
        ModelStopReason::ToolUse,
        "{CASE}: the follow-up response asks for another tool"
    );
    follow_up
}

/// Checks that the stop reason is `ToolUse` exactly when the response holds a
/// tool call, and that call ids are non-empty and unique. Returns the ids.
fn tool_calls_match_stop_reason(response: &ModelResponse, case: &str) -> Vec<String> {
    let call_ids: Vec<String> = response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        response.stop_reason == ModelStopReason::ToolUse,
        !call_ids.is_empty(),
        "{case}: stop reason {:?} with {} tool calls",
        response.stop_reason,
        call_ids.len()
    );
    assert!(
        call_ids.iter().all(|id| !id.is_empty()),
        "{case}: empty tool call id"
    );
    let unique: HashSet<&String> = call_ids.iter().collect();
    assert_eq!(
        unique.len(),
        call_ids.len(),
        "{case}: repeated tool call id"
    );
    call_ids
}
