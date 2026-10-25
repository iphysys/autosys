//! Migrations that rewrite stored data. Needs `DATABASE_URL` (see README,
//! section Database).

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use chrono::Utc;
use contracts::JobStore;
use contracts::types::JobStatus;
use infra::persistence::PgJobStore;
use serde_json::{Value, json};
use support::TestDb;
use testkit::builders::{id, job};

/// The last migration before job schema 0.2.
const BEFORE_JOB_STATUS: i64 = 3;
const JOB_STATUS: i64 = 4;

/// Stores `job_id` as job schema 0.1 did: the state at the top of the
/// document and in its own column, with the wake condition beside it.
async fn insert_v01(db: &TestDb, job_id: &str, state: &str, wake: Option<Value>) {
    let mut document = serde_json::to_value(job(job_id, "run-1", Utc::now())).expect("serializes");
    let fields = document.as_object_mut().expect("a job is an object");
    fields.remove("status");
    fields.insert("schema_version".to_owned(), json!("0.1"));
    fields.insert("state".to_owned(), json!(state));
    if let Some(wake) = &wake {
        fields.insert("wake_condition".to_owned(), wake.clone());
        fields.insert("stop_reason".to_owned(), json!("approval_required"));
    }
    let running = state == "running";
    sqlx::query(
        "INSERT INTO jobs (id, run_id, created_at, document, state, attempt, updated_at, \
             wake_condition, lease_holder, lease_acquired_at, lease_expires_at, lease_heartbeat_at) \
         VALUES ($1, 'run-1', now(), $2, $3, 1, now(), $4, \
             CASE WHEN $5 THEN 'worker-1' END, CASE WHEN $5 THEN now() END, \
             CASE WHEN $5 THEN now() + interval '1 hour' END, CASE WHEN $5 THEN now() END)",
    )
    .bind(job_id)
    .bind(&document)
    .bind(state)
    .bind(wake)
    .bind(running)
    .execute(db.pool())
    .await
    .expect("insert a schema 0.1 job");
}

#[tokio::test]
async fn job_status_moves_queued_and_running_jobs_into_the_status() {
    let db = TestDb::create_at(BEFORE_JOB_STATUS).await;
    insert_v01(&db, "job-queued", "queued", None).await;
    insert_v01(&db, "job-running", "running", None).await;
    db.migrate_to(JOB_STATUS)
        .await
        .expect("migration 0004 runs");

    let store = PgJobStore::new(db.database());
    let queued = store
        .get(&id("job-queued"))
        .await
        .expect("get the queued job");
    assert_eq!(queued.status, JobStatus::Queued { checkpoint: None });
    let running = store
        .get(&id("job-running"))
        .await
        .expect("get the running job");
    assert_eq!(running.status, JobStatus::Running);
    assert_eq!(
        running
            .lease
            .map(|l| l.holder.as_str().to_owned())
            .as_deref(),
        Some("worker-1"),
        "the lease survives"
    );

    let rows: Vec<(String, String, Value)> =
        sqlx::query_as("SELECT id, state, document FROM jobs ORDER BY id")
            .fetch_all(db.pool())
            .await
            .expect("read the rows");
    for (job_id, state, document) in rows {
        assert_eq!(document["schema_version"], "0.2", "{job_id}");
        assert_eq!(
            document["status"]["state"], state,
            "{job_id}: the state column"
        );
        for gone in ["state", "wake_condition", "stop_reason"] {
            assert!(
                document.get(gone).is_none(),
                "{job_id}: {gone} is still there"
            );
        }
    }
    db.drop().await;
}

#[tokio::test]
async fn job_status_refuses_a_parked_job_it_cannot_give_a_checkpoint() {
    let db = TestDb::create_at(BEFORE_JOB_STATUS).await;
    let wake = json!({ "kind": "approval_answered", "approval_id": "appr-1" });
    insert_v01(&db, "job-suspended", "suspended", Some(wake)).await;
    let error = db
        .migrate_to(JOB_STATUS)
        .await
        .expect_err("a suspended job has no checkpoint");
    assert!(
        error.to_string().contains("cannot move to job schema 0.2"),
        "{error}"
    );
    db.drop().await;
}
