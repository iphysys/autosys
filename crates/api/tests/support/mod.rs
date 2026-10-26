//! The API on testkit's stores and fakes, driven without a network, and a
//! checker that holds every response to `schemas/api/openapi.yaml`.

#![allow(
    clippy::expect_used,
    dead_code,
    reason = "test support: fixtures fail loudly, and not every test uses every helper"
)]

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use api::{DevToken, Limits, Services};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::Response;
use contracts::ids::{CorrelationId, RunId};
use contracts::types::{Event, JobStatus};
use contracts::{Claimed, Clock, JobStore, JobWrite, RecordWrite, SchemaSet, WriteAuthority};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use testkit::builders::{agent_spec, completed, continuation, id, parse};
use testkit::clock::ManualClock;
use testkit::fakes::{FakeReports, FakeSdrViews, FakeSubmitter};
use testkit::leases::{LONG_LEASE, api_command};
use testkit::stores::{InMemoryArtifactStore, InMemoryEventLog, InMemoryJobStore, sdr_storage};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

pub const TOKEN: &str = "test-token-0123456789";
pub const AGENT: &str = "test_agent";

/// A router on in-memory stores, and the stores themselves.
pub struct World {
    pub router: Router,
    pub jobs: Arc<InMemoryJobStore>,
    pub events: Arc<InMemoryEventLog>,
    pub artifacts: Arc<InMemoryArtifactStore>,
    pub clock: Arc<ManualClock>,
    pub shutdown: CancellationToken,
}

impl World {
    pub fn new() -> Self {
        Self::with_limits(limits())
    }

    pub fn with_limits(limits: Limits) -> Self {
        let clock = Arc::new(ManualClock::at_epoch());
        let jobs = Arc::new(InMemoryJobStore::new(clock.clone()));
        let events = Arc::new(InMemoryEventLog::fenced_by(jobs.leases()));
        let artifacts = Arc::new(InMemoryArtifactStore::new());
        let report = parse(
            serde_json::from_str::<Value>(include_str!(
                "../../../../schemas/reports/examples/run-report.json"
            ))
            .expect("the report example is JSON"),
        );
        let shutdown = CancellationToken::new();
        let services = Services {
            jobs: jobs.clone(),
            events: events.clone(),
            artifacts: artifacts.clone(),
            submitter: Arc::new(FakeSubmitter::new(
                jobs.clone(),
                events.clone(),
                sdr_storage(),
                clock.clone(),
                vec![agent_spec(AGENT, None, &["sdr_read", "sdr_propose"])],
            )),
            reports: Arc::new(FakeReports::new(jobs.clone(), report)),
            sdr: Arc::new(FakeSdrViews::new(vec![parse(
                serde_json::from_str::<Value>(include_str!(
                    "../../../../schemas/api/examples/sdr-version.json"
                ))
                .expect("the view example is JSON"),
            )])),
            command: api_command(),
            actor: parse(json!({ "kind": "user", "user_id": "dev" })),
            clock: clock.clone(),
            token: DevToken::new(TOKEN.to_owned()).expect("a token"),
            limits,
        };
        let router = api::router(services, shutdown.clone()).expect("the router builds");
        Self {
            router,
            jobs,
            events,
            artifacts,
            clock,
            shutdown,
        }
    }

    /// Sends `request` through the router.
    pub async fn send(&self, request: Request<Body>) -> Response {
        self.router
            .clone()
            .oneshot(request)
            .await
            .expect("the router never fails")
    }

    /// Starts a run with `key` and returns its id, checking the response.
    pub async fn start(&self, contract: &Contract, key: &str) -> RunId {
        let response = self.send(post("/v0/runs", Some(key), &run_request())).await;
        let checked = contract.check("createRun", response).await;
        assert_eq!(checked.status, StatusCode::CREATED, "{:?}", checked.json);
        id(checked.json["run_id"].as_str().expect("a run id"))
    }

    /// Claims the oldest queued job, as a worker would.
    pub async fn claim(&self) -> Claimed {
        let jobs: &dyn JobStore = self.jobs.as_ref();
        jobs.claim(&id("worker-1"), LONG_LEASE)
            .await
            .expect("claim")
            .expect("a job to claim")
    }

    /// Appends `payload` under `claimed`'s fence, as its worker would.
    pub async fn record(&self, claimed: &Claimed, payload: Value) -> Event {
        use contracts::EventLog;
        self.clock.advance(Duration::from_millis(10));
        let authority = WriteAuthority::Fenced(claimed.fence.clone());
        self.events
            .append(
                testkit::builders::new_event(
                    claimed.job.run_id.as_str(),
                    self.clock.now(),
                    payload,
                ),
                Some(&authority),
            )
            .await
            .expect("append")
    }

    /// Saves `claimed`'s job completed, so its run has stopped for good.
    pub async fn complete(&self, claimed: &Claimed) {
        let jobs: &dyn JobStore = self.jobs.as_ref();
        let mut job = claimed.job.clone();
        job.status = completed("success", 1);
        let record = continuation(claimed.job.run_id.as_str(), 1);
        jobs.save(
            &JobWrite {
                job: &job,
                continuation: RecordWrite::Replace(&record),
                retry_after: None,
            },
            &claimed.fence,
        )
        .await
        .expect("save the completed job");
        assert!(matches!(
            jobs.get(&job.id).await.expect("the job").status,
            JobStatus::Completed { .. }
        ));
    }
}

pub fn limits() -> Limits {
    Limits {
        request_timeout: Duration::from_secs(5),
        max_body_bytes: 64 * 1024,
        keep_alive: Duration::from_secs(15),
        page_size: 7,
    }
}

pub fn run_request() -> Value {
    json!({
        "schema_version": "0.1",
        "agent_id": AGENT,
        "name": "Size the plate",
        "description": "Choose plate dimensions that meet the requirements.",
        "goal": "A committed design whose requirements all have passing checks.",
        "acceptance_criteria": ["Every requirement has a passing check."],
        "requirements": [{
            "id": "R-mass",
            "text": "The plate mass shall not exceed 150 g.",
            "priority": "must",
            "numeric": {
                "parameter_id": "part.mass",
                "criterion": { "comparator": "le", "limit": { "value": 150, "unit": "g" } }
            },
            "evidence_ids": []
        }]
    })
}

/// A request with the token.
pub fn authorized(method: Method, uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
}

pub fn get(uri: &str) -> Request<Body> {
    authorized(Method::GET, uri)
        .body(Body::empty())
        .expect("a request")
}

/// A JSON POST, with an Idempotency-Key if `key` is given.
pub fn post(uri: &str, key: Option<&str>, body: &Value) -> Request<Body> {
    let mut builder =
        authorized(Method::POST, uri).header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        builder = builder.header("idempotency-key", key);
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("a request")
}

/// One frame of an event stream.
#[derive(Debug, Clone)]
pub struct Frame {
    pub id: u64,
    pub event: String,
    pub data: Value,
}

/// Reads frames from an event stream's body, one at a time; keep-alive
/// comments are skipped.
pub struct FrameReader {
    body: Body,
    pending: String,
}

impl FrameReader {
    pub fn new(body: Body) -> Self {
        Self {
            body,
            pending: String::new(),
        }
    }

    /// The next frame, or `None` once the stream ends.
    pub async fn next(&mut self) -> Option<Frame> {
        loop {
            if let Some(end) = self.pending.find("\n\n") {
                let block: String = self.pending.drain(..end + 2).collect();
                if let Some(frame) = parse_frame(&block) {
                    return Some(frame);
                }
                continue;
            }
            let frame = tokio::time::timeout(Duration::from_secs(10), self.body.frame())
                .await
                .expect("a frame within 10 s")?
                .expect("the body reads");
            if let Ok(data) = frame.into_data() {
                self.pending
                    .push_str(std::str::from_utf8(&data).expect("UTF-8"));
            }
        }
    }

    /// Every remaining frame, until the stream ends.
    pub async fn rest(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next().await {
            frames.push(frame);
        }
        frames
    }
}

fn parse_frame(block: &str) -> Option<Frame> {
    let mut id = None;
    let mut event = None;
    let mut data = String::new();
    for line in block.lines() {
        if let Some(v) = line.strip_prefix("id:") {
            id = Some(v.trim().parse().expect("a numeric id"));
        } else if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push_str(v.trim_start());
        }
    }
    Some(Frame {
        id: id?,
        event: event.expect("every frame names its event"),
        data: serde_json::from_str(&data).expect("frame data is JSON"),
    })
}

/// A checked response.
pub struct Checked {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// The JSON body, or Null when the body is not JSON.
    pub json: Value,
    pub bytes: Bytes,
    /// The frames of an event stream, read to its end.
    pub frames: Vec<Frame>,
}

/// `schemas/api/openapi.yaml`, and which (operation, status) pairs the
/// tests have exercised.
pub struct Contract {
    doc: Value,
    schemas: SchemaSet,
    exercised: Mutex<BTreeSet<(String, u16)>>,
}

impl Contract {
    pub fn load() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/api/openapi.yaml");
        let text = std::fs::read_to_string(&path).expect("openapi.yaml is readable");
        let doc: Value = serde_saphyr::from_str(&text).expect("openapi.yaml is YAML");
        let mut paths = BTreeSet::new();
        let contract = Self {
            doc,
            schemas: SchemaSet::compile(&[]).expect("an empty set"),
            exercised: Mutex::new(BTreeSet::new()),
        };
        for (_, _, op) in contract.operations() {
            for (_, response) in op["responses"].as_object().expect("responses") {
                let response = contract.resolve(response);
                for (_, media) in response["content"].as_object().into_iter().flatten() {
                    for key in ["schema", "x-data-schema"] {
                        if let Some(reference) = media[key]["$ref"].as_str() {
                            paths.insert(schema_path(reference));
                        }
                    }
                }
            }
        }
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        Self {
            schemas: SchemaSet::compile(&paths).expect("every body schema is in schemas/"),
            ..contract
        }
    }

    /// (path, method, operation) for every operation in the document.
    pub fn operations(&self) -> Vec<(String, String, Value)> {
        let mut found = Vec::new();
        for (path, item) in self.doc["paths"].as_object().expect("paths") {
            for (method, op) in item.as_object().expect("a path item") {
                found.push((path.clone(), method.clone(), op.clone()));
            }
        }
        found
    }

    fn operation(&self, operation_id: &str) -> Value {
        self.operations()
            .into_iter()
            .find(|(_, _, op)| op["operationId"] == operation_id)
            .map(|(_, _, op)| op)
            .unwrap_or_else(|| panic!("no operation {operation_id} in openapi.yaml"))
    }

    /// `value`, or what its local `$ref` points at.
    fn resolve(&self, value: &Value) -> Value {
        match value["$ref"].as_str() {
            Some(reference) if reference.starts_with("#/") => {
                let pointer = &reference[1..];
                self.doc
                    .pointer(pointer)
                    .unwrap_or_else(|| panic!("{reference} does not resolve"))
                    .clone()
            }
            _ => value.clone(),
        }
    }

    /// Checks `response` against `operation_id`'s documented responses: a
    /// documented status (or a 5xx under `default`), the documented media
    /// type, and a body that satisfies its schema. An event stream is read
    /// to its end, and every frame's data checked.
    pub async fn check(&self, operation_id: &str, response: Response) -> Checked {
        let op = self.operation(operation_id);
        let status = response.status();
        let documented = &op["responses"][status.as_str()];
        let spec = if documented.is_null() {
            assert!(
                status.is_server_error() && !op["responses"]["default"].is_null(),
                "{operation_id}: status {status} is not documented"
            );
            self.resolve(&op["responses"]["default"])
        } else {
            self.resolve(documented)
        };
        let headers = response.headers().clone();
        if status == StatusCode::UNAUTHORIZED {
            assert_eq!(
                headers.get(header::WWW_AUTHENTICATE).map(|v| v.as_bytes()),
                Some(&b"Bearer"[..]),
                "{operation_id}: a 401 names its scheme"
            );
        }
        assert!(
            headers.contains_key("x-correlation-id"),
            "{operation_id}: every response has a correlation id"
        );
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_owned();
        let content = spec["content"]
            .as_object()
            .unwrap_or_else(|| panic!("{operation_id} {status}: no documented content"));
        let (media, media_spec) = content
            .iter()
            .find(|(media, _)| *media == "*/*" || **media == content_type)
            .unwrap_or_else(|| {
                panic!("{operation_id} {status}: content type {content_type:?} is not documented")
            });
        let mut frames = Vec::new();
        let (bytes, json) = if media == "text/event-stream" {
            let mut reader = FrameReader::new(response.into_body());
            frames = reader.rest().await;
            let data = schema_path(
                media_spec["x-data-schema"]["$ref"]
                    .as_str()
                    .expect("a stream names its data schema"),
            );
            for frame in &frames {
                self.assert_valid(&data, &frame.data, operation_id);
                assert_eq!(
                    frame.data["seq"].as_u64(),
                    Some(frame.id),
                    "{operation_id}: a frame's id is its event's sequence number"
                );
                assert_eq!(frame.data["payload"]["type"], frame.event.as_str());
            }
            (Bytes::new(), Value::Null)
        } else {
            let bytes = response
                .into_body()
                .collect()
                .await
                .expect("the body reads")
                .to_bytes();
            let json = match media_spec["schema"]["$ref"].as_str() {
                Some(reference) => {
                    let json: Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                        panic!("{operation_id} {status}: the body is not JSON: {e}")
                    });
                    self.assert_valid(&schema_path(reference), &json, operation_id);
                    json
                }
                None => Value::Null,
            };
            (bytes, json)
        };
        self.exercised
            .lock()
            .expect("lock")
            .insert((operation_id.to_owned(), status.as_u16()));
        Checked {
            status,
            headers,
            json,
            bytes,
            frames,
        }
    }

    fn assert_valid(&self, schema: &str, value: &Value, operation_id: &str) {
        let problems = self
            .schemas
            .errors(schema, value)
            .expect("a compiled schema");
        assert!(
            problems.is_empty(),
            "{operation_id}: the body breaks {schema}: {problems:?}\n{value}"
        );
    }

    /// Every documented (operation, status) pair, `default` aside.
    pub fn documented(&self) -> BTreeSet<(String, u16)> {
        let mut all = BTreeSet::new();
        for (_, _, op) in self.operations() {
            let op_id = op["operationId"]
                .as_str()
                .expect("an operationId")
                .to_owned();
            for (status, _) in op["responses"].as_object().expect("responses") {
                if let Ok(code) = status.parse() {
                    all.insert((op_id.clone(), code));
                }
            }
        }
        all
    }

    pub fn exercised(&self) -> BTreeSet<(String, u16)> {
        self.exercised.lock().expect("lock").clone()
    }
}

/// A `$ref` relative to `schemas/api/` as a path relative to `schemas/`.
fn schema_path(reference: &str) -> String {
    let file = reference.split('#').next().unwrap_or(reference);
    let mut parts: Vec<&str> = vec!["api"];
    for part in file.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

pub fn correlation(response: &Checked) -> CorrelationId {
    id(response.headers["x-correlation-id"]
        .to_str()
        .expect("ASCII"))
}
