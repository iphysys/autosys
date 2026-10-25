//! `GET /v0/runs/{run_id}/report`.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State as Shared};
use contracts::ids::RunId;
use contracts::types::RunReport;

use crate::State;
use crate::error::ApiError;
use crate::request::{Call, parse_id};

/// The run report: 404 for an unknown run, 409 until the run stops.
pub(crate) async fn get(
    Shared(state): Shared<Arc<State>>,
    Call(ctx): Call,
    Path(run_id): Path<String>,
) -> Result<Json<RunReport>, ApiError> {
    let run: RunId = parse_id(&run_id, "run id", &ctx)?;
    state
        .services
        .reports
        .report(&run, &ctx)
        .await
        .map(Json)
        .map_err(|e| ApiError::from_error(&e, ctx.correlation_id()))
}
