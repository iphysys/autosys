//! Claude through the Messages API (`POST /v1/messages`, API version
//! 2026-10-21), without streaming. The mapping to the neutral contract is
//! tabled in ADR-0027.

use std::time::Duration;

use async_trait::async_trait;
use contracts::ids::{Identifier, ToolName};
use contracts::types::{ModelEffort, ModelRetryPolicy, TokenUsage};
use contracts::{
    ContentBlock, Error, ErrorCode, ExecutionContext, Message, ModelProvider, ModelRequest,
    ModelResponse, ModelStopReason, Role,
};
use reqwest::Url;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    ModelUsage, ProviderAdapter, ProviderCall, insert_header, malformed, post_json,
    sensitive_header,
};
use crate::env::Secret;
use crate::http::{self, CaBundle};
use crate::retry::with_retries;

/// The API version this adapter's wire format follows.
pub const ANTHROPIC_VERSION: &str = "2026-10-21";
/// The beta that enables `fallbacks: "default"`.
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

const PROVIDER: &str = "anthropic";
const MESSAGES_PATH: &str = "v1/messages";

/// One Claude profile's settings, resolved from configuration and the
/// environment.
#[derive(Clone, Debug)]
pub struct AnthropicSettings {
    /// `https`, or `http` on a loopback host for tests.
    pub base_url: String,
    pub api_key: Secret,
    /// Sent as `anthropic-workspace-id` when set.
    pub workspace_id: Option<String>,
    pub effort: Option<ModelEffort>,
    /// Sends `fallbacks: "default"` with the [`FALLBACK_BETA`] header.
    pub refusal_fallback: bool,
    /// Extra headers from the profile.
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
    pub retry: ModelRetryPolicy,
    pub ca_bundle: Option<CaBundle>,
}

/// The Claude adapter.
#[derive(Debug)]
pub struct AnthropicProvider {
    id: Identifier,
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    effort: Option<ModelEffort>,
    refusal_fallback: bool,
    timeout: Duration,
    retry: ModelRetryPolicy,
}

impl AnthropicProvider {
    pub fn new(settings: AnthropicSettings) -> Result<Self, Error> {
        let base = http::base_url(&settings.base_url, "anthropic base_url")?;
        let mut headers = HeaderMap::new();
        for (name, value) in &settings.headers {
            insert_header(&mut headers, name, value)?;
        }
        headers.insert(
            AUTHORIZATION,
            sensitive_header(
                &format!("Bearer {}", settings.api_key.expose()),
                "the Anthropic API key",
            )?,
        );
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        if let Some(workspace) = &settings.workspace_id {
            headers.insert(
                "anthropic-workspace-id",
                HeaderValue::from_str(workspace).map_err(|_| {
                    Error::new(
                        ErrorCode::InvalidInput,
                        "the Anthropic workspace id contains characters not allowed in an HTTP header",
                    )
                })?,
            );
        }
        if settings.refusal_fallback {
            headers.insert("anthropic-beta", HeaderValue::from_static(FALLBACK_BETA));
        }
        Ok(Self {
            id: provider_id(),
            client: http::provider_client(settings.timeout, settings.ca_bundle.as_ref())?,
            url: http::endpoint(&base, MESSAGES_PATH)?,
            headers,
            effort: settings.effort,
            refusal_fallback: settings.refusal_fallback,
            timeout: settings.timeout,
            retry: settings.retry,
        })
    }
}

#[allow(
    clippy::expect_used,
    reason = "\"anthropic\" matches the Identifier pattern; checked by tests::provider_id_is_valid"
)]
fn provider_id() -> Identifier {
    Identifier::try_from(PROVIDER).expect("a valid identifier")
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
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
impl ProviderAdapter for AnthropicProvider {
    async fn call(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ProviderCall, Error> {
        ctx.ensure_not_cancelled()?;
        let body = WireRequest {
            model: &request.model_id,
            max_tokens: request.max_output_tokens,
            system: &request.system,
            messages: wire_messages(&request.messages, &self.id),
            tools: request
                .tools
                .iter()
                .map(|t| WireTool {
                    name: t.name.as_str(),
                    description: &t.description,
                    input_schema: &t.input_schema,
                })
                .collect(),
            output_config: self.effort.map(|effort| OutputConfig { effort }),
            fallbacks: self.refusal_fallback.then_some("default"),
        };
        let (client, url, headers, body) = (&self.client, &self.url, &self.headers, &body);
        let retried = with_retries(&self.retry, ctx, move |_| {
            post_json(client, url, headers, body, self.timeout, PROVIDER)
        })
        .await?;
        let wire: WireResponse =
            serde_json::from_str(&retried.value.body).map_err(|e| malformed(PROVIDER, &e))?;
        let (response, usage_by_model) = normalize(wire, &self.id)?;
        Ok(ProviderCall {
            response,
            usage_by_model,
            attempts: retried.attempts,
            request_id: retried.value.request_id,
        })
    }
}

// Request

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "str::is_empty")]
    system: &'a str,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallbacks: Option<&'static str>,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    content: Vec<Value>,
}

#[derive(Serialize)]
struct WireTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

#[derive(Serialize)]
struct OutputConfig {
    effort: ModelEffort,
}

/// Neutral messages as Messages API messages. Opaque blocks of this provider
/// go back verbatim; those of other providers are dropped, and so is a
/// message left with no content.
fn wire_messages(messages: &[Message], provider: &Identifier) -> Vec<WireMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let content: Vec<Value> = message
                .content
                .iter()
                .filter_map(|block| wire_block(block, provider))
                .collect();
            (!content.is_empty()).then_some(WireMessage {
                role: match message.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content,
            })
        })
        .collect()
}

fn wire_block(block: &ContentBlock, provider: &Identifier) -> Option<Value> {
    let mut wire = Map::new();
    match block {
        ContentBlock::Text { text } => {
            wire.insert("type".into(), "text".into());
            wire.insert("text".into(), text.as_str().into());
        }
        ContentBlock::ToolCall {
            id,
            name,
            arguments,
        } => {
            wire.insert("type".into(), "tool_use".into());
            wire.insert("id".into(), id.as_str().into());
            wire.insert("name".into(), name.as_str().into());
            wire.insert("input".into(), Value::Object(arguments.clone()));
        }
        ContentBlock::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            wire.insert("type".into(), "tool_result".into());
            wire.insert("tool_use_id".into(), call_id.as_str().into());
            wire.insert("content".into(), content.as_str().into());
            wire.insert("is_error".into(), (*is_error).into());
        }
        ContentBlock::Opaque {
            provider: origin,
            data,
        } => {
            if origin != provider {
                return None;
            }
            return Some(Value::Object(data.clone()));
        }
    }
    Some(Value::Object(wire))
}

// Response

#[derive(Deserialize)]
struct WireResponse {
    model: String,
    content: Vec<Map<String, Value>>,
    stop_reason: Option<String>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    /// One entry per model that ran, when a refusal fallback was enabled.
    #[serde(default)]
    iterations: Option<Vec<WireIteration>>,
}

#[derive(Deserialize)]
struct WireIteration {
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

fn normalize(
    wire: WireResponse,
    provider: &Identifier,
) -> Result<(ModelResponse, Vec<ModelUsage>), Error> {
    let usage = wire.usage.ok_or_else(|| {
        Error::new(
            ErrorCode::Internal,
            "anthropic: the response reports no token usage, so the call cannot be budgeted",
        )
    })?;
    let usage_by_model = usage_by_model(&wire.model, usage);

    let mut content = wire
        .content
        .into_iter()
        .map(|block| neutral_block(block, provider))
        .collect::<Result<Vec<_>, _>>()?;
    let has_calls = content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolCall { .. }));
    let stop_reason = match wire.stop_reason.as_deref() {
        Some("end_turn" | "stop_sequence") if has_calls => ModelStopReason::ToolUse,
        Some("end_turn" | "stop_sequence") => ModelStopReason::EndTurn,
        Some("tool_use") if has_calls => ModelStopReason::ToolUse,
        Some("tool_use") => {
            return Err(Error::new(
                ErrorCode::Internal,
                "anthropic: stop_reason tool_use without a tool_use block",
            ));
        }
        Some("max_tokens" | "model_context_window_exceeded") => ModelStopReason::MaxTokens,
        Some("refusal") => ModelStopReason::Refusal,
        other => {
            return Err(Error::new(
                ErrorCode::Internal,
                format!("anthropic: unexpected stop_reason {other:?}"),
            ));
        }
    };
    // A cut-off or declined response may hold an incomplete tool call; it is
    // never run.
    if matches!(
        stop_reason,
        ModelStopReason::MaxTokens | ModelStopReason::Refusal
    ) {
        content.retain(|b| !matches!(b, ContentBlock::ToolCall { .. }));
    }

    let total = usage_by_model.iter().fold(
        TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
        },
        |sum, attempt| TokenUsage {
            input_tokens: sum.input_tokens.saturating_add(attempt.usage.input_tokens),
            output_tokens: sum
                .output_tokens
                .saturating_add(attempt.usage.output_tokens),
        },
    );
    Ok((
        ModelResponse {
            model_id: wire.model,
            content,
            stop_reason,
            usage: total,
        },
        usage_by_model,
    ))
}

/// Usage per model that ran. Cache reads and writes count as input: prompt
/// caching is not used yet, so they are priced as input too.
fn usage_by_model(model: &str, usage: WireUsage) -> Vec<ModelUsage> {
    let input = |base: u64, created: Option<u64>, read: Option<u64>| {
        base.saturating_add(created.unwrap_or(0))
            .saturating_add(read.unwrap_or(0))
    };
    match usage.iterations {
        Some(iterations) if !iterations.is_empty() => iterations
            .into_iter()
            .map(|it| ModelUsage {
                model: it.model,
                usage: TokenUsage {
                    input_tokens: input(
                        it.input_tokens,
                        it.cache_creation_input_tokens,
                        it.cache_read_input_tokens,
                    ),
                    output_tokens: it.output_tokens,
                },
            })
            .collect(),
        _ => vec![ModelUsage {
            model: model.to_owned(),
            usage: TokenUsage {
                input_tokens: input(
                    usage.input_tokens,
                    usage.cache_creation_input_tokens,
                    usage.cache_read_input_tokens,
                ),
                output_tokens: usage.output_tokens,
            },
        }],
    }
}

/// `text` and `tool_use` blocks become their neutral forms; every other
/// block (thinking, redacted thinking, fallback markers) is kept opaque, to
/// be sent back unchanged.
fn neutral_block(block: Map<String, Value>, provider: &Identifier) -> Result<ContentBlock, Error> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            let text = block.get("text").and_then(Value::as_str).ok_or_else(|| {
                Error::new(ErrorCode::Internal, "anthropic: a text block without text")
            })?;
            Ok(ContentBlock::Text {
                text: text.to_owned(),
            })
        }
        Some("tool_use") => {
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    Error::new(
                        ErrorCode::Internal,
                        "anthropic: a tool_use block without an id",
                    )
                })?;
            let raw_name = block.get("name").and_then(Value::as_str).unwrap_or("");
            let name = ToolName::try_from(raw_name).map_err(|_| {
                Error::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "anthropic: the model asked for a tool with an invalid name {raw_name:?}"
                    ),
                )
            })?;
            let Some(Value::Object(arguments)) = block.get("input") else {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "anthropic: the model's call to `{raw_name}` has arguments that are not a JSON object"
                    ),
                ));
            };
            Ok(ContentBlock::ToolCall {
                id: id.to_owned(),
                name,
                arguments: arguments.clone(),
            })
        }
        _ => Ok(ContentBlock::Opaque {
            provider: provider.clone(),
            data: block,
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn provider_id_is_valid() {
        assert_eq!(provider_id().as_str(), PROVIDER);
    }

    fn response(value: Value) -> Result<(ModelResponse, Vec<ModelUsage>), Error> {
        normalize(
            serde_json::from_value(value).expect("wire response"),
            &provider_id(),
        )
    }

    #[test]
    fn a_tool_use_stop_without_a_call_is_an_error() {
        let result = response(json!({
            "model": "claude-opus-5-5",
            "content": [{ "type": "text", "text": "Calling." }],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }));
        assert_eq!(result.err().map(|e| e.code()), Some(ErrorCode::Internal));
    }

    #[test]
    fn unknown_stop_reasons_and_missing_usage_are_errors() {
        let pause = response(json!({
            "model": "m", "content": [], "stop_reason": "pause_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }));
        assert!(
            pause
                .err()
                .is_some_and(|e| e.message().contains("pause_turn"))
        );
        let no_usage = response(json!({ "model": "m", "content": [], "stop_reason": "end_turn" }));
        assert_eq!(no_usage.err().map(|e| e.code()), Some(ErrorCode::Internal));
    }

    #[test]
    fn foreign_opaque_blocks_and_empty_messages_are_dropped() {
        let other: Identifier = Identifier::try_from("other").expect("valid id");
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Opaque {
                    provider: other,
                    data: Map::new(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Hello.".into(),
                }],
            },
        ];
        let wire = wire_messages(&messages, &provider_id());
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "user");
    }
}
