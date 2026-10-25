//! `GET /v0/artifacts/{artifact_id}` and its `/content`.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State as Shared};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use contracts::ids::ArtifactId;
use contracts::types::ArtifactMetadata;

use crate::State;
use crate::error::ApiError;
use crate::request::{Call, parse_id};

pub(crate) async fn metadata(
    Shared(state): Shared<Arc<State>>,
    Call(ctx): Call,
    Path(artifact_id): Path<String>,
) -> Result<Json<ArtifactMetadata>, ApiError> {
    let id: ArtifactId = parse_id(&artifact_id, "artifact id", &ctx)?;
    state
        .services
        .artifacts
        .metadata(&id)
        .await
        .map(Json)
        .map_err(|e| ApiError::from_error(&e, ctx.correlation_id()))
}

/// The bytes, with the media type the metadata names. Browsers must not
/// guess another type.
pub(crate) async fn content(
    Shared(state): Shared<Arc<State>>,
    Call(ctx): Call,
    Path(artifact_id): Path<String>,
) -> Result<Response, ApiError> {
    let id: ArtifactId = parse_id(&artifact_id, "artifact id", &ctx)?;
    let store = &state.services.artifacts;
    let failed = |e: contracts::Error| ApiError::from_error(&e, ctx.correlation_id());
    let metadata = store.metadata(&id).await.map_err(failed)?;
    let bytes = store.content(&id).await.map_err(failed)?;
    let media_type = HeaderValue::from_str(metadata.media_type.as_str())
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    Ok((
        [
            (header::CONTENT_TYPE, media_type),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        bytes,
    )
        .into_response())
}
