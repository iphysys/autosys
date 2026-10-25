//! What every request goes through: a correlation id, the development
//! token, the time limit, and bodies and ids checked before a handler sees
//! them.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use contracts::ids::{CorrelationId, RunId};
use contracts::types::{ApprovalAnswerRequest, CancelRequest, Event, Job, JobState, RunRequest};
use contracts::{Error, ErrorCode, EventKind, EventLog, ExecutionContext, JobStore, job_state};
use serde::de::DeserializeOwned;
use subtle::ConstantTimeEq;

use crate::State;
use crate::error::ApiError;

/// The header that carries the request's correlation id back.
pub(crate) const CORRELATION_HEADER: &str = "x-correlation-id";

/// Numbers the request, checks its token, and logs its outcome. Every
/// route and the fallbacks are behind it.
pub(crate) async fn authenticate(
    axum::extract::State(state): axum::extract::State<Arc<State>>,
    mut request: Request,
    next: Next,
) -> Response {
    let n = state.requests.fetch_add(1, Ordering::Relaxed);
    let correlation_id: CorrelationId = match format!("corr-api-{}-{n}", state.started_ms).parse() {
        Ok(id) => id,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let mut response = if authorized(request.headers(), &state) {
        request.extensions_mut().insert(correlation_id.clone());
        next.run(request).await
    } else {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::PermissionDenied,
            "a valid development token is required: Authorization: Bearer <token>",
            &correlation_id,
        )
        .into_response()
    };
    if let Ok(value) = HeaderValue::from_str(correlation_id.as_str()) {
        response.headers_mut().insert(CORRELATION_HEADER, value);
    }
    tracing::info!(
        correlation_id = correlation_id.as_str(),
        %method,
        path,
        status = response.status().as_u16(),
        "request"
    );
    response
}

/// Whether the request carries the token, compared in constant time.
fn authorized(headers: &HeaderMap, state: &State) -> bool {
    let Some(given) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "))
    else {
        return false;
    };
    given.ct_eq(state.services.token.as_bytes()).into()
}

/// Bounds a request by the configured time limit. Event streams are not
/// behind it: they stay open while their run can change.
pub(crate) async fn bounded(
    axum::extract::State(state): axum::extract::State<Arc<State>>,
    Call(ctx): Call,
    request: Request,
    next: Next,
) -> Response {
    let limit = state.services.limits.request_timeout;
    match tokio::time::timeout(limit, next.run(request)).await {
        Ok(response) => response,
        Err(_) => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Timeout,
            format!("the request took longer than {} ms", limit.as_millis()),
            ctx.correlation_id(),
        )
        .into_response(),
    }
}

pub(crate) async fn no_such_path(Call(ctx): Call) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        ErrorCode::NotFound,
        "no such path",
        ctx.correlation_id(),
    )
}

pub(crate) async fn no_such_method(Call(ctx): Call) -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        ErrorCode::InvalidInput,
        "this path does not take that method",
        ctx.correlation_id(),
    )
}

/// The request's context: its correlation id, and no write authority.
pub(crate) struct Call(pub(crate) ExecutionContext);

impl<S: Send + Sync> FromRequestParts<S> for Call {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        match parts.extensions.get::<CorrelationId>() {
            Some(id) => Ok(Call(ExecutionContext::new(id.clone()))),
            None => Err(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        }
    }
}

/// A request body type and the schema it must satisfy, relative to
/// `schemas/`.
pub(crate) trait Checked: DeserializeOwned {
    const SCHEMA: &'static str;
}

impl Checked for RunRequest {
    const SCHEMA: &'static str = crate::runs::RUN_REQUEST;
}

impl Checked for CancelRequest {
    const SCHEMA: &'static str = crate::runs::CANCEL_REQUEST;
}

impl Checked for ApprovalAnswerRequest {
    const SCHEMA: &'static str = crate::approvals::APPROVAL_ANSWER;
}

/// A JSON body checked against its schema: 415 unless it is
/// `application/json`, 413 if it is too large, 400 if it is not JSON or
/// breaks the schema, listing every problem.
pub(crate) struct Body<T>(pub(crate) T);

impl<T: Checked> FromRequest<Arc<State>> for Body<T> {
    type Rejection = Response;

    async fn from_request(request: Request, state: &Arc<State>) -> Result<Self, Self::Rejection> {
        // The middleware gives every request its id before any handler runs.
        let Some(correlation_id) = request.extensions().get::<CorrelationId>().cloned() else {
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        };
        read_body(request, state, &correlation_id)
            .await
            .map(Body)
            .map_err(IntoResponse::into_response)
    }
}

async fn read_body<T: Checked>(
    request: Request,
    state: &Arc<State>,
    correlation_id: &CorrelationId,
) -> Result<T, ApiError> {
    let json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
    if !json {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::InvalidInput,
            "the body must be application/json",
            correlation_id,
        ));
    }
    let bytes = Bytes::from_request(request, state).await.map_err(|e| {
        let status = e.status();
        if status == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                status,
                ErrorCode::InvalidInput,
                "the body is larger than the server accepts",
                correlation_id,
            )
        } else {
            ApiError::invalid(format!("the body cannot be read: {e}"), correlation_id)
        }
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::invalid(format!("the body is not JSON: {e}"), correlation_id))?;
    let problems = state
        .schemas
        .errors(T::SCHEMA, &value)
        .map_err(|e| ApiError::from_error(&e, correlation_id))?;
    if !problems.is_empty() {
        return Err(ApiError::invalid(
            format!(
                "the body breaks schemas/{}: {}",
                T::SCHEMA,
                problems.join("; ")
            ),
            correlation_id,
        ));
    }
    serde_json::from_value(value).map_err(|e| {
        ApiError::invalid(
            format!("the body does not match its type: {e}"),
            correlation_id,
        )
    })
}

/// A path or header value parsed as an id: 400, naming `what`, if it breaks
/// the id's pattern.
pub(crate) fn parse_id<T: FromStr>(
    value: &str,
    what: &str,
    ctx: &ExecutionContext,
) -> Result<T, ApiError> {
    value.parse().map_err(|_| {
        ApiError::invalid(
            format!("{what} `{value}` is not a valid identifier"),
            ctx.correlation_id(),
        )
    })
}

/// The run's job; 404 if there is none.
pub(crate) async fn job_of(
    jobs: &dyn JobStore,
    run: &RunId,
    ctx: &ExecutionContext,
) -> Result<Job, ApiError> {
    match jobs.find_by_run(run).await {
        Ok(Some(job)) => Ok(job),
        Ok(None) => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            format!("run {} does not exist", run.as_str()),
            ctx.correlation_id(),
        )),
        Err(e) => Err(ApiError::from_error(&e, ctx.correlation_id())),
    }
}

/// Whether the job has stopped for good, so nothing more happens to it.
pub(crate) fn is_final(job: &Job) -> bool {
    matches!(
        job_state(&job.status),
        JobState::Completed | JobState::Cancelled | JobState::Failed
    )
}

/// Every event of the given kinds in `run`, in order.
pub(crate) async fn read_all(
    events: &dyn EventLog,
    run: &RunId,
    kinds: &[EventKind],
    page: usize,
) -> Result<Vec<Event>, Error> {
    let mut all = Vec::new();
    let mut after = 0;
    loop {
        let found = events.read_kinds(run, after, kinds, page.max(1)).await?;
        let Some(last) = found.last() else {
            return Ok(all);
        };
        after = last.seq.get();
        all.extend(found);
    }
}

/// 409: the run has stopped for good.
pub(crate) fn stopped(run: &RunId, ctx: &ExecutionContext) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        ErrorCode::Conflict,
        format!("run {} has stopped for good", run.as_str()),
        ctx.correlation_id(),
    )
}
