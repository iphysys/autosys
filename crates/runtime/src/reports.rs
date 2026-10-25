//! Run reports for clients (ADR-0033): the assembler of `synth-artifacts`
//! behind the contract the API calls.

use artifacts::ReportAssembler;
use async_trait::async_trait;
use contracts::ids::RunId;
use contracts::types::RunReport;
use contracts::{Error, ExecutionContext, RunReports};

/// Serves each run's report, assembled on demand from durable records.
pub struct Reports {
    assembler: ReportAssembler,
}

impl Reports {
    pub fn new(assembler: ReportAssembler) -> Self {
        Self { assembler }
    }
}

#[async_trait]
impl RunReports for Reports {
    /// An unknown run is NotFound and a run that has not stopped is
    /// Conflict, as `ArtifactsError` maps them.
    async fn report(&self, run_id: &RunId, ctx: &ExecutionContext) -> Result<RunReport, Error> {
        self.assembler
            .assemble(run_id, ctx)
            .await
            .map_err(|e| e.into_error(ctx))
    }
}
