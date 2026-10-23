//! A model behind an OpenAI-compatible chat-completions endpoint
//! (`POST {base}/chat/completions`), without streaming: the internal Pliny
//! platform. The mapping to the neutral contract is tabled in ADR-0027.

use std::time::Duration;

use async_trait::async_trait;
use contracts::ids::{Identifier, ToolName};
use contracts::types::{ModelRetryPolicy, TokenUsage};
use contracts::{
    ContentBlock, Error, ErrorCode, ExecutionContext, Message, ModelProvider, ModelRequest,
    ModelResponse, ModelStopReason, Role,
};
use reqwest::Url;
use reqwest::header::{AUTHORIZATION, HeaderMap};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{
    ModelUsage, ProviderAdapter, ProviderCall, insert_header, malformed, post_json,
    sensitive_header,
};
use crate::env::Secret;
use crate::http::{self, CaBundle};
use crate::retry::with_retries;

const PROVIDER: &str = "openai_compatible";
const COMPLETIONS_PATH: &str = "chat/completions";
/// The chat format has no error flag on a tool result, so a failed one is
/// marked in its text.
const TOOL_ERROR_PREFIX: &str = "Error: ";

/// One OpenAI-compatible profile's settings, resolved from configuration and
/// the environment.
#[derive(Clone, Debug)]
pub struct OpenAiCompatibleSettings {
    /// Up to and excluding `/chat/completions`. `https`, or `http` on a
    /// loopback host for tests.
    pub base_url: String,
    /// The environment variable `base_url` came from, named in errors.
    pub base_url_origin: String,
    pub api_key: Secret,
    /// Extra headers from the profile.
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
    pub retry: ModelRetryPolicy,
    pub ca_bundle: Option<CaBundle>,
}

/// The OpenAI-compatible adapter.
#[derive(Debug)]
pub struct OpenAiCompatibleProvider {
    id: Identifier,
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    timeout: Duration,
    retry: ModelRetryPolicy,
}

impl OpenAiCompatibleProvider {
    pub fn new(settings: OpenAiCompatibleSettings) -> Result<Self, Error> {
        let base = http::base_url(&settings.base_url, &settings.base_url_origin)?;
        let mut headers = HeaderMap::new();
        for (name, value) in &settings.headers {
            insert_header(&mut headers, name, value)?;
        }
        headers.insert(
            AUTHORIZATION,
            sensitive_header(
                &format!("Bearer {}", settings.api_key.expose()),
                "the API key",
            )?,
        );
        Ok(Self {
            id: provider_id(),
            client: http::provider_client(settings.timeout, settings.ca_bundle.as_ref())?,
            url: http::endpoint(&base, COMPLETIONS_PATH)?,
            headers,
            timeout: settings.timeout,
            retry: settings.retry,
        })
    }
}

#[allow(
    clippy::expect_used,
    reason = "\"openai_compatible\" matches the Identifier pattern; checked by tests::provider_id_is_valid"
)]
fn provider_id() -> Identifier {
    Identifier::try_from(PROVIDER).expect("a valid identifier")
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
    fn provider_id(&self) -> &Identifier {
        &self.id
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ModelResponse, Error> {
        self.call(request, ctx).await.map(|call| call.response)
    }
}

#[async_trait]
impl ProviderAdapter for OpenAiCompatibleProvider {
    async fn call(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ProviderCall, Error> {
        ctx.ensure_not_cancelled()?;
        let body = WireRequest {
            model: &request.model_id,
            messages: wire_messages(&request.system, &request.messages),
            tools: request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name.as_str(),
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect(),
            max_tokens: request.max_output_tokens,
            stream: false,
        };
        let (client, url, headers, body) = (&self.client, &self.url, &self.headers, &body);
        let retried = with_retries(&self.retry, ctx, move |_| {
            post_json(client, url, headers, body, self.timeout, PROVIDER)
        })
        .await?;
        let wire: WireResponse =
            serde_json::from_str(&retried.value.body).map_err(|e| malformed(PROVIDER, &e))?;
        let (response, usage) = normalize(wire, &request.model_id)?;
        Ok(ProviderCall {
            usage_by_model: vec![ModelUsage {
                model: response.model_id.clone(),
                usage,
            }],
            response,
            attempts: retried.attempts,
            request_id: retried.value.request_id,
        })
    }
}

// Request

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    max_tokens: u32,
    stream: bool,
}

/// Neutral messages as chat messages: the system prompt first; a user turn's
/// tool results as `tool` messages, then its text; an assistant turn's text
/// and tool calls in one message. Opaque blocks belong to other providers
/// and are dropped.
fn wire_messages(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut wire = Vec::new();
    if !system.is_empty() {
        wire.push(json!({ "role": "system", "content": system }));
    }
    for message in messages {
        let text = joined_text(&message.content);
        match message.role {
            Role::User => {
                for block in &message.content {
                    if let ContentBlock::ToolResult {
                        call_id,
                        content,
                        is_error,
                    } = block
                    {
                        let content = if *is_error {
                            format!("{TOOL_ERROR_PREFIX}{content}")
                        } else {
                            content.clone()
                        };
                        wire.push(
                            json!({ "role": "tool", "tool_call_id": call_id, "content": content }),
                        );
                    }
                }
                if let Some(text) = text {
                    wire.push(json!({ "role": "user", "content": text }));
                }
            }
            Role::Assistant => {
                let calls: Vec<Value> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name.as_str(),
                                "arguments": Value::Object(arguments.clone()).to_string(),
                            }
                        })),
                        _ => None,
                    })
                    .collect();
                if text.is_none() && calls.is_empty() {
                    continue;
                }
                let mut assistant = Map::new();
                assistant.insert("role".into(), "assistant".into());
                assistant.insert("content".into(), text.map_or(Value::Null, Value::String));
                if !calls.is_empty() {
                    assistant.insert("tool_calls".into(), Value::Array(calls));
                }
                wire.push(Value::Object(assistant));
            }
        }
    }
    wire
}

/// The message's text blocks joined by a blank line, if it has any.
fn joined_text(content: &[ContentBlock]) -> Option<String> {
    let texts: Vec<&str> = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n\n"))
}

// Response

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    model: Option<String>,
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireReply,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireReply {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<WireToolCall>>,
}

#[derive(Deserialize)]
struct WireToolCall {
    id: String,
    function: WireFunction,
}

#[derive(Deserialize)]
struct WireFunction {
    name: String,
    /// A JSON object, serialized.
    arguments: String,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

fn normalize(
    wire: WireResponse,
    requested_model: &str,
) -> Result<(ModelResponse, TokenUsage), Error> {
    let usage = wire.usage.ok_or_else(|| {
        Error::new(
            ErrorCode::Internal,
            "openai_compatible: the response reports no token usage, so the call cannot be budgeted",
        )
    })?;
    let usage = TokenUsage {
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
    };
    let choice = wire.choices.into_iter().next().ok_or_else(|| {
        Error::new(
            ErrorCode::Internal,
            "openai_compatible: the response has no choice",
        )
    })?;

    let mut content = Vec::new();
    if let Some(text) = choice.message.content.filter(|t| !t.is_empty()) {
        content.push(ContentBlock::Text { text });
    }
    for call in choice.message.tool_calls.unwrap_or_default() {
        content.push(tool_call(call)?);
    }
    let has_calls = content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolCall { .. }));
    let stop_reason = match choice.finish_reason.as_deref() {
        Some("stop") if has_calls => ModelStopReason::ToolUse,
        Some("stop") => ModelStopReason::EndTurn,
        Some("tool_calls" | "function_call") if has_calls => ModelStopReason::ToolUse,
        Some("tool_calls" | "function_call") => {
            return Err(Error::new(
                ErrorCode::Internal,
                "openai_compatible: finish_reason tool_calls without a tool call",
            ));
        }
        Some("length") => ModelStopReason::MaxTokens,
        Some("content_filter") => ModelStopReason::Refusal,
        other => {
            return Err(Error::new(
                ErrorCode::Internal,
                format!("openai_compatible: unexpected finish_reason {other:?}"),
            ));
        }
    };
    // A cut-off or filtered response may hold an incomplete tool call; it is
    // never run.
    if matches!(
        stop_reason,
        ModelStopReason::MaxTokens | ModelStopReason::Refusal
    ) {
        content.retain(|b| !matches!(b, ContentBlock::ToolCall { .. }));
    }
    Ok((
        ModelResponse {
            model_id: wire
                .model
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| requested_model.to_owned()),
            content,
            stop_reason,
            usage: usage.clone(),
        },
        usage,
    ))
}

fn tool_call(call: WireToolCall) -> Result<ContentBlock, Error> {
    let raw_name = call.function.name;
    let name = ToolName::try_from(raw_name.as_str()).map_err(|_| {
        Error::new(
            ErrorCode::InvalidInput,
            format!(
                "openai_compatible: the model asked for a tool with an invalid name {raw_name:?}"
            ),
        )
    })?;
    if call.id.is_empty() {
        return Err(Error::new(
            ErrorCode::Internal,
            "openai_compatible: a tool call without an id",
        ));
    }
    // A call without arguments may come as an empty string.
    let arguments = if call.function.arguments.trim().is_empty() {
        Map::new()
    } else {
        match serde_json::from_str::<Value>(&call.function.arguments) {
            Ok(Value::Object(arguments)) => arguments,
            _ => {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "openai_compatible: the model's call to `{raw_name}` has arguments that are not a JSON object"
                    ),
                ));
            }
        }
    };
    Ok(ContentBlock::ToolCall {
        id: call.id,
        name,
        arguments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_id_is_valid() {
        assert_eq!(provider_id().as_str(), PROVIDER);
    }

    fn response(value: Value) -> Result<(ModelResponse, TokenUsage), Error> {
        normalize(
            serde_json::from_value(value).expect("wire response"),
            "requested",
        )
    }

    #[test]
    fn malformed_tool_arguments_are_invalid_input() {
        let result = response(json!({
            "choices": [{
                "message": { "content": null, "tool_calls": [{
                    "id": "call-1", "type": "function",
                    "function": { "name": "measure", "arguments": "{\"part\": " }
                }]},
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
        }));
        assert_eq!(
            result.err().map(|e| e.code()),
            Some(ErrorCode::InvalidInput)
        );
    }

    #[test]
    fn calls_under_a_stop_finish_are_tool_use_and_missing_model_falls_back() {
        let (response, _) = response(json!({
            "choices": [{
                "message": { "content": "", "tool_calls": [{
                    "id": "call-1", "type": "function",
                    "function": { "name": "measure", "arguments": "" }
                }]},
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
        }))
        .expect("valid response");
        assert_eq!(response.stop_reason, ModelStopReason::ToolUse);
        assert_eq!(response.model_id, "requested");
        assert_eq!(response.content.len(), 1);
    }

    #[test]
    fn missing_usage_and_unknown_finish_reasons_are_errors() {
        let no_usage = response(json!({
            "choices": [{ "message": { "content": "Hi." }, "finish_reason": "stop" }]
        }));
        assert_eq!(no_usage.err().map(|e| e.code()), Some(ErrorCode::Internal));
        let unknown = response(json!({
            "choices": [{ "message": { "content": "Hi." }, "finish_reason": "eos" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
        }));
        assert!(unknown.err().is_some_and(|e| e.message().contains("eos")));
    }

    #[test]
    fn tool_results_become_tool_messages_and_errors_are_marked() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    call_id: "call-1".into(),
                    content: "too thin".into(),
                    is_error: true,
                },
                ContentBlock::Text {
                    text: "Try again.".into(),
                },
            ],
        }];
        let wire = wire_messages("System.", &messages);
        assert_eq!(
            wire,
            vec![
                json!({ "role": "system", "content": "System." }),
                json!({ "role": "tool", "tool_call_id": "call-1", "content": "Error: too thin" }),
                json!({ "role": "user", "content": "Try again." }),
            ]
        );
    }
}
