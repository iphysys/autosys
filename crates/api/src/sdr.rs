//! `GET /v0/projects/{project_id}/sdr/versions/{version}` (ADR-0035).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State as Shared};
use contracts::ids::{ProjectId, SdrVersion};
use contracts::types::SdrVersionView;

use crate::State;
use crate::error::ApiError;
use crate::request::{Call, parse_id};

/// A committed version with its record and evidence: 404 for an unknown
/// project or version. Read only: the API never writes the SDR.
pub(crate) async fn version(
    Shared(state): Shared<Arc<State>>,
    Call(ctx): Call,
    Path((project_id, version)): Path<(String, String)>,
) -> Result<Json<SdrVersionView>, ApiError> {
    let project: ProjectId = parse_id(&project_id, "project id", &ctx)?;
    let version = version.parse::<u64>().map(SdrVersion).map_err(|_| {
        ApiError::invalid(
            format!("version `{version}` is not an SDR version"),
            ctx.correlation_id(),
        )
    })?;
    state
        .services
        .sdr
        .version(&project, &version, &ctx)
        .await
        .map(Json)
        .map_err(|e| ApiError::from_error(&e, ctx.correlation_id()))
}
