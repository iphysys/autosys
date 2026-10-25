//! `POST /v0/runs/{run_id}/approvals/{approval_id}`.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State as Shared};
use axum::http::StatusCode;
use contracts::ids::{ApprovalId, RunId};
use contracts::types::{ApprovalAnswerRequest, Event, EventPayload};
use contracts::{ErrorCode, EventKind};

use crate::State;
use crate::error::ApiError;
use crate::request::{Body, Call, is_final, job_of, parse_id, read_all, stopped};
use crate::runs::command;

pub(crate) const APPROVAL_ANSWER: &str = "api/approval-answer.schema.json";

/// Records a human's answer: 202 with the stored `approval_answered`; 404
/// unless the run asked for this approval; 409 if it was answered already
/// or the run stopped for good. Two answers sent at once can both be
/// stored; the run uses the earliest.
pub(crate) async fn answer(
    Shared(state): Shared<Arc<State>>,
    Call(ctx): Call,
    Path((run_id, approval_id)): Path<(String, String)>,
    Body(request): Body<ApprovalAnswerRequest>,
) -> Result<(StatusCode, Json<Event>), ApiError> {
    let run: RunId = parse_id(&run_id, "run id", &ctx)?;
    let approval: ApprovalId = parse_id(&approval_id, "approval id", &ctx)?;
    let s = &state.services;
    let job = job_of(s.jobs.as_ref(), &run, &ctx).await?;
    if is_final(&job) {
        return Err(stopped(&run, &ctx));
    }
    let log = read_all(
        s.events.as_ref(),
        &run,
        &[EventKind::APPROVAL_REQUESTED, EventKind::APPROVAL_ANSWERED],
        s.limits.page_size,
    )
    .await
    .map_err(|e| ApiError::from_error(&e, ctx.correlation_id()))?;
    let asked = log.iter().any(|e| {
        matches!(&e.payload, EventPayload::ApprovalRequested { approval_id, .. } if *approval_id == approval)
    });
    if !asked {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            format!(
                "run {} did not ask for approval {}",
                run.as_str(),
                approval.as_str()
            ),
            ctx.correlation_id(),
        ));
    }
    let answered = log.iter().any(|e| {
        matches!(&e.payload, EventPayload::ApprovalAnswered { approval_id, .. } if *approval_id == approval)
    });
    if answered {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            ErrorCode::Conflict,
            format!("approval {} was answered already", approval.as_str()),
            ctx.correlation_id(),
        ));
    }
    let event = command(
        &state,
        &run,
        EventPayload::ApprovalAnswered {
            approval_id: approval.clone(),
            answer: request.answer,
            comment: request.comment,
        },
        &ctx,
    )
    .await?;
    tracing::info!(
        run = run.as_str(),
        approval = approval.as_str(),
        "approval answered"
    );
    Ok((StatusCode::ACCEPTED, Json(event)))
}
