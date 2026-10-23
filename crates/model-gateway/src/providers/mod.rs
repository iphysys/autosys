//! Provider adapters (blueprint section 14.4). Each turns the neutral
//! `contracts::model` types into one provider's wire format and back; its
//! wire types stay private to its module.

mod anthropic;
mod openai_compatible;

use std::time::Duration;

use async_trait::async_trait;
use contracts::types::TokenUsage;
use contracts::{Error, ErrorCode, ExecutionContext, ModelProvider, ModelRequest, ModelResponse};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{StatusCode, Url};
use serde::Serialize;
use serde_json::Value;

use crate::retry::AttemptError;

pub use crate::http::CaBundle;
pub use anthropic::{ANTHROPIC_VERSION, AnthropicProvider, AnthropicSettings, FALLBACK_BETA};
pub use openai_compatible::{OpenAiCompatibleProvider, OpenAiCompatibleSettings};

/// The tokens one attempt used on one model.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelUsage {
    pub model: String,
    pub usage: TokenUsage,
}

/// One model call with what accounting needs.
#[derive(Clone, Debug)]
pub struct ProviderCall {
    pub response: ModelResponse,
    /// Usage per model that ran: more than one entry when a refusal fallback
    /// ran another model. The response's own usage is their sum.
    pub usage_by_model: Vec<ModelUsage>,
    /// HTTP attempts made, retries included.
    pub attempts: u32,
    /// The provider's id for the request that succeeded, for support.
    pub request_id: Option<String>,
}

/// A provider adapter as the gateway uses it. `ModelProvider::complete`
/// returns the `response` of [`ProviderAdapter::call`].
#[async_trait]
pub trait ProviderAdapter: ModelProvider {
    async fn call(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ProviderCall, Error>;
}

/// A successful HTTP response.
pub(crate) struct HttpReply {
    pub body: String,
    pub request_id: Option<String>,
}

/// Posts `body` as JSON once. A response that is not a 2xx is an error whose
/// code follows the status: 429 and 5xx are `Unavailable`, 408 and 504 are
/// `Timeout` (both retryable). The message names the provider's error type
/// and message and the request id, never the request.
pub(crate) async fn post_json(
    client: &reqwest::Client,
    url: &Url,
    headers: &HeaderMap,
    body: &impl Serialize,
    timeout: Duration,
    provider: &str,
) -> Result<HttpReply, AttemptError> {
    let body = serde_json::to_vec(body).map_err(|e| {
        Error::new(
            ErrorCode::Internal,
            format!("{provider}: cannot serialize the request: {e}"),
        )
    })?;
    let exchange = async {
        let response = client
            .post(url.clone())
            .headers(headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| transport_error(provider, &e))?;
        let status = response.status();
        let request_id = header_text(response.headers(), "request-id")
            .or_else(|| header_text(response.headers(), "x-request-id"));
        let retry_after = header_text(response.headers(), "retry-after")
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map(Duration::from_secs_f64);
        let text = response
            .text()
            .await
            .map_err(|e| transport_error(provider, &e))?;
        if status.is_success() {
            Ok(HttpReply {
                body: text,
                request_id,
            })
        } else {
            Err(AttemptError {
                error: status_error(provider, status, &text, request_id.as_deref()),
                retry_after,
            })
        }
    };
    match tokio::time::timeout(timeout, exchange).await {
        Ok(result) => result,
        Err(_) => Err(Error::new(
            ErrorCode::Timeout,
            format!("{provider}: no response within {}s", timeout.as_secs()),
        )
        .into()),
    }
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn transport_error(provider: &str, error: &reqwest::Error) -> AttemptError {
    let code = if error.is_timeout() {
        ErrorCode::Timeout
    } else {
        ErrorCode::Unavailable
    };
    Error::new(code, format!("{provider}: request failed: {error}")).into()
}

/// The longest provider error message kept, so a large body cannot flood a
/// log or an event.
const MAX_ERROR_MESSAGE: usize = 500;

fn status_error(provider: &str, status: StatusCode, body: &str, request_id: Option<&str>) -> Error {
    let code = match status.as_u16() {
        400 | 413 | 422 => ErrorCode::InvalidInput,
        401..=403 => ErrorCode::PermissionDenied,
        404 => ErrorCode::NotFound,
        409 => ErrorCode::Conflict,
        408 | 504 => ErrorCode::Timeout,
        429 | 500..=599 => ErrorCode::Unavailable,
        _ => ErrorCode::Internal,
    };
    // Both providers put `type` and `message` under `error`; the rest of the
    // body is not kept.
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let detail = parsed.as_ref().and_then(|v| v.get("error"));
    let kind = detail
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message: String = detail
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("no message")
        .chars()
        .take(MAX_ERROR_MESSAGE)
        .collect();
    let request = request_id
        .map(|id| format!(" (request {id})"))
        .unwrap_or_default();
    Error::new(
        code,
        format!("{provider}: HTTP {status}, {kind}: {message}{request}"),
    )
}

/// Adds a configured header. Names and values were checked by `Routing`.
pub(crate) fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), Error> {
    let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
        Error::new(
            ErrorCode::InvalidInput,
            format!("`{name}` is not a valid header name"),
        )
    })?;
    let value = HeaderValue::from_str(value).map_err(|_| {
        Error::new(
            ErrorCode::InvalidInput,
            format!("header `{name}` has an invalid value"),
        )
    })?;
    headers.insert(name, value);
    Ok(())
}

/// A header carrying a secret: marked sensitive, so reqwest's `Debug`
/// output hides it.
pub(crate) fn sensitive_header(value: &str, what: &str) -> Result<HeaderValue, Error> {
    let mut value = HeaderValue::from_str(value).map_err(|_| {
        Error::new(
            ErrorCode::InvalidInput,
            format!("{what} contains characters not allowed in an HTTP header"),
        )
    })?;
    value.set_sensitive(true);
    Ok(value)
}

/// A response body that could not be read as the provider's format.
pub(crate) fn malformed(provider: &str, error: &serde_json::Error) -> Error {
    Error::new(
        ErrorCode::Internal,
        format!("{provider}: the response is not in the expected format: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_error_codes() {
        let cases = [
            (400, ErrorCode::InvalidInput),
            (401, ErrorCode::PermissionDenied),
            (402, ErrorCode::PermissionDenied),
            (403, ErrorCode::PermissionDenied),
            (404, ErrorCode::NotFound),
            (408, ErrorCode::Timeout),
            (409, ErrorCode::Conflict),
            (413, ErrorCode::InvalidInput),
            (429, ErrorCode::Unavailable),
            (500, ErrorCode::Unavailable),
            (503, ErrorCode::Unavailable),
            (504, ErrorCode::Timeout),
            (529, ErrorCode::Unavailable),
        ];
        for (status, code) in cases {
            let status = StatusCode::from_u16(status).expect("valid status");
            assert_eq!(status_error("p", status, "", None).code(), code, "{status}");
        }
    }

    #[test]
    fn error_messages_keep_only_type_message_and_request_id() {
        let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"},"request_id":"req_1","echo":"secret prompt"}"#;
        let error = status_error(
            "anthropic",
            StatusCode::from_u16(529).expect("valid status"),
            body,
            Some("req_1"),
        );
        assert!(
            error.message().contains("overloaded_error: Overloaded"),
            "{error}"
        );
        assert!(error.message().contains("req_1"), "{error}");
        assert!(!error.message().contains("secret prompt"), "{error}");
    }
}
