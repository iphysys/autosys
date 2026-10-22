//! The model gateway contract (blueprint section 4.1, item 8).
//!
//! Provider-neutral messages, tool-call requests, stop reasons and usage.
//! Provider-specific types stay inside each adapter. The types derive serde so
//! recorded responses can be stored as test fixtures; they are not persisted
//! as harness records.

use async_trait::async_trait;
use schema_types::TokenUsage;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::context::ExecutionContext;
use crate::error::Error;
use crate::ids::{Identifier, ToolName};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequest {
    /// A concrete model id; the gateway resolves it from the agent's model profile.
    pub model_id: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// A tool call the model asks for. Untrusted: the tool gateway validates it.
    ToolCall {
        id: String,
        name: ToolName,
        arguments: Map<String, Value>,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub name: ToolName,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelResponse {
    pub model_id: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: ModelStopReason,
    pub usage: TokenUsage,
}

/// Why the model stopped generating. Not the run's stop reason, which checks
/// and gates decide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStopReason {
    EndTurn,
    /// The response contains at least one tool call.
    ToolUse,
    MaxTokens,
    Refusal,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn provider_id(&self) -> &Identifier;

    /// One model call. The stop reason is `ToolUse` exactly when the response
    /// contains a tool call. Returns `Cancelled` if `ctx` is cancelled.
    async fn complete(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ModelResponse, Error>;
}
