use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use contracts::ids::Identifier;
use contracts::types::TokenUsage;
use contracts::{
    ContentBlock, Error, ErrorCode, ExecutionContext, ModelProvider, ModelRequest, ModelResponse,
    ModelStopReason,
};
use serde_json::Value;

use crate::builders::id;

/// Returns scripted responses in order and records every request it receives.
/// Running out of script is an `Internal` error, so a test that makes an
/// unexpected extra call fails loudly.
pub struct ScriptedModelProvider {
    id: Identifier,
    script: Mutex<VecDeque<Result<ModelResponse, Error>>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedModelProvider {
    pub fn new(script: impl IntoIterator<Item = Result<ModelResponse, Error>>) -> Self {
        Self {
            id: id("scripted"),
            script: Mutex::new(script.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Requests received so far, in order. Cancelled calls are not recorded.
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

#[async_trait]
impl ModelProvider for ScriptedModelProvider {
    fn provider_id(&self) -> &Identifier {
        &self.id
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ModelResponse, Error> {
        ctx.ensure_not_cancelled()?;
        let call = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            requests.len()
        };
        self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
            Err(Error::new(
                ErrorCode::Internal,
                format!("model script exhausted at call {call}"),
            ))
        })
    }
}

/// A response that ends the turn with text.
pub fn text_response(text: &str) -> ModelResponse {
    ModelResponse {
        model_id: "scripted-model".to_owned(),
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        stop_reason: ModelStopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}

/// A response asking for tool calls, each `(call id, tool name, arguments)`.
/// `arguments` must be a JSON object.
pub fn tool_call_response(calls: &[(&str, &str, Value)]) -> ModelResponse {
    let content = calls
        .iter()
        .map(|(call_id, tool, arguments)| ContentBlock::ToolCall {
            id: (*call_id).to_owned(),
            name: id(tool),
            arguments: arguments
                .as_object()
                .cloned()
                .expect("tool call arguments must be a JSON object"),
        })
        .collect();
    ModelResponse {
        model_id: "scripted-model".to_owned(),
        content,
        stop_reason: ModelStopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}
