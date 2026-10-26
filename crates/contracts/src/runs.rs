//! What the external API asks of the harness (blueprint section 3.5,
//! ADR-0018): starting a run, reading its report, and reading a committed SDR
//! version (ADR-0035). `synth-runtime` implements all three. `synth-api` reaches the harness only through these and
//! the store traits, so its contract tests run on testkit's fakes.

use async_trait::async_trait;
use schema_types::{Actor, Job, RunReport, RunRequest, SdrVersionView};

use crate::context::ExecutionContext;
use crate::error::Error;
use crate::ids::{Identifier, ProjectId, RunId, SdrVersion};

/// What [`RunSubmitter::submit`] did.
#[derive(Clone, Debug, PartialEq)]
pub struct Submitted {
    /// The run's job, as stored.
    pub job: Job,
    /// False when an earlier submit with the same key made the run.
    pub created: bool,
}

/// Starts runs a client asks for.
#[async_trait]
pub trait RunSubmitter: Send + Sync {
    /// Starts the run `request` describes: a new project whose SDR version 0
    /// holds the requirements, and a queued job of the named agent, recorded
    /// as submitted. `actor` is who asked, recorded with version 0.
    ///
    /// The ids come from `key`, so submitting again with the same key and
    /// request returns the same run with `created: false`, and finishes a
    /// submit that stopped partway. InvalidInput if the request breaks its
    /// schema, names an agent that is not registered, or asks more than the
    /// agent allows; Conflict if `key` was used for a different request.
    async fn submit(
        &self,
        key: &Identifier,
        request: &RunRequest,
        actor: &Actor,
        ctx: &ExecutionContext,
    ) -> Result<Submitted, Error>;
}

/// Serves run reports (ADR-0033).
#[async_trait]
pub trait RunReports: Send + Sync {
    /// The report of `run_id`. NotFound if there is no such run; Conflict
    /// while it is queued, running or waiting, since it has no report yet.
    async fn report(&self, run_id: &RunId, ctx: &ExecutionContext) -> Result<RunReport, Error>;
}

/// Serves committed SDR versions to clients (ADR-0035): reads only, through
/// the SDR manager, never its storage.
#[async_trait]
pub trait SdrViews: Send + Sync {
    /// The document at `version`, its record, and the evidence the record
    /// cites, in the record's order. NotFound for an unknown project or
    /// version.
    async fn version(
        &self,
        project_id: &ProjectId,
        version: &SdrVersion,
        ctx: &ExecutionContext,
    ) -> Result<SdrVersionView, Error>;
}
