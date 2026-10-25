//! `POST /v0/runs`, `GET /v0/runs/{run_id}` and `POST
//! /v0/runs/{run_id}/cancel`.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State as Shared};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use contracts::ids::{Identifier, RunId};
use contracts::types::{
    CancelRequest, Event, EventPayload, Job, Run, RunRequest, RunSchemaVersion, Timestamp,
};
use contracts::{ExecutionContext, NewEvent};

use crate::State;
use crate::error::ApiError;
use crate::request::{Body, Call, is_final, job_of, parse_id, stopped};

pub(crate) const RUN_REQUEST: &str = "api/run-request.schema.json";
pub(crate) const CANCEL_REQUEST: &str = "api/cancel.schema.json";

const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// Starts a run: 201 with the new run, or 200 with the run an earlier
/// request with the same key started.
pub(crate) async fn create(
    Shared(state): Shared<Arc<State>>,
    Call(ctx): Call,
    headers: HeaderMap,
    Body(request): Body<RunRequest>,
) -> Result<Response, ApiError> {
    let key: Identifier = match headers.get(IDEMPOTENCY_KEY).map(|v| v.to_str()) {
        Some(Ok(key)) => parse_id(key, "the Idempotency-Key", &ctx)?,
        Some(Err(_)) => {
            return Err(ApiError::invalid(
                "the Idempotency-Key is not visible ASCII",
                ctx.correlation_id(),
            ));
        }
        None => {
            return Err(ApiError::invalid(
                "the Idempotency-Key header is required",
                ctx.correlation_id(),
            ));
        }
    };
    let s = &state.services;
    let submitted = s
        .submitter
        .submit(&key, &request, &s.actor, &ctx)
        .await
        .map_err(|e| ApiError::from_error(&e, ctx.correlation_id()))?;
    let run = view(&state, &submitted.job, &ctx).await?;
    let status = if submitted.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let mut response = (status, Json(run)).into_response();
    if let Ok(location) =
        HeaderValue::from_str(&format!("/v0/runs/{}", submitted.job.run_id.as_str()))
    {
        response.headers_mut().insert(header::LOCATION, location);
    }
    Ok(response)
}

pub(crate) async fn get(
    Shared(state): Shared<Arc<State>>,
    Call(ctx): Call,
    Path(run_id): Path<String>,
) -> Result<Json<Run>, ApiError> {
    let run: RunId = parse_id(&run_id, "run id", &ctx)?;
    let job = job_of(state.services.jobs.as_ref(), &run, &ctx).await?;
    Ok(Json(view(&state, &job, &ctx).await?))
}

/// Asks the run to stop: 202 with the stored `cancel_requested`, or 409 if
/// it already stopped for good.
pub(crate) async fn cancel(
    Shared(state): Shared<Arc<State>>,
    Call(ctx): Call,
    Path(run_id): Path<String>,
    Body(request): Body<CancelRequest>,
) -> Result<(StatusCode, Json<Event>), ApiError> {
    let run: RunId = parse_id(&run_id, "run id", &ctx)?;
    let job = job_of(state.services.jobs.as_ref(), &run, &ctx).await?;
    if is_final(&job) {
        return Err(stopped(&run, &ctx));
    }
    let event = command(
        &state,
        &run,
        EventPayload::CancelRequested {
            reason: request.reason,
        },
        &ctx,
    )
    .await?;
    tracing::info!(run = run.as_str(), "cancel requested");
    Ok((StatusCode::ACCEPTED, Json(event)))
}

/// The run as a client sees it.
async fn view(state: &State, job: &Job, ctx: &ExecutionContext) -> Result<Run, ApiError> {
    let last_seq = state
        .services
        .events
        .last_seq(&job.run_id)
        .await
        .map_err(|e| ApiError::from_error(&e, ctx.correlation_id()))?;
    Ok(Run {
        schema_version: RunSchemaVersion::X01,
        run_id: job.run_id.clone(),
        project_id: job.project_id.clone(),
        status: job.status.clone(),
        created_at: job.created_at.clone(),
        updated_at: job.updated_at.clone(),
        last_seq,
    })
}

/// Appends a client's command to the run's log, with the command authority
/// and the token's actor.
pub(crate) async fn command(
    state: &State,
    run: &RunId,
    payload: EventPayload,
    ctx: &ExecutionContext,
) -> Result<Event, ApiError> {
    let s = &state.services;
    s.events
        .append(
            NewEvent {
                run_id: run.clone(),
                occurred_at: Timestamp(s.clock.now()),
                correlation_id: ctx.correlation_id().clone(),
                actor: s.actor.clone(),
                payload,
            },
            Some(&s.command),
        )
        .await
        .map_err(|e| ApiError::from_error(&e, ctx.correlation_id()))
}
