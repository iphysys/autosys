//! Shared fixtures for the runtime's tests: an orchestrator over in-memory
//! stores, the real checks and gates, the committed configuration and
//! prompts, a scripted model behind the model gateway, and fake build123d
//! tools behind the tool gateway.
//!
//! Tests here may not name the SDR repository trait (arch-check scans test
//! files); the `SdrManager` gets testkit's `fenced_sdr_storage()`. Every
//! store is fenced by one claimed job, and the run's context carries its
//! fence. Test requirements are fixtures written here, never read from
//! `evals/`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    dead_code,
    reason = "test support: fixtures fail loudly, and not every test binary uses every helper"
)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use context::{ContextBuilder, ContextProfiles, PromptLibrary};
use contracts::ids::{Identifier, ProjectId, RunId, SdrVersion};
use contracts::types::{
    AgentSpec, ApprovalAnswer, AutonomyPolicy, ContextBudgets, ContextPolicy, Event, EventPayload,
    GatesPolicy, HarnessVersions, Job, ModelRouting, RuntimeSettings, Task, TokenUsage,
    ToolOverlay, ToolResultContent, Transition,
};
use contracts::{
    Check, ContentBlock, Error, ErrorCode, EventLog, ExecutionContext, Gate, GateSubjectKind,
    Message, ModelProvider, ModelRequest, ModelResponse, ModelStopReason, Role, Tool, ToolOutput,
};
use evaluation::Evaluator;
use evaluation::checks::{NumericThresholdCheck, SchemaCheck, UnitsCheck};
use governance::{ApprovalGate, GateRunner, SdrCommitGate, ToolPermissionGate};
use model_gateway::{ModelGateway, ModelUsage, ProviderAdapter, ProviderCall, Routing};
use runtime::{
    AgentRegistry, Components, Continuation, Orchestrator, RunEvent, RunOutcome, RunStream,
    RuntimeError,
};
use sdr::{CommitInfo, InitialVersion, SdrManager, Validation};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use testkit::builders::{id, parse};
use testkit::clock::ManualClock;
use testkit::fakes::FakeTool;
use testkit::leases::{HOLDER, LONG_LEASE, LeasedJob};
use testkit::stores::{
    InMemoryEventLog, InMemoryEvidenceStore, InMemoryStepStore, fenced_sdr_storage,
};
use tools::{ServerTools, ToolGateway, ToolRegistry};

pub const AGENT: &str = "main_orchestrator";
pub const PROJECT: &str = "p1";
pub const RUN: &str = "run-1";

// ---------------------------------------------------------------- config ---

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A committed YAML file under `config/`, as its type.
pub fn committed<T: DeserializeOwned>(path: &str) -> T {
    let text = fs::read_to_string(repo_root().join("config").join(path))
        .unwrap_or_else(|e| panic!("config/{path}: {e}"));
    serde_saphyr::from_str(&text).unwrap_or_else(|e| panic!("config/{path}: {e}"))
}

/// `config/agents/main_orchestrator.yaml` as committed.
pub fn orchestrator_spec() -> AgentSpec {
    committed("agents/main_orchestrator.yaml")
}

/// `config/tools/build123d.yaml` as committed.
pub fn build123d_overlay() -> ToolOverlay {
    committed("tools/build123d.yaml")
}

/// The tool names the committed overlay lists.
pub fn overlay_tools() -> Vec<String> {
    build123d_overlay()
        .tools
        .iter()
        .map(|t| t.name.as_str().to_owned())
        .collect()
}

pub fn prompts() -> PromptLibrary {
    PromptLibrary::load_dir(&repo_root().join("prompts")).expect("the repository prompts load")
}

/// The system prompt every call must have: the base prompt, then the
/// orchestrator's.
pub fn expected_system() -> String {
    let library = prompts();
    let body = |path: &str| library.get(path).expect("prompt").body().to_owned();
    format!(
        "{}\n\n{}",
        body("system/base.md"),
        body("orchestrator/main.md")
    )
}

pub fn context_profiles() -> ContextProfiles {
    let budgets: ContextBudgets = committed("context.yaml");
    let policy: ContextPolicy = committed("policies/context.yaml");
    ContextProfiles::new(budgets, &policy).expect("the committed profiles are valid")
}

/// The committed autonomy policy: ask before a final commit.
pub fn committed_autonomy() -> AutonomyPolicy {
    committed("policies/autonomy.yaml")
}

/// No approval rules.
pub fn no_approvals() -> AutonomyPolicy {
    parse(json!({ "schema_version": "0.1", "approvals": [] }))
}

// ----------------------------------------------------------------- tasks ---

/// A task for the orchestrator. It may read the whole design and write
/// parameters, decisions and assumptions.
pub fn task_json(requirement_ids: &[&str], budget: Value) -> Value {
    json!({
        "schema_version": "0.1",
        "id": "task-1",
        "project_id": PROJECT,
        "agent_id": AGENT,
        "name": "Size the plate",
        "description": "Choose plate dimensions that meet the requirements.",
        "goal": "A committed design whose requirements all have passing checks.",
        "acceptance_criteria": ["Every requirement the task names has a passing check."],
        "requirement_ids": requirement_ids,
        "skills": [],
        "tools": ["sdr_read", "sdr_propose", "create_plate", "measure"],
        "sdr": {
            "base_version": 1,
            "read_slice": { "kinds": ["requirement", "parameter", "decision", "assumption"], "ids": [] },
            "write_scope": { "kinds": ["parameter", "decision", "assumption"], "ids": [] }
        },
        "budget": budget,
        "result_schema": "results/result.schema.json"
    })
}

/// A budget as JSON.
pub fn budget(
    max_tokens: u64,
    max_tool_calls: u64,
    max_steps: u64,
    max_wall_time_seconds: u64,
    max_repair_attempts: u64,
) -> Value {
    json!({
        "max_tokens": max_tokens,
        "max_tool_calls": max_tool_calls,
        "max_steps": max_steps,
        "max_wall_time_seconds": max_wall_time_seconds,
        "max_repair_attempts": max_repair_attempts
    })
}

/// Roomy everywhere, three repairs.
pub fn roomy_budget() -> Value {
    budget(200_000, 20, 20, 600, 3)
}

pub fn task() -> Task {
    parse(task_json(&["R-mass"], roomy_budget()))
}

/// A running job of run `run-1` for `task`, with nothing used yet.
pub fn job_for(task: Value) -> Job {
    parse(json!({
        "schema_version": "0.2",
        "id": "job-1",
        "run_id": RUN,
        "project_id": PROJECT,
        "status": { "state": "running" },
        "task": task,
        "idempotency_key": "job-1",
        "attempt": 1,
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

// ---------------------------------------------------------- requirements ---

/// `add_requirement`: `id` is must-level and needs `part.mass` at most
/// `limit_g` grams.
pub fn mass_requirement(id: &str, limit_g: f64) -> Value {
    json!({
        "op": "add_requirement",
        "requirement": {
            "id": id,
            "text": format!("The plate mass shall not exceed {limit_g} g."),
            "priority": "must",
            "numeric": {
                "parameter_id": "part.mass",
                "criterion": { "comparator": "le", "limit": { "value": limit_g, "unit": "g" } }
            },
            "evidence_ids": []
        }
    })
}

/// `add_requirement` with free text only.
pub fn text_requirement(id: &str, priority: &str, text: &str) -> Value {
    json!({
        "op": "add_requirement",
        "requirement": { "id": id, "text": text, "priority": priority, "evidence_ids": [] }
    })
}

// ----------------------------------------------------------------- model ---

/// One scripted model step. It sees the request, so it can answer from the
/// tool results in it.
pub type ScriptStep = Box<dyn Fn(&ModelRequest) -> Result<ModelResponse, Error> + Send + Sync>;

/// A provider adapter that answers from a script and records every request.
/// Running out of script is an `Internal` error.
pub struct ScriptedAdapter {
    id: Identifier,
    script: Mutex<VecDeque<ScriptStep>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedAdapter {
    pub fn new(script: Vec<ScriptStep>) -> Self {
        Self {
            id: id("scripted"),
            script: Mutex::new(script.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Every request received, in order.
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Script steps not used.
    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

#[async_trait]
impl ModelProvider for ScriptedAdapter {
    fn provider_id(&self) -> &Identifier {
        &self.id
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ModelResponse, Error> {
        self.call(request, ctx).await.map(|call| call.response)
    }
}

#[async_trait]
impl ProviderAdapter for ScriptedAdapter {
    async fn call(
        &self,
        request: &ModelRequest,
        ctx: &ExecutionContext,
    ) -> Result<ProviderCall, Error> {
        ctx.ensure_not_cancelled()?;
        self.requests.lock().unwrap().push(request.clone());
        let step = self.script.lock().unwrap().pop_front();
        let response = match step {
            Some(step) => step(request)?,
            None => return Err(Error::new(ErrorCode::Internal, "model script exhausted")),
        };
        Ok(ProviderCall {
            usage_by_model: vec![ModelUsage {
                model: response.model_id.clone(),
                usage: response.usage.clone(),
            }],
            response,
            attempts: 1,
            request_id: None,
        })
    }
}

pub const USAGE: TokenUsage = TokenUsage {
    input_tokens: 1000,
    output_tokens: 100,
};

fn response(content: Vec<ContentBlock>, stop_reason: ModelStopReason) -> ModelResponse {
    ModelResponse {
        model_id: "claude-haiku-5-5".to_owned(),
        content,
        stop_reason,
        usage: USAGE,
    }
}

/// A reply that ends the turn with text.
pub fn reply(text: &str) -> ScriptStep {
    let text = text.to_owned();
    Box::new(move |_| {
        Ok(response(
            vec![ContentBlock::Text { text: text.clone() }],
            ModelStopReason::EndTurn,
        ))
    })
}

/// A reply that asks for tool calls, each `(call id, tool, arguments)`.
pub fn call_tools(calls: Vec<(&str, &str, Value)>) -> ScriptStep {
    let calls: Vec<(String, String, Value)> = calls
        .into_iter()
        .map(|(c, t, a)| (c.to_owned(), t.to_owned(), a))
        .collect();
    Box::new(move |_| Ok(tool_use(&calls)))
}

/// A step computed from the request.
pub fn from_request(
    f: impl Fn(&ModelRequest) -> ModelResponse + Send + Sync + 'static,
) -> ScriptStep {
    Box::new(move |request| Ok(f(request)))
}

pub fn tool_use(calls: &[(String, String, Value)]) -> ModelResponse {
    let content = calls
        .iter()
        .map(|(call_id, tool, arguments)| ContentBlock::ToolCall {
            id: call_id.clone(),
            name: id(tool),
            arguments: arguments
                .as_object()
                .cloned()
                .expect("arguments are an object"),
        })
        .collect();
    response(content, ModelStopReason::ToolUse)
}

/// The same response with other usage.
pub fn with_usage(step: ScriptStep, input_tokens: u64, output_tokens: u64) -> ScriptStep {
    Box::new(move |request| {
        let mut response = step(request)?;
        response.usage = TokenUsage {
            input_tokens,
            output_tokens,
        };
        Ok(response)
    })
}

/// A tool result of a request: the call it answers, whether it is an
/// error, and its content as the tool result schema describes it.
#[derive(Clone, Debug)]
pub struct SeenResult {
    pub call_id: String,
    pub is_error: bool,
    pub envelope: String,
    pub content: ToolResultContent,
}

/// The tool results in the last message of `request`, unwrapped from their
/// untrusted envelopes.
pub fn last_results(request: &ModelRequest) -> Vec<SeenResult> {
    let Some(last) = request.messages.last() else {
        return Vec::new();
    };
    last.content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => Some(SeenResult {
                call_id: call_id.clone(),
                is_error: *is_error,
                envelope: content.clone(),
                content: unwrap_envelope(content),
            }),
            _ => None,
        })
        .collect()
}

fn unwrap_envelope(envelope: &str) -> ToolResultContent {
    let start = envelope.find('>').expect("an envelope opens") + 1;
    let end = envelope.rfind("</untrusted>").expect("an envelope closes");
    let escaped = &envelope[start..end];
    let text = escaped
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("not a tool result: {e}: {text}"))
}

/// The evidence ids of the last `ok` result in `request`.
pub fn last_evidence(request: &ModelRequest) -> Vec<String> {
    last_results(request)
        .into_iter()
        .rev()
        .find_map(|seen| match seen.content {
            ToolResultContent::Ok { evidence_ids, .. } => {
                Some(evidence_ids.iter().map(|e| e.as_str().to_owned()).collect())
            }
            _ => None,
        })
        .unwrap_or_default()
}

// ----------------------------------------------------------------- tools ---

/// Arguments of the draft benchmark's plate.
pub fn plate() -> Value {
    json!({
        "length_mm": 110,
        "width_mm": 70,
        "thickness_mm": 6,
        "hole_diameter_mm": 6.5,
        "hole_inset_mm": 10
    })
}

/// The schema example result of `create_plate` for [`plate`].
pub fn plate_result() -> Value {
    json!({
        "part": {
            "part_id": "sha256:c184e3b64442c589f4fee88c0f62ec6e182cf4b5f0eed59e90cb693259dbf706",
            "plate": plate()
        }
    })
}

/// `measure` arguments for the plate, with aluminium's density.
pub fn measure_args() -> Value {
    json!({ "part": plate_result()["part"], "density": { "value": 2.70, "unit": "g/cm3" } })
}

/// The mass `measure` reports for [`plate`], in g.
pub const MASS_G: f64 = 122.58973690825044;

/// The schema example result of `measure` for [`plate`].
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

// ------------------------------------------------------------- proposals ---

/// `sdr_propose` arguments that set `part.mass` to `mass_g`, citing
/// `evidence` as the tool result it came from.
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

/// `sdr_propose` arguments that record a decision only.
pub fn propose_decision(is_final: bool) -> Value {
    json!({
        "reason": "Record the choice of stock.",
        "final": is_final,
        "operations": [{
            "op": "add_decision",
            "decision": {
                "id": "dec-stock",
                "choice": "6 mm aluminium plate.",
                "rationale": "The thinnest stock that keeps the holes clear of the edges.",
                "related_ids": ["R-mass"]
            }
        }]
    })
}

// ----------------------------------------------------------------- world ---

/// How a test world is set up.
pub struct Setup {
    /// `add_requirement` operations committed as version 1.
    pub requirements: Vec<Value>,
    pub autonomy: AutonomyPolicy,
    pub script: Vec<ScriptStep>,
    pub create_plate: FakeTool,
    pub measure: FakeTool,
    /// A profile and the adapter that answers for it instead of the script,
    /// for the live tests. The profile becomes the default.
    pub provider: Option<(String, Arc<dyn ProviderAdapter>)>,
    /// The tool overlay, the committed build123d one unless a test changes it.
    pub overlay: ToolOverlay,
    /// How long `worker-1`'s claim of the job lasts.
    pub lease: Duration,
}

impl Setup {
    /// R-mass at most 150 g, which the measured plate meets; no approvals.
    pub fn new(script: Vec<ScriptStep>) -> Self {
        Self {
            requirements: vec![mass_requirement("R-mass", 150.0)],
            autonomy: no_approvals(),
            script,
            create_plate: returning("create_plate", plate_result()),
            measure: returning("measure", measure_result()),
            provider: None,
            overlay: build123d_overlay(),
            lease: LONG_LEASE,
        }
    }

    pub fn overlay(mut self, overlay: ToolOverlay) -> Self {
        self.overlay = overlay;
        self
    }

    /// A lease short enough for the test to let it expire.
    pub fn lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    pub fn provider(mut self, profile: &str, provider: Arc<dyn ProviderAdapter>) -> Self {
        self.provider = Some((profile.to_owned(), provider));
        self
    }

    pub fn requirements(mut self, requirements: Vec<Value>) -> Self {
        self.requirements = requirements;
        self
    }

    pub fn autonomy(mut self, policy: AutonomyPolicy) -> Self {
        self.autonomy = policy;
        self
    }

    pub fn create_plate(mut self, tool: FakeTool) -> Self {
        self.create_plate = tool;
        self
    }

    pub fn measure(mut self, tool: FakeTool) -> Self {
        self.measure = tool;
        self
    }
}

pub struct World {
    pub orchestrator: Orchestrator,
    pub sdr: Arc<SdrManager>,
    pub events: Arc<InMemoryEventLog>,
    pub evidence: Arc<InMemoryEvidenceStore>,
    pub clock: Arc<ManualClock>,
    pub model: Arc<ScriptedAdapter>,
    pub create_plate: Arc<FakeTool>,
    pub measure: Arc<FakeTool>,
    /// The claimed job of run `run-1`, whose fence every store checks.
    pub leased: LeasedJob,
    pub tools: Arc<ToolGateway>,
    pub steps: Arc<InMemoryStepStore>,
}

pub fn harness() -> HarnessVersions {
    parse(json!({
        "harness_build": "test-seed",
        "model_ids": [],
        "prompts": [],
        "skills": [],
        "agent_specs": []
    }))
}

impl World {
    pub async fn new(setup: Setup) -> Self {
        Self::with_gates(setup, None).await
    }

    /// Like [`World::new`], with the commit gates given instead of the
    /// committed ones.
    pub async fn with_gates(setup: Setup, commit_gates: Option<Vec<Arc<dyn Gate>>>) -> Self {
        let clock = Arc::new(ManualClock::at_epoch());
        let leased = LeasedJob::with_lease(clock.clone(), RUN, &id(HOLDER), setup.lease);
        let events = Arc::new(InMemoryEventLog::fenced_by(leased.leases()));
        let evidence = Arc::new(InMemoryEvidenceStore::new());
        let steps = Arc::new(InMemoryStepStore::fenced_by(leased.leases()));
        let sdr = Arc::new(
            SdrManager::new(fenced_sdr_storage(leased.leases()), clock.clone())
                .expect("SDR schemas"),
        );
        seed(&sdr, setup.requirements, &run_ctx(&leased)).await;

        let overlay = setup.overlay;
        let tool_gates = GateRunner::new(
            vec![
                Arc::new(
                    ToolPermissionGate::new(std::slice::from_ref(&overlay))
                        .expect("permission gate"),
                ),
                Arc::new(
                    ApprovalGate::new(&setup.autonomy, GateSubjectKind::ToolCall)
                        .expect("tool approval gate"),
                ),
            ],
            events.clone(),
            clock.clone(),
        )
        .expect("tool gates");
        let create_plate = Arc::new(setup.create_plate);
        let measure = Arc::new(setup.measure);
        let registry = ToolRegistry::new(
            &[overlay],
            vec![ServerTools {
                server_id: id("build123d"),
                tools: vec![
                    create_plate.clone() as Arc<dyn Tool>,
                    measure.clone() as Arc<dyn Tool>,
                ],
            }],
        )
        .expect("tool registry");
        let available: Vec<String> = registry
            .available()
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let tools = Arc::new(ToolGateway::new(
            registry,
            tool_gates,
            steps.clone(),
            evidence.clone(),
            events.clone(),
            clock.clone(),
        ));

        let prompt_library = prompts();
        let names: Vec<&str> = available.iter().map(String::as_str).collect();
        let agents = Arc::new(
            AgentRegistry::new(vec![orchestrator_spec()], &prompt_library, &names)
                .expect("the committed agent registers"),
        );
        let context = Arc::new(
            ContextBuilder::new(
                sdr.clone(),
                events.clone(),
                Arc::new(prompt_library),
                context_profiles(),
                clock.clone(),
            )
            .expect("context builder"),
        );
        let checks: Vec<Arc<dyn Check>> = vec![
            Arc::new(SchemaCheck::new().expect("schema check")),
            Arc::new(UnitsCheck::new().expect("units check")),
            Arc::new(NumericThresholdCheck::new().expect("threshold check")),
        ];
        let evaluator = Arc::new(
            Evaluator::new(checks, evidence.clone(), events.clone(), clock.clone())
                .expect("evaluator"),
        );
        let gates_policy: GatesPolicy = committed("policies/gates.yaml");
        let commit_gates = commit_gates.unwrap_or_else(|| {
            vec![
                Arc::new(SdrCommitGate::new(&gates_policy).expect("commit gate")),
                Arc::new(
                    ApprovalGate::new(&setup.autonomy, GateSubjectKind::SdrCommit)
                        .expect("commit approval gate"),
                ),
            ]
        });

        let model = Arc::new(ScriptedAdapter::new(setup.script));
        let routing_config: ModelRouting = committed("models/routing.yaml");
        let routing = Routing::new(
            routing_config,
            Arc::new(model_gateway::providers::default_registry().expect("providers")),
            contracts::Environment::Test,
        )
        .expect("routing");
        let (profile, provider) = setup.provider.unwrap_or_else(|| {
            (
                "haiku".to_owned(),
                model.clone() as Arc<dyn ProviderAdapter>,
            )
        });
        let env = HashMap::from([(
            model_gateway::PROFILE_OVERRIDE_ENV.to_owned(),
            profile.clone(),
        )]);
        let providers: BTreeMap<String, Arc<dyn ProviderAdapter>> =
            BTreeMap::from([(profile, provider)]);
        let models = Arc::new(
            ModelGateway::with_providers(routing, &agents.specs(), &env, providers)
                .expect("model gateway"),
        );
        let settings: RuntimeSettings = committed("runtime.yaml");

        let orchestrator = Orchestrator::new(Components {
            agents,
            sdr: sdr.clone(),
            context,
            context_profiles: context_profiles(),
            tools: tools.clone(),
            evaluator,
            evidence: evidence.clone(),
            commit_gates,
            models,
            events: events.clone(),
            clock: clock.clone(),
            settings,
            harness_build: parse(json!("test-build")),
        })
        .expect("orchestrator");
        Self {
            orchestrator,
            sdr,
            events,
            evidence,
            clock,
            model,
            create_plate,
            measure,
            leased,
            tools,
            steps,
        }
    }

    /// The run's root context, fenced by the claimed job.
    pub fn ctx(&self) -> ExecutionContext {
        run_ctx(&self.leased)
    }

    pub async fn run(&self, job: Job) -> Ran {
        collect(self.orchestrator.run(job, self.ctx())).await
    }

    pub async fn run_with(&self, job: Job, ctx: ExecutionContext) -> Ran {
        collect(self.orchestrator.run(job, ctx)).await
    }

    pub async fn resume(&self, continuation: Continuation, answer: ApprovalAnswer) -> Ran {
        collect(self.orchestrator.resume(continuation, answer, self.ctx())).await
    }

    /// Every event of run `run-1` in the log.
    pub async fn logged(&self) -> Vec<Event> {
        self.events
            .read(&id::<RunId>(RUN), 0, 10_000)
            .await
            .expect("read events")
    }

    pub async fn head(&self) -> u64 {
        self.sdr
            .head(&id::<ProjectId>(PROJECT), &self.ctx())
            .await
            .expect("head")
            .expect("the project exists")
            .0
    }
}

/// The root context of a run fenced by `leased`'s claim.
pub fn run_ctx(leased: &LeasedJob) -> ExecutionContext {
    leased.ctx("corr-run")
}

/// Creates version 0, then commits `requirements` as version 1.
async fn seed(sdr: &SdrManager, requirements: Vec<Value>, ctx: &ExecutionContext) {
    let project: ProjectId = id(PROJECT);
    sdr.create_project(
        &project,
        InitialVersion {
            transition_id: id("tr-init"),
            actor: parse(json!({ "kind": "system", "component": "test" })),
            reason: parse(json!("Start the design.")),
            harness: harness(),
        },
        ctx,
    )
    .await
    .expect("create the project");
    let transition: Transition = parse(json!({
        "schema_version": "0.1",
        "id": "tr-seed",
        "base_version": 0,
        "actor": { "kind": "user", "user_id": "test" },
        "reason": "The task's requirements.",
        "final": false,
        "operations": requirements
    }));
    let proposed = sdr.propose(project, transition);
    let Validation::Valid(validated) = sdr.validate(proposed, ctx).await.expect("validation runs")
    else {
        panic!("the seed requirements are invalid");
    };
    let record = sdr
        .commit(
            validated,
            CommitInfo {
                evidence_ids: Vec::new(),
                harness: harness(),
            },
            ctx,
        )
        .await
        .expect("commit the requirements");
    assert_eq!(record.version, SdrVersion(1));
}

// ---------------------------------------------------------------- stream ---

/// What a run's stream yielded.
pub struct Ran {
    pub events: Vec<Event>,
    pub outcome: Result<RunOutcome, RuntimeError>,
}

impl Ran {
    pub fn outcome(&self) -> &RunOutcome {
        match &self.outcome {
            Ok(outcome) => outcome,
            Err(e) => panic!("the run failed: {e}"),
        }
    }

    /// The event types the stream yielded, in order.
    pub fn kinds(&self) -> Vec<&'static str> {
        self.events.iter().map(|e| kind(&e.payload)).collect()
    }

    pub fn count(&self, kind_name: &str) -> usize {
        self.kinds().iter().filter(|k| **k == kind_name).count()
    }

    /// The payloads of the `error` events, as (code, message).
    pub fn errors(&self) -> Vec<(String, String)> {
        self.events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Error { error } => Some((
                    error.code.as_str().to_owned(),
                    error.message.as_str().to_owned(),
                )),
                _ => None,
            })
            .collect()
    }

    /// `(transition, outcome, new version, reasons)` of each `sdr_transition`.
    pub fn transitions(&self) -> Vec<(String, String, Option<u64>, Vec<String>)> {
        self.events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::SdrTransition {
                    transition_id,
                    outcome,
                    new_version,
                    reasons,
                    ..
                } => Some((
                    transition_id.as_str().to_owned(),
                    outcome.to_string(),
                    new_version.as_ref().map(|v| v.0),
                    reasons.iter().map(|r| r.as_str().to_owned()).collect(),
                )),
                _ => None,
            })
            .collect()
    }
}

/// Drives `stream` to its end. It must end with exactly one `Stopped` or
/// one error, and nothing after it.
pub async fn collect(mut stream: RunStream<'_>) -> Ran {
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(RunEvent::Recorded(event)) => events.push(event),
            Ok(RunEvent::Stopped(outcome)) => {
                assert!(stream.next().await.is_none(), "an item after Stopped");
                return Ran {
                    events,
                    outcome: Ok(*outcome),
                };
            }
            Err(error) => {
                assert!(stream.next().await.is_none(), "an item after an error");
                return Ran {
                    events,
                    outcome: Err(error),
                };
            }
        }
    }
    panic!("the stream ended without Stopped or an error");
}

pub fn kind(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::RunStarted { .. } => "run_started",
        EventPayload::RunStopped { .. } => "run_stopped",
        EventPayload::JobStateChanged { .. } => "job_state_changed",
        EventPayload::ContextAssembled { .. } => "context_assembled",
        EventPayload::ModelCallCompleted { .. } => "model_call_completed",
        EventPayload::ToolCallStarted { .. } => "tool_call_started",
        EventPayload::ToolCallCompleted { .. } => "tool_call_completed",
        EventPayload::CheckCompleted { .. } => "check_completed",
        EventPayload::GateDecided { .. } => "gate_decided",
        EventPayload::ApprovalRequested { .. } => "approval_requested",
        EventPayload::ApprovalAnswered { .. } => "approval_answered",
        EventPayload::SdrTransition { .. } => "sdr_transition",
        EventPayload::ArtifactPublished { .. } => "artifact_published",
        EventPayload::CancelRequested { .. } => "cancel_requested",
        EventPayload::Error { .. } => "error",
    }
}

/// The text of the one user message every context starts with.
pub fn context_text(request: &ModelRequest) -> String {
    let first = request.messages.first().expect("a context message");
    assert_eq!(first.role, Role::User);
    first
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The roles of a request's messages.
pub fn roles(request: &ModelRequest) -> Vec<Role> {
    request.messages.iter().map(|m: &Message| m.role).collect()
}

/// A long wait, for tools that must still be running when something else
/// happens.
pub const SLOW: Duration = Duration::from_secs(120);
