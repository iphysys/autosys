//! A worker that bootstrap builds runs a job end to end, and startup stops
//! on a configuration it must not run with.

use std::collections::HashMap;
use std::sync::Arc;

use bootstrap::{BootstrapError, Overrides, WorkerSetup, build_worker};
use contracts::types::{JobState, JobStatus};

use crate::support::db::TestDb;
use crate::support::*;

use JobState::{Completed, Queued, Running};

#[tokio::test(flavor = "multi_thread")]
async fn a_built_worker_runs_a_submitted_job_to_completion() {
    let db = TestDb::create().await;
    let config = TestConfig::new(&[("policies/autonomy.yaml", NO_APPROVALS_YAML)]);
    let parts = Parts::new(succeeding());
    let model = parts.model.clone();
    let worker = build("worker-a", &db, &config, parts).await;
    seed(&worker).await;
    worker
        .scheduler
        .submit(&job("job-1", "run-1"), None)
        .await
        .expect("submit");

    let running = Background::start(Arc::clone(&worker));
    let job = wait_for(&worker, "job-1", Completed).await;
    running.stop().await;

    assert!(
        matches!(&job.status, JobStatus::Completed { stop_reason, .. } if stop_reason.to_string() == "success"),
        "{:?}",
        job.status
    );
    assert_eq!(job.lease, None);
    assert_eq!(model.remaining(), 0, "every scripted call was made");
    assert_eq!(
        chain(&worker, "run-1").await,
        [
            (None, Queued),
            (Some(Queued), Running),
            (Some(Running), Completed)
        ]
    );
    assert_eq!(head(&worker).await, 2, "the final proposal committed");
    drop(worker);
    db.drop().await;
}

fn setup(config: &TestConfig, overrides: Overrides) -> WorkerSetup {
    WorkerSetup {
        config_dir: config.path().to_owned(),
        prompts_dir: repo_root().join("prompts"),
        worker_id: Some("worker-a".to_owned()),
        overrides,
    }
}

/// Fake tool servers, so startup gets past connecting to them.
fn fake_tools() -> Overrides {
    let parts = Parts::new(Vec::new());
    Overrides {
        tool_servers: Some(build123d(parts.create_plate, parts.measure)),
        ..Overrides::default()
    }
}

/// The startup check that refused `setup`.
async fn refused(setup: WorkerSetup, vars: &HashMap<String, String>) -> (&'static str, String) {
    match build_worker(setup, vars).await {
        Ok(_) => panic!("the worker started"),
        Err(BootstrapError::Startup { check, message, .. }) => (check, message),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_intervals_do_not_start_outside_the_test_environment() {
    let db = TestDb::create().await;
    let config = TestConfig::new(&[]);
    let mut vars = vars(&db);
    vars.remove("SYNTH_ENVIRONMENT");
    let (check, message) = refused(setup(&config, fake_tools()), &vars).await;
    assert_eq!(check, "config", "{message}");
    assert!(message.contains("production"), "{message}");
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_tool_server_stops_startup() {
    let db = TestDb::create().await;
    let config = TestConfig::new(&[]);
    let (check, message) = refused(setup(&config, Overrides::default()), &vars(&db)).await;
    assert_eq!(check, "tool servers", "{message}");
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_the_gate_does_not_require_stops_startup() {
    let db = TestDb::create().await;
    let gates = "schema_version: \"0.1\"\nsdr_commit:\n  required_checks: [schema, units]\n";
    let config = TestConfig::new(&[("policies/gates.yaml", gates)]);
    let (check, message) = refused(setup(&config, fake_tools()), &vars(&db)).await;
    assert_eq!(check, "checks", "{message}");
    assert!(
        message.contains("requirement.numeric_threshold"),
        "{message}"
    );
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_model_key_stops_startup_naming_it() {
    let db = TestDb::create().await;
    let config = TestConfig::new(&[]);
    let (check, message) = refused(setup(&config, fake_tools()), &vars(&db)).await;
    assert_eq!(check, "models", "{message}");
    assert!(message.contains("ANTHROPIC_API_KEY"), "{message}");
    db.drop().await;
}
