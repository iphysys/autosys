//! Shared fixtures for the system tests: workers built by bootstrap from a
//! copy of the committed `config/` and `prompts/`, on a throwaway Postgres
//! database, with a scripted model and fake build123d tools.
//!
//! Tests here get write authority only from `testkit::leases`, and never
//! name the SDR repository trait. Test requirements are fixtures written
//! here, never read from `evals/`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    dead_code,
    reason = "test support: fixtures fail loudly, and not every test uses every helper"
)]

pub mod db;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bootstrap::{CancellationToken, Overrides, Worker, WorkerSetup, build_worker};
use contracts::ids::{Identifier, JobId, ProjectId, RunId, SdrVersion};
use contracts::types::{
    Event, EventPayload, Job, JobState, TokenUsage, ToolResultContent, Transition,
};
use contracts::{
    ContentBlock, Error, ErrorCode, ExecutionContext, ModelProvider, ModelRequest, ModelResponse,
    ModelStopReason, Tool, ToolOutput, job_state,
};
use sdr::{CommitInfo, InitialVersion, Validation};
use serde_json::{Value, json};
use testkit::builders::{id, parse};
use testkit::fakes::FakeTool;
use testkit::leases::{fenced_ctx, job_submit, project_create, unfenced_ctx};
use tools::ServerTools;

use self::db::TestDb;

pub const PROJECT: &str = "p1";
/// How long a test waits for a worker to reach a state.
pub const WAIT: Duration = Duration::from_secs(30);

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

// ---------------------------------------------------------------- config ---

/// Intervals short enough for a test, allowed only in the test environment.
pub const TEST_WORKER_YAML: &str = "\
schema_version: \"0.1\"
lease_ms: 3000
heartbeat_ms: 500
poll_interval_ms: 50
poll_jitter_ms: 0
max_attempts: 3
retry_backoff_min_ms: 100
retry_backoff_max_ms: 400
database:
  max_connections: 8
  acquire_timeout_ms: 5000
  operation_timeout_ms: 10000
tool_servers:
  - server_id: build123d
    url: http://127.0.0.1:9/mcp
    connect_timeout_ms: 1000
    call_timeout_ms: 10000
";

/// An autonomy policy that asks for nothing.
pub const NO_APPROVALS_YAML: &str = "schema_version: \"0.1\"\napprovals: []\n";

/// A copy of the committed `config/`, with the test worker settings and any
/// other file replaced.
pub struct TestConfig {
    dir: tempfile::TempDir,
}

impl TestConfig {
    /// The committed config with [`TEST_WORKER_YAML`] and `replaced`, as
    /// (path under `config/`, text).
    pub fn new(replaced: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        copy_dir(&repo_root().join("config"), dir.path());
        fs::write(dir.path().join("worker.yaml"), TEST_WORKER_YAML).expect("write worker.yaml");
        for (path, text) in replaced {
            fs::write(dir.path().join(path), text).expect("write a config file");
        }
        Self { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create a config directory");
    for entry in fs::read_dir(from).expect("read config/") {
        let entry = entry.expect("a config entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy a config file");
        }
    }
}

/// The variables a test worker sees: never the process environment.
pub fn vars(db: &TestDb) -> HashMap<String, String> {
    HashMap::from([
        ("DATABASE_URL".to_owned(), db.url().to_owned()),
        ("SYNTH_ENVIRONMENT".to_owned(), "test".to_owned()),
        ("SYNTH_MODEL_PROFILE".to_owned(), "haiku".to_owned()),
    ])
}

// ----------------------------------------------------------------- model ---

/// One scripted model step. It sees the request, so it can answer from the
/// tool results in it.
pub type ScriptStep = Box<dyn Fn(&ModelRequest) -> Result<ModelResponse, Error> + Send + Sync>;

/// Answers from a script and records every request. Running out of script
/// is an `Internal` error.
pub struct ScriptedModel {
    id: Identifier,
    script: Mutex<VecDeque<ScriptStep>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedModel {
    pub fn new(script: Vec<ScriptStep>) -> Self {
        Self {
            id: id("scripted"),
            script: Mutex::new(script.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

#[async_trait]
impl ModelProvider for ScriptedModel {
    fn provider_id(&self) -> &Identifier {
        &self.id
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ModelResponse, Error> {
        ctx.ensure_not_cancelled()?;
        self.requests.lock().unwrap().push(request.clone());
        let step = self.script.lock().unwrap().pop_front();
        match step {
            Some(step) => step(request),
            None => Err(Error::new(ErrorCode::Internal, "model script exhausted")),
        }
    }
}

fn response(content: Vec<ContentBlock>, stop_reason: ModelStopReason) -> ModelResponse {
    ModelResponse {
        model_id: "claude-haiku-5-5".to_owned(),
        content,
        stop_reason,
        usage: TokenUsage {
            input_tokens: 1000,
            output_tokens: 100,
        },
    }
}

pub fn tool_use(calls: &[(&str, &str, Value)]) -> ModelResponse {
    let content = calls
        .iter()
        .map(|(call_id, tool, arguments)| ContentBlock::ToolCall {
            id: (*call_id).to_owned(),
            name: id(tool),
            arguments: arguments
                .as_object()
                .cloned()
                .expect("arguments are an object"),
        })
        .collect();
    response(content, ModelStopReason::ToolUse)
}

/// A reply that asks for one tool call.
pub fn call_tool(call_id: &'static str, tool: &'static str, arguments: Value) -> ScriptStep {
    Box::new(move |_| Ok(tool_use(&[(call_id, tool, arguments.clone())])))
}

/// A step computed from the request.
pub fn from_request(
    f: impl Fn(&ModelRequest) -> ModelResponse + Send + Sync + 'static,
) -> ScriptStep {
    Box::new(move |request| Ok(f(request)))
}

/// Plate, measure, then a final proposal of the measured mass.
pub fn succeeding() -> Vec<ScriptStep> {
    vec![
        call_tool("c1", "create_plate", plate()),
        call_tool("c2", "measure", measure_args()),
        from_request(|request| {
            let evidence = last_evidence(request);
            tool_use(&[("c3", "sdr_propose", propose_mass(MASS_G, &evidence, true))])
        }),
    ]
}

/// The evidence ids of the last `ok` tool result in `request`.
pub fn last_evidence(request: &ModelRequest) -> Vec<String> {
    let Some(last) = request.messages.last() else {
        return Vec::new();
    };
    last.content
        .iter()
        .rev()
        .find_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => match unwrap_envelope(content) {
                ToolResultContent::Ok { evidence_ids, .. } => {
                    Some(evidence_ids.iter().map(|e| e.as_str().to_owned()).collect())
                }
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_default()
}

fn unwrap_envelope(envelope: &str) -> ToolResultContent {
    let start = envelope.find('>').expect("an envelope opens") + 1;
    let end = envelope.rfind("</untrusted>").expect("an envelope closes");
    let text = envelope[start..end]
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("not a tool result: {e}: {text}"))
}

// ----------------------------------------------------------------- tools ---

pub fn plate() -> Value {
    json!({
        "length_mm": 110,
        "width_mm": 70,
        "thickness_mm": 6,
        "hole_diameter_mm": 6.5,
        "hole_inset_mm": 10
    })
}

pub fn plate_result() -> Value {
    json!({
        "part": {
            "part_id": "sha256:c184e3b64442c589f4fee88c0f62ec6e182cf4b5f0eed59e90cb693259dbf706",
            "plate": plate()
        }
    })
}

pub fn measure_args() -> Value {
    json!({ "part": plate_result()["part"], "density": { "value": 2.70, "unit": "g/cm3" } })
}

/// The mass `measure` reports for [`plate`], in g.
pub const MASS_G: f64 = 122.58973690825044;

pub fn measure_result() -> Value {
    json!({
        "part_id": "sha256:c184e3b64442c589f4fee88c0f62ec6e182cf4b5f0eed59e90cb693259dbf706",
        "volume": { "value": 45403.60626231498, "unit": "mm3" },
        "mass": { "value": MASS_G, "unit": "g" },
        "bounding_box": {
            "length": { "value": 110, "unit": "mm" },
            "width": { "value": 70, "unit": "mm" },
            "height": { "value": 6, "unit": "mm" }
        }
    })
}

pub fn returning(name: &str, result: Value) -> FakeTool {
    FakeTool::new(name, move |_| {
        Ok(ToolOutput {
            structured: result.clone(),
        })
    })
}

/// The build123d server with these two tools.
pub fn build123d(create_plate: Arc<FakeTool>, measure: Arc<FakeTool>) -> Vec<ServerTools> {
    vec![ServerTools {
        server_id: id("build123d"),
        tools: vec![create_plate as Arc<dyn Tool>, measure as Arc<dyn Tool>],
    }]
}

/// `sdr_propose` arguments that set `part.mass`, citing `evidence`.
pub fn propose_mass(mass_g: f64, evidence: &[String], is_final: bool) -> Value {
    let source = match evidence.first() {
        Some(e) => json!({ "kind": "tool_result", "evidence_id": e }),
        None => json!({ "kind": "user" }),
    };
    json!({
        "reason": "Record the measured mass of the plate.",
        "final": is_final,
        "operations": [{
            "op": "set_parameter",
            "parameter": {
                "id": "part.mass",
                "value": { "value": mass_g, "unit": "g" },
                "source": source,
                "evidence_ids": evidence
            }
        }]
    })
}

// ------------------------------------------------------------------ jobs ---

/// A queued job of `run`, for the orchestrator, needing R-mass, as a client
/// submits it.
pub fn job(job_id: &str, run: &str) -> Job {
    parse(json!({
        "schema_version": "0.2",
        "id": job_id,
        "run_id": run,
        "project_id": PROJECT,
        "status": { "state": "queued" },
        "task": {
            "schema_version": "0.1",
            "id": "task-1",
            "project_id": PROJECT,
            "agent_id": "main_orchestrator",
            "name": "Size the plate",
            "description": "Choose plate dimensions that meet the requirements.",
            "goal": "A committed design whose requirements all have passing checks.",
            "acceptance_criteria": ["Every requirement the task names has a passing check."],
            "requirement_ids": ["R-mass"],
            "skills": [],
            "tools": ["sdr_read", "sdr_propose", "create_plate", "measure"],
            "sdr": {
                "base_version": 1,
                "read_slice": { "kinds": ["requirement", "parameter", "decision", "assumption"], "ids": [] },
                "write_scope": { "kinds": ["parameter", "decision", "assumption"], "ids": [] }
            },
            "budget": {
                "max_tokens": 200000, "max_tool_calls": 20, "max_steps": 20,
                "max_wall_time_seconds": 600, "max_repair_attempts": 3
            },
            "result_schema": "results/result.schema.json"
        },
        "idempotency_key": job_id,
        "attempt": 0,
        "failed_attempts": 0,
        "attempt_history": [],
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "sdr_base_version": 1,
        "progress": { "steps_completed": 0, "last_event_seq": 0 },
        "pending_work": [],
        "artifact_ids": [],
        "budget_used": {
            "input_tokens": 0, "output_tokens": 0, "tool_calls": 0, "steps": 0,
            "wall_time_seconds": 0
        }
    }))
}

// ----------------------------------------------------------------- world ---

/// What one worker is built with.
pub struct Parts {
    pub model: Arc<ScriptedModel>,
    pub create_plate: Arc<FakeTool>,
    pub measure: Arc<FakeTool>,
    pub overrides: Overrides,
}

impl Parts {
    /// The script, the committed tool results, nothing else replaced.
    pub fn new(script: Vec<ScriptStep>) -> Self {
        Self {
            model: Arc::new(ScriptedModel::new(script)),
            create_plate: Arc::new(returning("create_plate", plate_result())),
            measure: Arc::new(returning("measure", measure_result())),
            overrides: Overrides::default(),
        }
    }

    pub fn create_plate(mut self, tool: FakeTool) -> Self {
        self.create_plate = Arc::new(tool);
        self
    }

    pub fn overrides(mut self, overrides: Overrides) -> Self {
        self.overrides = overrides;
        self
    }
}

/// Builds a worker named `name` on `db` with `config` and `parts`.
pub async fn build(name: &str, db: &TestDb, config: &TestConfig, parts: Parts) -> Arc<Worker> {
    let mut overrides = parts.overrides;
    overrides.models = Some(BTreeMap::from([(
        "haiku".to_owned(),
        parts.model as Arc<dyn ModelProvider>,
    )]));
    overrides.tool_servers = Some(build123d(parts.create_plate, parts.measure));
    let setup = WorkerSetup {
        config_dir: config.path().to_owned(),
        prompts_dir: repo_root().join("prompts"),
        worker_id: Some(name.to_owned()),
        overrides,
    };
    Arc::new(
        build_worker(setup, &vars(db))
            .await
            .unwrap_or_else(|e| panic!("worker {name} does not build: {e}")),
    )
}

/// A worker running in the background until [`Background::stop`].
pub struct Background {
    pub worker: Arc<Worker>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Background {
    pub fn start(worker: Arc<Worker>) -> Self {
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let worker = worker.clone();
            let shutdown = shutdown.clone();
            async move { worker.run(shutdown).await }
        });
        Self {
            worker,
            shutdown,
            task,
        }
    }

    /// Shuts the worker down, as a signal would, and waits for it.
    pub async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(WAIT, self.task)
            .await
            .expect("the worker stops in time")
            .expect("the worker task does not panic");
        tokio::time::timeout(WAIT, self.worker.drain())
            .await
            .expect("the worker's tasks end in time");
    }
}

/// Creates the project and commits R-mass (at most 150 g) as version 1,
/// under the fence of a seeding job that keeps its lease for the whole
/// test, so no worker claims it.
pub async fn seed(worker: &Worker) {
    let project: ProjectId = id(PROJECT);
    worker
        .sdr
        .create_project(
            &project,
            InitialVersion {
                transition_id: id("tr-init"),
                actor: parse(json!({ "kind": "system", "component": "test" })),
                reason: parse(json!("Start the design.")),
                harness: harness(),
            },
            &unfenced_ctx(project_create(), id("corr-seed")),
        )
        .await
        .expect("create the project");
    let mut seeder = job("job-seed", "run-seed");
    seeder.task.requirement_ids.clear();
    worker
        .jobs
        .insert(&seeder, Some(&job_submit()))
        .await
        .expect("insert the seeding job");
    let claimed = worker
        .jobs
        .claim(&id("seeder"), Duration::from_secs(3600))
        .await
        .expect("claim")
        .expect("the seeding job is claimable");
    assert_eq!(claimed.job.id.as_str(), "job-seed");
    let ctx = fenced_ctx(&claimed, id("corr-seed"));
    let transition: Transition = parse(json!({
        "schema_version": "0.1",
        "id": "tr-seed",
        "base_version": 0,
        "actor": { "kind": "user", "user_id": "test" },
        "reason": "The task's requirements.",
        "final": false,
        "operations": [{
            "op": "add_requirement",
            "requirement": {
                "id": "R-mass",
                "text": "The plate mass shall not exceed 150 g.",
                "priority": "must",
                "numeric": {
                    "parameter_id": "part.mass",
                    "criterion": { "comparator": "le", "limit": { "value": 150.0, "unit": "g" } }
                },
                "evidence_ids": []
            }
        }]
    }));
    let proposed = worker.sdr.propose(project, transition);
    let Validation::Valid(validated) = worker
        .sdr
        .validate(proposed, &ctx)
        .await
        .expect("validation runs")
    else {
        panic!("the seed requirements are invalid");
    };
    let record = worker
        .sdr
        .commit(
            validated,
            CommitInfo {
                evidence_ids: Vec::new(),
                harness: harness(),
            },
            &ctx,
        )
        .await
        .expect("commit the requirements");
    assert_eq!(record.version, SdrVersion(1));
}

pub fn harness() -> contracts::types::HarnessVersions {
    parse(json!({
        "harness_build": "test-seed",
        "model_ids": [],
        "prompts": [],
        "skills": [],
        "agent_specs": []
    }))
}

/// Waits until `job` is in `state`, or panics after [`WAIT`].
pub async fn wait_for(worker: &Worker, job: &str, state: JobState) -> Job {
    let job_id: JobId = id(job);
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let stored = worker.jobs.get(&job_id).await.expect("read the job");
        if job_state(&stored.status) == state {
            return stored;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{job} is still {:?}, not {state:?}",
            stored.status
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The project's head version.
pub async fn head(worker: &Worker) -> u64 {
    let ctx = ExecutionContext::builder(id("corr-read")).build();
    worker
        .sdr
        .head(&id::<ProjectId>(PROJECT), &ctx)
        .await
        .expect("head")
        .expect("the project exists")
        .0
}

/// Every event of `run`.
pub async fn logged(worker: &Worker, run: &str) -> Vec<Event> {
    worker
        .events
        .read(&id::<RunId>(run), 0, 10_000)
        .await
        .expect("read events")
}

/// `run`'s job_state_changed records, as (from, to).
pub async fn chain(worker: &Worker, run: &str) -> Vec<(Option<JobState>, JobState)> {
    logged(worker, run)
        .await
        .into_iter()
        .filter_map(|e| match e.payload {
            EventPayload::JobStateChanged { from, to, .. } => Some((from, job_state(&to))),
            _ => None,
        })
        .collect()
}
