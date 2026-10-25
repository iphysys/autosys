//! The run report of a fixture run on Postgres (ADR-0033): it validates,
//! every requirement status links to stored evidence with the same verdict,
//! and its numbers are the job's and the log's.

use std::sync::Arc;
use std::time::Duration;

use artifacts::{ReportAssembler, ReportSources};
use bootstrap::Worker;
use contracts::ids::RunId;
use contracts::types::{EvidenceBody, JobState, RequirementStatus, RunReport, StopReason};
use contracts::{ArtifactStore, ExecutionContext, SchemaSet, provenance};
use infra::persistence::{Database, PgArtifactStore, PoolSettings};
use testkit::builders::id;

use crate::support::db::TestDb;
use crate::support::*;

use JobState::{Completed, Suspended};

async fn assembler(worker: &Worker, db: &TestDb, volume: &tempfile::TempDir) -> ReportAssembler {
    let database = Database::connect(
        db.url(),
        &PoolSettings {
            max_connections: 2,
            acquire_timeout: Duration::from_secs(5),
        },
        Duration::from_secs(5),
    )
    .await
    .expect("connect for the artifact store");
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(PgArtifactStore::new(database, volume.path()));
    ReportAssembler::new(
        ReportSources {
            jobs: worker.jobs.clone(),
            events: worker.events.clone(),
            evidence: worker.evidence.clone(),
            artifacts,
            sdr: worker.sdr.clone(),
        },
        Duration::from_secs(5),
    )
    .expect("the report schema compiles")
}

async fn report(assembler: &ReportAssembler) -> RunReport {
    assembler
        .assemble(
            &id::<RunId>("run-1"),
            &ExecutionContext::new(id("corr-report")),
        )
        .await
        .expect("a report")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_report_of_a_fixture_run_validates_and_links_every_status_to_evidence() {
    let db = TestDb::create().await;
    // The default autonomy policy asks before the final commit.
    let config = TestConfig::new(&[]);
    let worker = build("worker-a", &db, &config, Parts::policy()).await;
    seed(&worker).await;
    worker
        .scheduler
        .submit(&job("job-1", "run-1"), None)
        .await
        .expect("submit");
    let volume = tempfile::tempdir().expect("an artifact volume");
    let assembler = assembler(&worker, &db, &volume).await;

    let running = Background::start(Arc::clone(&worker));
    let suspended = wait_for(&worker, "job-1", Suspended).await;
    let waiting = report(&assembler).await;
    assert!(!waiting.final_, "a suspended run can still continue");
    assert_eq!(waiting.stop_reason, StopReason::ApprovalRequired);
    approve(&worker, &suspended).await;
    let job = wait_for(&worker, "job-1", Completed).await;
    running.stop().await;

    let report = report(&assembler).await;
    let schemas = SchemaSet::compile(&["reports/run-report.schema.json"]).expect("schema");
    assert_eq!(
        schemas
            .errors_of("reports/run-report.schema.json", &report)
            .expect("validates"),
        Vec::<String>::new()
    );
    assert!(report.final_);
    assert_eq!(report.stop_reason, StopReason::Success);
    assert_eq!(report.final_sdr_version.0, head(&worker).await);

    // Every status links to evidence: stored, about the requirement, with
    // the verdict the row shows.
    assert!(!report.requirements.is_empty());
    for row in &report.requirements {
        assert_eq!(row.status, RequirementStatus::Pass, "{row:?}");
        assert!(row.evidence.iter().any(|f| f.used), "{row:?}");
        for finding in &row.evidence {
            let stored = worker
                .evidence
                .get(&finding.evidence_id)
                .await
                .expect("the linked evidence is stored");
            assert!(stored.subject_ids.contains(&row.requirement_id));
            let EvidenceBody::Check { verdict, .. } = stored.body else {
                panic!("{} is not a check", finding.evidence_id.as_str());
            };
            assert_eq!(finding.verdict, verdict);
        }
    }

    // The numbers are the job's and the log's.
    assert_eq!(report.budget_used.len(), 1);
    assert_eq!(report.budget_used[0].usage, job.budget_used);
    let events = logged(&worker, "run-1").await;
    assert_eq!(report.versions, provenance::fold(&events));
    assert_eq!(
        report.versions.harness_builds.len(),
        2,
        "one build per segment: before and after the approval"
    );
    assert!(!report.versions.prompts.is_empty());
    assert_eq!(report.versions.agent_specs.len(), 1);
    assert!(!report.versions.model_ids.is_empty());
    assert_eq!(report.approvals.len(), 1);
    assert!(
        report.artifacts.is_empty(),
        "no tool produces artifacts yet"
    );

    drop(assembler);
    drop(worker);
    db.drop().await;
}
