//! The external HTTP API v0 (blueprint section 3.5, ADR-0018), as
//! `schemas/api/openapi.yaml` describes it.
//!
//! Commands are requests, not state edits. Starting a run goes to the
//! [`RunSubmitter`]; an approval's answer and a cancel are events appended
//! with the command authority bootstrap hands in (ADR-0032), which permits
//! those two events and nothing else. Reads go to the stores, to
//! [`RunReports`] and to [`SdrViews`]. The API never writes the SDR or the
//! job store itself.
//!
//! Every request needs the development token (ADR-0022) and gets a
//! correlation id, returned in `X-Correlation-Id` and in every error body.
//! Run events stream as server-sent events that resume from `Last-Event-ID`.

mod approvals;
mod artifacts;
mod error;
mod events;
mod reports;
mod request;
mod runs;
mod sdr;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{get, post};
use contracts::types::{Actor, ApiSettings};
use contracts::{
    ArtifactStore, Clock, Error, ErrorCode, EventLog, JobStore, RunReports, RunSubmitter,
    SchemaSet, SdrViews, WriteAuthority,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

pub use axum::Router;
pub use error::ApiError;

/// The development token every request must carry (ADR-0022). Its value is
/// never logged or printed.
#[derive(Clone)]
pub struct DevToken(String);

impl DevToken {
    /// InvalidInput if `token` is empty.
    pub fn new(token: String) -> Result<Self, Error> {
        if token.is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                "the development token is empty",
            ));
        }
        Ok(Self(token))
    }

    fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for DevToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DevToken(..)")
    }
}

/// How long requests may take and how large they may be, from
/// `config/api.yaml`.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Every request but an event stream.
    pub request_timeout: Duration,
    pub max_body_bytes: usize,
    /// How often an idle event stream sends a comment.
    pub keep_alive: Duration,
    /// How many events one read of the log returns at most.
    pub page_size: usize,
}

impl Limits {
    pub fn new(settings: &ApiSettings) -> Self {
        Self {
            request_timeout: Duration::from_millis(settings.request_timeout_ms.get()),
            max_body_bytes: usize::try_from(settings.max_body_bytes.get()).unwrap_or(usize::MAX),
            keep_alive: Duration::from_millis(settings.stream.keep_alive_ms.get()),
            page_size: usize::try_from(settings.stream.page_size.get()).unwrap_or(usize::MAX),
        }
    }
}

/// What the API serves from. Bootstrap builds it; tests give testkit's
/// stores and fakes.
pub struct Services {
    pub jobs: Arc<dyn JobStore>,
    pub events: Arc<dyn EventLog>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub submitter: Arc<dyn RunSubmitter>,
    pub reports: Arc<dyn RunReports>,
    /// Committed SDR versions, read through the SDR manager (ADR-0035).
    pub sdr: Arc<dyn SdrViews>,
    /// The authority of a client's command: it permits appending
    /// `approval_answered` and `cancel_requested` only.
    pub command: WriteAuthority,
    /// Who the token stands for, recorded as the actor of every command.
    pub actor: Actor,
    pub clock: Arc<dyn Clock>,
    pub token: DevToken,
    pub limits: Limits,
}

/// The schemas request bodies are checked against, relative to `schemas/`.
const REQUEST_SCHEMAS: [&str; 3] = [
    runs::RUN_REQUEST,
    runs::CANCEL_REQUEST,
    approvals::APPROVAL_ANSWER,
];

pub(crate) struct State {
    services: Services,
    schemas: SchemaSet,
    /// Ends every open event stream when the server shuts down.
    shutdown: CancellationToken,
    /// When the router was built, in ms since the epoch, and how many
    /// requests it has numbered: together, each request's correlation id.
    started_ms: i64,
    requests: AtomicU64,
}

/// The API's routes on `services`. Open event streams end when `shutdown`
/// is cancelled, so a graceful shutdown does not wait for them.
pub fn router(services: Services, shutdown: CancellationToken) -> Result<Router, Error> {
    let schemas = SchemaSet::compile(&REQUEST_SCHEMAS)?;
    let max_body = services.limits.max_body_bytes;
    let started_ms = services.clock.now().timestamp_millis();
    let state = Arc::new(State {
        services,
        schemas,
        shutdown,
        started_ms,
        requests: AtomicU64::new(0),
    });
    let bounded = Router::new()
        .route("/v0/runs", post(runs::create))
        .route("/v0/runs/{run_id}", get(runs::get))
        .route("/v0/runs/{run_id}/cancel", post(runs::cancel))
        .route(
            "/v0/runs/{run_id}/approvals/{approval_id}",
            post(approvals::answer),
        )
        .route("/v0/runs/{run_id}/report", get(reports::get))
        .route(
            "/v0/projects/{project_id}/sdr/versions/{version}",
            get(sdr::version),
        )
        .route("/v0/artifacts/{artifact_id}", get(artifacts::metadata))
        .route(
            "/v0/artifacts/{artifact_id}/content",
            get(artifacts::content),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            request::bounded,
        ));
    let streams = Router::new().route("/v0/runs/{run_id}/events", get(events::stream));
    Ok(bounded
        .merge(streams)
        .fallback(request::no_such_path)
        .method_not_allowed_fallback(request::no_such_method)
        .layer(DefaultBodyLimit::max(max_body))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            request::authenticate,
        ))
        .with_state(state))
}

/// Serves `router` on `listener` until `shutdown`, then lets the requests in
/// flight finish.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
}
