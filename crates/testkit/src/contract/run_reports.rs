use std::sync::Arc;

use contracts::ids::RunId;
use contracts::{ErrorCode, RunReports, SchemaSet};

use super::{assert_code, ctx};
use crate::builders::id;

const REPORT: &str = "reports/run-report.schema.json";

/// What the run-reports suite tests: runs its implementation already holds.
pub struct ReportsWorld {
    pub reports: Arc<dyn RunReports>,
    /// A run that is queued, running or waiting.
    pub not_stopped: RunId,
    /// A run that stopped for good.
    pub stopped: RunId,
}

/// The `RunReports` contract.
pub async fn run_reports(world: ReportsWorld) {
    assert_code(
        world.reports.report(&id("run-unknown"), &ctx()).await,
        ErrorCode::NotFound,
        "run reports: an unknown run",
    );
    assert_code(
        world.reports.report(&world.not_stopped, &ctx()).await,
        ErrorCode::Conflict,
        "run reports: a run that has not stopped",
    );
    let report = world
        .reports
        .report(&world.stopped, &ctx())
        .await
        .unwrap_or_else(|e| panic!("run reports: a stopped run has no report: {e}"));
    assert_eq!(
        report.run_id, world.stopped,
        "run reports: the run's own report"
    );
    assert!(
        report.final_,
        "run reports: a run stopped for good is final"
    );
    let schemas = SchemaSet::compile(&[REPORT]).expect("the report schema compiles");
    let problems = schemas.errors_of(REPORT, &report).expect("validate");
    assert!(
        problems.is_empty(),
        "run reports: the report breaks its schema: {problems:?}"
    );
}
