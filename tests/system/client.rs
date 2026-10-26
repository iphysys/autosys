//! The API client (ADR-0034) against api-server on Postgres, beside a
//! worker built by bootstrap: what the CLI and `cargo xtask eval` rely on.
//! A run is started once per key, streamed across a dropped connection and
//! a server restart without a gap or a repeat, approved, reported and
//! refused further commands; a queued run is cancelled; errors arrive as
//! the server's `ApiError`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use api_client::{Client, ClientConfig, ClientError, EventJournal, IdempotencyKey, Token};
use contracts::ids::{ApprovalId, ArtifactId, RunId};
use contracts::types::{
    ApprovalAnswer, ApprovalAnswerRequest, CancelRequest, Event, EventPayload, JobState, Run,
    RunRequest, StopReason,
};
use serde_json::Value;
use testkit::builders::id;

use crate::api::{Server, TOKEN, brief, stored_events};
use crate::support::db::TestDb;
use crate::support::*;

/// Quick retries, so a restarted server is found within the test's wait.
fn client_config() -> ClientConfig {
    ClientConfig {
        request_timeout: WAIT,
        stream_idle_timeout: WAIT,
        retries: 40,
        initial_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(250),
        ..ClientConfig::default()
    }
}

fn client_of(server: &Server) -> Client {
    Client::new(
        &server.base,
        Token::new(TOKEN.to_owned()).expect("a token"),
        client_config(),
    )
    .expect("a client")
}

fn request() -> RunRequest {
    serde_json::from_value(brief()).expect("the brief is a run request")
}

fn key(key: &str) -> IdempotencyKey {
    IdempotencyKey::new(key).expect("a key")
}

fn state(run: &Run) -> JobState {
    contracts::job_state(&run.status)
}

/// Polls the run through the client until its state is `wanted`.
async fn wait_for_state(client: &Client, run: &RunId, wanted: JobState) -> Run {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let view = client.run(run).await.expect("the run");
        if state(&view) == wanted {
            return view;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run is still {:?}",
            state(&view)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The approval a suspended run waits for.
fn awaited_approval(run: &Run) -> ApprovalId {
    let status = serde_json::to_value(&run.status).expect("a status");
    id(status["wake_condition"]["approval_id"]
        .as_str()
        .expect("a suspended run names its approval"))
}

fn api_status(error: &ClientError) -> Option<(u16, String)> {
    match error {
        ClientError::Api { status, body } => Some((*status, body.code.as_str().to_owned())),
        _ => None,
    }
}

/// The CLI's path through the client: start (twice, one run), stream into a
/// journal, drop the stream, resume from the journal, survive a server
/// restart while suspended, approve, read to the end, read the report;
/// then every command on the stopped run is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_is_driven_to_its_end_through_the_client() {
    let db = TestDb::create().await;
    let config = TestConfig::new(&[]);
    let worker = build("worker-a", &db, &config, Parts::policy()).await;
    let server = Server::start(&db, &config).await;
    let client = client_of(&server);

    let started = client
        .start_run(&request(), &key("client-run-1"))
        .await
        .expect("started");
    assert!(started.created);
    assert_eq!(state(&started.run), JobState::Queued);
    let run = started.run.run_id.clone();
    let again = client
        .start_run(&request(), &key("client-run-1"))
        .await
        .expect("started again");
    assert!(
        !again.created,
        "the same key and request return the same run"
    );
    assert_eq!(again.run.run_id, run);

    // The first stream fills the journal, then drops mid-run.
    let journal_dir = tempfile::tempdir().expect("a state directory");
    let journal_path = journal_dir.path().join("runs/events.jsonl");
    let mut journal = EventJournal::open(&journal_path).expect("a journal");
    let running = Background::start(Arc::clone(&worker));
    let mut first = client.events(&run, journal.last_seq());
    while journal.last_seq() < 3 {
        let event = first.next().await.expect("reads").expect("goes on");
        journal.append(&event).expect("journaled");
    }
    drop(first);
    drop(journal);

    // The second resumes from the reopened journal, in the background, and
    // keeps going through a server restart while the run is suspended.
    let mut journal = EventJournal::open(&journal_path).expect("reopened");
    assert_eq!(journal.last_seq(), 3);
    let reader = {
        let client = client.clone();
        let run = run.clone();
        tokio::spawn(async move {
            let mut stream = client.events(&run, journal.last_seq());
            while let Some(event) = stream.next().await.expect("the stream recovers") {
                journal.append(&event).expect("journaled");
            }
            journal.last_seq()
        })
    };
    let suspended = wait_for_state(&client, &run, JobState::Suspended).await;
    let address: SocketAddr = server
        .base
        .trim_start_matches("http://")
        .parse()
        .expect("a socket address");
    server.stop().await;
    let server = Server::start_at(&db, &config, address).await;

    let approval = awaited_approval(&suspended);
    let answered = client
        .answer_approval(
            &run,
            &approval,
            &ApprovalAnswerRequest {
                answer: ApprovalAnswer::Approve,
                comment: None,
            },
        )
        .await
        .expect("answered");
    assert!(matches!(
        answered.payload,
        EventPayload::ApprovalAnswered { .. }
    ));

    let last = tokio::time::timeout(WAIT, reader)
        .await
        .expect("the stream ends once the run has stopped")
        .expect("the reader does not panic");
    running.stop().await;
    let finished = client.run(&run).await.expect("the run");
    assert_eq!(state(&finished), JobState::Completed);
    assert_eq!(finished.last_seq, last);

    let stored = stored_events(&worker, &run).await;
    let journaled: Vec<Event> = std::fs::read_to_string(&journal_path)
        .expect("the journal")
        .lines()
        .map(|line| serde_json::from_str(line).expect("an event"))
        .collect();
    assert_eq!(journaled, stored, "every event once, as stored");

    let report = client.report(&run).await.expect("the report");
    assert!(report.final_);
    assert_eq!(report.stop_reason, StopReason::Success);

    let refused = client
        .answer_approval(
            &run,
            &approval,
            &ApprovalAnswerRequest {
                answer: ApprovalAnswer::Reject,
                comment: None,
            },
        )
        .await
        .expect_err("a stopped run takes no answer");
    assert_eq!(api_status(&refused).map(|s| s.0), Some(409), "{refused}");
    let refused = client
        .cancel(&run, &CancelRequest { reason: None })
        .await
        .expect_err("a stopped run cannot be cancelled");
    assert_eq!(api_status(&refused).map(|s| s.0), Some(409), "{refused}");

    server.stop().await;
    db.drop().await;
}

/// A queued run is cancelled through the client; the worker that claims it
/// stops it without running it, and its stream ends.
#[tokio::test(flavor = "multi_thread")]
async fn a_queued_run_is_cancelled_through_the_client() {
    let db = TestDb::create().await;
    let config = TestConfig::new(&[]);
    let worker = build("worker-a", &db, &config, Parts::policy()).await;
    let server = Server::start(&db, &config).await;
    let client = client_of(&server);

    let run = client
        .start_run(&request(), &key("client-cancel-1"))
        .await
        .expect("started")
        .run
        .run_id;
    let reason = "no longer needed".parse().ok();
    let cancelled = client
        .cancel(&run, &CancelRequest { reason })
        .await
        .expect("cancel accepted");
    assert!(matches!(
        cancelled.payload,
        EventPayload::CancelRequested { .. }
    ));

    let running = Background::start(Arc::clone(&worker));
    let mut stream = client.events(&run, 0);
    let mut seen = Vec::new();
    while let Some(event) = tokio::time::timeout(WAIT, stream.next())
        .await
        .expect("the stream moves")
        .expect("reads")
    {
        seen.push(event);
    }
    running.stop().await;
    assert_eq!(seen, stored_events(&worker, &run).await);
    assert_eq!(
        state(&client.run(&run).await.expect("the run")),
        JobState::Cancelled
    );

    server.stop().await;
    db.drop().await;
}

/// Refusals arrive as the server's `ApiError`, with its status and code.
#[tokio::test(flavor = "multi_thread")]
async fn refusals_carry_the_servers_error() {
    let db = TestDb::create().await;
    let config = TestConfig::new(&[]);
    let server = Server::start(&db, &config).await;
    let client = client_of(&server);

    let stranger = Client::new(
        &server.base,
        Token::new("not-the-token".to_owned()).expect("a token"),
        client_config(),
    )
    .expect("a client");
    let refused = stranger.run(&id("run-1")).await.expect_err("another token");
    assert_eq!(
        api_status(&refused),
        Some((401, "permission_denied".to_owned()))
    );

    let missing = client.run(&id("run-1")).await.expect_err("no such run");
    assert_eq!(api_status(&missing).map(|s| s.0), Some(404));
    let missing = client
        .artifact(&id::<ArtifactId>("artifact-1"))
        .await
        .expect_err("no such artifact");
    assert_eq!(api_status(&missing).map(|s| s.0), Some(404));

    let run = client
        .start_run(&request(), &key("client-errors-1"))
        .await
        .expect("started")
        .run
        .run_id;
    let early = client.report(&run).await.expect_err("not stopped yet");
    assert_eq!(api_status(&early).map(|s| s.0), Some(409), "{early}");

    let mut other: Value = brief();
    other["name"] = "Another run".into();
    let other: RunRequest = serde_json::from_value(other).expect("a request");
    let conflict = client
        .start_run(&other, &key("client-errors-1"))
        .await
        .expect_err("the same key for another request");
    assert_eq!(api_status(&conflict).map(|s| s.0), Some(409), "{conflict}");

    server.stop().await;
    db.drop().await;
}
