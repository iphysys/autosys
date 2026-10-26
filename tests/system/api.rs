//! api-server on Postgres (ADR-0018), beside a worker built by bootstrap:
//! PR 14's done-when tests against the real stack. A client starts a run,
//! drops its event stream mid-run, answers the approval, reconnects with
//! its last id and receives every later event exactly once. Requests
//! without the token are refused, and the server does not start without
//! one.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bootstrap::{API_TOKEN_ENV, ApiSetup, BootstrapError, Worker, build_api};
use contracts::ExecutionContext;
use contracts::ids::RunId;
use contracts::types::{Event, RunReport};
use serde_json::{Value, json};
use testkit::builders::id;
use tokio_util::sync::CancellationToken;

use crate::support::db::TestDb;
use crate::support::*;

pub(crate) const TOKEN: &str = "system-test-token-0123456789abcdef";

/// api-server serving on a loopback port until stopped.
pub(crate) struct Server {
    pub(crate) base: String,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    /// On an ephemeral port.
    pub(crate) async fn start(db: &TestDb, config: &TestConfig) -> Self {
        Self::start_at(db, config, SocketAddr::from(([127, 0, 0, 1], 0))).await
    }

    /// On `address`, for example the port of a server stopped before.
    pub(crate) async fn start_at(db: &TestDb, config: &TestConfig, address: SocketAddr) -> Self {
        let shutdown = CancellationToken::new();
        let built = build_api(setup(config), &api_vars(db), shutdown.clone())
            .await
            .unwrap_or_else(|e| panic!("api-server does not build: {e}"));
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .expect("bind a loopback port");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        let task = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                api::serve(listener, built.router, shutdown)
                    .await
                    .expect("api-server serves");
            }
        });
        Self {
            base,
            shutdown,
            task,
        }
    }

    pub(crate) async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(WAIT, self.task)
            .await
            .expect("api-server stops in time")
            .expect("the server task does not panic");
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

fn setup(config: &TestConfig) -> ApiSetup {
    ApiSetup {
        config_dir: config.path().to_owned(),
        prompts_dir: repo_root().join("prompts"),
    }
}

fn api_vars(db: &TestDb) -> HashMap<String, String> {
    let mut vars = vars(db);
    vars.insert(API_TOKEN_ENV.to_owned(), TOKEN.to_owned());
    vars
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(WAIT)
        .build()
        .expect("an HTTP client")
}

/// A request with the token.
fn authorized(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    builder.bearer_auth(TOKEN)
}

pub(crate) fn brief() -> Value {
    json!({
        "schema_version": "0.1",
        "agent_id": "main_orchestrator",
        "name": "Size the plate",
        "description": "Choose plate dimensions that meet the requirements.",
        "goal": "A committed design whose requirements all have passing checks.",
        "acceptance_criteria": ["Every requirement the task names has a passing check."],
        "requirements": [{
            "id": "R-mass",
            "text": "The plate mass shall not exceed 150 g.",
            "priority": "must",
            "numeric": {
                "parameter_id": "part.mass",
                "criterion": { "comparator": "le", "limit": { "value": 150.0, "unit": "g" } }
            },
            "evidence_ids": []
        }]
    })
}

/// An open event stream, read frame by frame.
struct Stream {
    response: reqwest::Response,
    pending: String,
}

impl Stream {
    async fn open(server: &Server, run: &RunId, last_event_id: Option<u64>) -> Self {
        let mut request = authorized(
            reqwest::Client::new().get(server.url(&format!("/v0/runs/{}/events", run.as_str()))),
        );
        if let Some(last) = last_event_id {
            request = request.header("Last-Event-ID", last.to_string());
        }
        let response = request.send().await.expect("open the stream");
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["content-type"].to_str().expect("ASCII"),
            "text/event-stream"
        );
        Self {
            response,
            pending: String::new(),
        }
    }

    /// The next event, or `None` once the stream ends.
    async fn next(&mut self) -> Option<Event> {
        loop {
            if let Some(end) = self.pending.find("\n\n") {
                let block: String = self.pending.drain(..end + 2).collect();
                let mut id = None;
                let mut data = None;
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("id:") {
                        id = Some(v.trim().parse::<u64>().expect("a numeric id"));
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data = Some(v.trim_start().to_owned());
                    }
                }
                let (Some(id), Some(data)) = (id, data) else {
                    continue; // a keep-alive comment
                };
                let event: Event = serde_json::from_str(&data).expect("an event");
                assert_eq!(event.seq.get(), id, "a frame's id is its sequence number");
                return Some(event);
            }
            let chunk = tokio::time::timeout(WAIT, self.response.chunk())
                .await
                .expect("the stream moves within the wait")
                .expect("the stream reads")?;
            self.pending
                .push_str(std::str::from_utf8(&chunk).expect("UTF-8"));
        }
    }
}

async fn run_view(server: &Server, run: &RunId) -> Value {
    let response = authorized(client().get(server.url(&format!("/v0/runs/{}", run.as_str()))))
        .send()
        .await
        .expect("get the run");
    assert_eq!(response.status(), 200);
    response.json().await.expect("a run")
}

/// Polls the run through the API until its state is `state`.
async fn wait_for_state(server: &Server, run: &RunId, state: &str) -> Value {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let view = run_view(server, run).await;
        if view["status"]["state"] == state {
            return view;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run is still {}",
            view["status"]
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(crate) async fn stored_events(worker: &Worker, run: &RunId) -> Vec<Event> {
    worker.events.read(run, 0, 10_000).await.expect("the log")
}

/// The exit-criterion path through the API: start, stream, drop the stream
/// mid-run, approve, reconnect with the last id, read to the end. Every
/// event arrives exactly once, as stored; the report served is the one
/// the assembler builds from the same records.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_reconnects_receives_every_later_event_exactly_once() {
    let db = TestDb::create().await;
    let config = TestConfig::new(&[]);
    let worker = build("worker-a", &db, &config, Parts::policy()).await;
    let server = Server::start(&db, &config).await;

    let created = authorized(client().post(server.url("/v0/runs")))
        .header("Idempotency-Key", "system-run-1")
        .json(&brief())
        .send()
        .await
        .expect("create the run");
    assert_eq!(created.status(), 201);
    let created: Value = created.json().await.expect("a run");
    let run: RunId = id(created["run_id"].as_str().expect("a run id"));
    assert_eq!(created["status"]["state"], "queued");

    // The first connection reads the submit's record and the claim, then
    // drops while the run goes on.
    let mut first = Stream::open(&server, &run, None).await;
    let running = Background::start(Arc::clone(&worker));
    let mut seen: Vec<Event> = Vec::new();
    while seen.len() < 3 {
        seen.push(first.next().await.expect("the stream goes on"));
    }
    drop(first);

    let suspended = wait_for_state(&server, &run, "suspended").await;
    let approval = suspended["status"]["wake_condition"]["approval_id"]
        .as_str()
        .expect("the approval the run waits for")
        .to_owned();
    let answered = authorized(
        client().post(server.url(&format!("/v0/runs/{}/approvals/{approval}", run.as_str()))),
    )
    .json(&json!({ "answer": "approve" }))
    .send()
    .await
    .expect("answer the approval");
    assert_eq!(answered.status(), 202);
    let answered: Event = answered.json().await.expect("the stored answer");
    assert_eq!(
        serde_json::to_value(&answered.actor).expect("an actor"),
        json!({ "kind": "user", "user_id": "dev" })
    );

    let last = seen.last().expect("some events").seq.get();
    let mut second = Stream::open(&server, &run, Some(last)).await;
    while let Some(event) = second.next().await {
        seen.push(event);
    }
    let finished = run_view(&server, &run).await;
    assert_eq!(finished["status"]["state"], "completed", "{finished}");
    assert_eq!(finished["status"]["stop_reason"], "success", "{finished}");
    running.stop().await;

    let stored = stored_events(&worker, &run).await;
    let seqs: Vec<u64> = seen.iter().map(|e| e.seq.get()).collect();
    assert_eq!(
        seqs,
        (1..=stored.len() as u64).collect::<Vec<_>>(),
        "no gaps and no repeats"
    );
    assert_eq!(seen, stored, "every event as stored");
    assert!(seqs.len() > 6, "the drop came mid-run: {seqs:?}");

    let served = authorized(client().get(server.url(&format!("/v0/runs/{}/report", run.as_str()))))
        .send()
        .await
        .expect("get the report");
    assert_eq!(served.status(), 200);
    let served: RunReport = served.json().await.expect("a report");
    assert!(served.final_);
    let volume = tempfile::tempdir().expect("an artifact volume");
    let assembled = crate::report::assembler(&worker, &db, &volume)
        .await
        .assemble(&run, &ExecutionContext::new(id("corr-report")))
        .await
        .expect("the assembler's report");
    assert_eq!(served, assembled, "the API serves the assembler's report");

    server.stop().await;
    db.drop().await;
}

/// The real server refuses a request without the token, or with another.
#[tokio::test(flavor = "multi_thread")]
async fn requests_without_the_token_are_rejected() {
    let db = TestDb::create().await;
    let config = TestConfig::new(&[]);
    let server = Server::start(&db, &config).await;
    let url = server.url("/v0/runs/run-1");
    for request in [
        client().get(&url),
        client().get(&url).bearer_auth("not-the-token"),
        client()
            .post(server.url("/v0/runs"))
            .header("Idempotency-Key", "k")
            .json(&brief()),
    ] {
        let response = request.send().await.expect("a response");
        assert_eq!(response.status(), 401);
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        let body: Value = response.json().await.expect("an error body");
        assert_eq!(body["code"], "permission_denied");
    }
    let found = authorized(client().get(&url))
        .send()
        .await
        .expect("a response");
    assert_eq!(
        found.status(),
        404,
        "with the token, the run is just unknown"
    );
    server.stop().await;
    db.drop().await;
}

/// api-server does not start without its token.
#[tokio::test(flavor = "multi_thread")]
async fn the_server_does_not_start_without_a_token() {
    let db = TestDb::create().await;
    let config = TestConfig::new(&[]);
    for token in [None, Some("")] {
        let mut env = vars(&db);
        if let Some(token) = token {
            env.insert(API_TOKEN_ENV.to_owned(), token.to_owned());
        }
        let error = build_api(setup(&config), &env, CancellationToken::new())
            .await
            .err()
            .expect("no token, no server");
        assert!(
            matches!(
                &error,
                BootstrapError::Startup {
                    check: "api token",
                    ..
                }
            ),
            "{error}"
        );
        assert!(!error.to_string().contains(TOKEN));
    }
    db.drop().await;
}
