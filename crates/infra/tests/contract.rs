//! The testkit contract suites, unchanged, against the Postgres stores: PR 4's
//! first done-when test. Needs `DATABASE_URL` (see README, section Database).

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};

use infra::persistence::{
    PgArtifactStore, PgEventLog, PgEvidenceStore, PgJobStore, PgSdrRepository, PgStepStore,
};
use support::TestDb;
use testkit::contract;

/// The suite drives time with a `ManualClock`, while `PgJobStore` reads time
/// only from the database. So this test database's clock follows the suite's
/// clock: every connection handed to the store carries the clock's current
/// time, and `autosys_now()` returns it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_job_store() {
    let db = TestDb::create().await;
    db.use_session_clock().await;
    contract::job_store(|clock| {
        let db = &db;
        async move {
            db.empty().await;
            PgJobStore::new(db.following(clock))
        }
    })
    .await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_event_log() {
    let db = TestDb::create().await;
    contract::event_log(|| async {
        db.empty().await;
        PgEventLog::connect(db.database())
            .await
            .expect("cannot start the event listener")
    })
    .await;
    db.drop().await;
}

/// Appends are fenced by a job the suite claims, so the repository and its
/// job store share the database, whose clock follows the suite's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_sdr_repository() {
    let db = TestDb::create().await;
    db.use_session_clock().await;
    contract::sdr_repository(|clock| {
        let db = &db;
        async move {
            db.empty().await;
            let database = db.following(clock);
            (
                PgSdrRepository::new(database.clone()),
                PgJobStore::new(database),
            )
        }
    })
    .await;
    db.drop().await;
}

/// Like the job store: `PgStepStore` reads time only from the database, so
/// the database clock follows the suite's clock. Its job store, which fences
/// the writes, shares the database.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_step_store() {
    let db = TestDb::create().await;
    db.use_session_clock().await;
    contract::step_store(|clock| {
        let db = &db;
        async move {
            db.empty().await;
            let database = db.following(clock);
            (
                PgStepStore::new(database.clone()),
                PgJobStore::new(database),
            )
        }
    })
    .await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_evidence_store() {
    let db = TestDb::create().await;
    contract::evidence_store(|| async {
        db.empty().await;
        PgEvidenceStore::new(db.database())
    })
    .await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_artifact_store() {
    let db = TestDb::create().await;
    let volume = tempfile::tempdir().expect("cannot create a temporary directory");
    let cases = AtomicUsize::new(0);
    contract::artifact_store(|| async {
        db.empty().await;
        let case = cases.fetch_add(1, Ordering::Relaxed);
        PgArtifactStore::new(db.database(), volume.path().join(format!("case-{case}")))
    })
    .await;
    db.drop().await;
}
