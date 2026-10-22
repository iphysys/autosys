use std::collections::HashSet;

use contracts::{ContentBlock, ErrorCode, ModelProvider, ModelRequest, ModelStopReason};

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
    let call_ids: Vec<&str> = response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        response.stop_reason == ModelStopReason::ToolUse,
        !call_ids.is_empty(),
        "model_provider: stop reason {:?} with {} tool calls",
        response.stop_reason,
        call_ids.len()
    );
    assert!(
        call_ids.iter().all(|id| !id.is_empty()),
        "model_provider: empty tool call id"
    );
    let unique: HashSet<&&str> = call_ids.iter().collect();
    assert_eq!(
        unique.len(),
        call_ids.len(),
        "model_provider: repeated tool call id"
    );

    assert_code(
        make().complete(&request, &cancelled_ctx()).await,
        ErrorCode::Cancelled,
        "model_provider: cancelled context",
    );
}
