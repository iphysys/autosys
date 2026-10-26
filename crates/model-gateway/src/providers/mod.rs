//! Provider adapters (blueprint section 14.4, ADR-0028). Each provider is one
//! module: its adapter, which turns the neutral `contracts::model` types into
//! its wire format and back, its [`ProviderFactory`], its tests and its
//! recordings. Its settings schema is
//! `schemas/config/model-providers/<provider>.schema.json`.
//!
//! This file holds what every provider shares, and [`default_registry`], the
//! one list of registered providers. Removing a provider is: delete its
//! module and its settings schema, and remove its two lines here and its line
//! in `crates/schema-types/tests/schemas.rs`.

pub mod openai_compatible;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use contracts::types::{EnvVarName, ModelHeaders, TokenUsage};
use contracts::{Error, ErrorCode, ExecutionContext, ModelProvider, ModelRequest, ModelResponse};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{StatusCode, Url};
use serde::Serialize;
use serde_json::Value;

use crate::env::Env;
use crate::registry::{ProviderFactory, ProviderRegistry};
use crate::retry::AttemptError;

pub use crate::http::CaBundle;

/// The providers the harness ships with, in registration order.
pub fn default_registry() -> Result<ProviderRegistry, Error> {
    let factories: Vec<Arc<dyn ProviderFactory>> = vec![Arc::new(openai_compatible::Factory)];
    ProviderRegistry::new(factories)
}

/// Headers the gateway sets itself, or that carry credentials. A profile may
/// not configure them: credentials come from environment variables only.
const RESERVED_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "cookie",
    "host",
    "content-type",
    "content-length",
    "transfer-encoding",
    "connection",
];

/// Problems with configured headers: invalid names or values, reserved
/// names, and names starting with one of the provider's `reserved_prefixes`.
pub(crate) fn header_problems(
    headers: Option<&ModelHeaders>,
    reserved_prefixes: &[&str],
) -> Vec<String> {
    let Some(headers) = headers else {
        return Vec::new();
    };
    let mut sorted: Vec<_> = headers.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut problems = Vec::new();
    for (header, value) in sorted {
        let lower = header.to_ascii_lowercase();
        if HeaderName::from_bytes(header.as_bytes()).is_err() {
            problems.push(format!("`{header}` is not a valid header name"));
        } else if RESERVED_HEADERS.contains(&lower.as_str())
            || reserved_prefixes.iter().any(|p| lower.starts_with(p))
        {
            problems.push(format!(
                "header `{header}` is set by the gateway or carries credentials, and may not be configured"
            ));
        }
        if HeaderValue::from_str(value.as_str()).is_err() {
            problems.push(format!("header `{header}` has an invalid value"));
        }
    }
    problems
}

/// `Ok` if `problems` is empty, else one `InvalidInput` error listing them.
pub(crate) fn no_problems(problems: Vec<String>) -> Result<(), Error> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::new(ErrorCode::InvalidInput, problems.join("; ")))
    }
}

/// The configured headers as name and value pairs, sorted by name.
pub(crate) fn header_pairs(headers: Option<&ModelHeaders>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = headers
        .map(|h| {
            h.iter()
                .map(|(k, v)| (k.clone(), v.as_str().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    pairs.sort();
    pairs
}

/// Reads a profile's environment variables, collecting every missing one so
/// a single error names them all. Public, like [`CaBundle`], as part of the
/// toolkit provider modules build on, so it does not depend on which
/// providers use which method.
pub struct EnvReader<'a> {
    env: &'a dyn Env,
    missing: Vec<String>,
}

impl<'a> EnvReader<'a> {
    pub fn new(env: &'a dyn Env) -> Self {
        Self {
            env,
            missing: Vec::new(),
        }
    }

    /// The variable's value; recorded as missing when unset or blank.
    pub fn required(&mut self, var: &EnvVarName) -> Option<String> {
        let value = self.env.var(var.as_str());
        if value.is_none() {
            self.missing.push(var.as_str().to_owned());
        }
        value
    }

    /// The variable's value, if it is named and set.
    pub fn optional(&self, var: Option<&EnvVarName>) -> Option<String> {
        var.and_then(|v| self.env.var(v.as_str()))
    }

    /// The CA bundle the variable names, if the variable is set.
    pub fn ca_bundle(&self, var: Option<&EnvVarName>) -> Option<CaBundle> {
        var.and_then(|var| {
            self.env.var(var.as_str()).map(|path| CaBundle {
                var: var.as_str().to_owned(),
                path,
            })
        })
    }

    /// `Err` naming every missing variable, if any.
    pub fn finish(self) -> Result<(), Error> {
        if self.missing.is_empty() {
            return Ok(());
        }
        let (noun, verb) = if self.missing.len() == 1 {
            ("variable", "is")
        } else {
            ("variables", "are")
        };
        Err(Error::new(
            ErrorCode::InvalidInput,
            format!(
                "environment {noun} {} {verb} not set",
                self.missing.join(", ")
            ),
        ))
    }
}

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
    // OpenAI-compatible servers put `type` and `message` under `error`; the
    // rest of the body is not kept.
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
        let body = r#"{"error":{"type":"overloaded_error","message":"Overloaded"},"request_id":"req_1","echo":"secret prompt"}"#;
        let error = status_error(
            "openai_compatible",
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
