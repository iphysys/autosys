//! Shared fixtures: a builder over in-memory stores, SDR and event helpers,
//! event logs that misbehave, and the golden-file comparison.
//!
//! Tests here may not name the SDR repository trait (arch-check scans test
//! files). A manager over faulty storage is built with testkit's
//! `faulty_sdr_storage` and handed to [`World::from_parts`].

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    dead_code,
    reason = "test support: fixtures fail loudly, and not every test binary uses every helper"
)]

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use context::{
    AssembledContext, BuildRequest, ContextBuilder, ContextError, ContextProfiles, PromptLibrary,
    Snapshot,
};
use contracts::ids::{ProjectId, RunId};
use contracts::types::{
    ContextBudgets, ContextPolicy, ContextProfile, Event, HarnessVersions, Job, Operation,
    Transition,
};
use contracts::{
    Clock, ContentBlock, Error, ErrorCode, EventKind, EventLog, ExecutionContext, Message,
    NewEvent, Role, WriteAuthority,
};
use sdr::{CommitInfo, InitialVersion, SdrManager, Validation};
use serde_json::{Value, json};
use testkit::builders::{id, new_event, parse};
use testkit::clock::ManualClock;
use testkit::leases::LeasedJob;
use testkit::stores::{InMemoryEventLog, fenced_sdr_storage};

pub const PROJECT: &str = "p1";
pub const RUN: &str = "run-1";

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

// ---------------------------------------------------------------- config ---

pub fn committed_config() -> (ContextBudgets, ContextPolicy) {
    let read = |name: &str| {
        fs::read_to_string(repo_root().join("config").join(name))
            .unwrap_or_else(|e| panic!("config/{name}: {e}"))
    };
    let budgets = serde_saphyr::from_str(&read("context.yaml")).expect("context.yaml parses");
    let policy = serde_saphyr::from_str(&read("policies/context.yaml"))
        .expect("policies/context.yaml parses");
    (budgets, policy)
}

/// The profiles of `config/context.yaml` under the committed ceiling.
pub fn profiles() -> ContextProfiles {
    let (budgets, policy) = committed_config();
    ContextProfiles::new(budgets, &policy).expect("the committed profiles are valid")
}

pub fn default_profile() -> ContextProfile {
    profiles().resolve(None).expect("a default profile").clone()
}

/// A profile JSON value with the given totals, for tests that need a small
/// or odd budget. Caps are the default profile's unless `sections` overrides them.
pub fn profile_json(name: &str, total: u64, sections: Value) -> Value {
    let mut merged = json!({
        "instructions": 3000,
        "task_contract": 1500,
        "job_state": 500,
        "design_state": 8000,
        "run_history": 3000,
        "untrusted_content": 6000,
        "open_turn": 6000
    });
    for (key, value) in sections.as_object().cloned().unwrap_or_default() {
        merged[key] = value;
    }
    json!({
        "name": name,
        "total_tokens": total,
        "bytes_per_token": 3,
        "history_window_events": 200,
        "io_timeout_seconds": 10,
        "sections": merged
    })
}

/// Profiles from JSON values, under a ceiling.
pub fn profiles_from(profiles: Vec<Value>, ceiling: u64) -> Result<ContextProfiles, ContextError> {
    let budgets: ContextBudgets = parse(json!({ "schema_version": "0.1", "profiles": profiles }));
    let policy: ContextPolicy =
        parse(json!({ "schema_version": "0.1", "max_total_tokens": ceiling }));
    ContextProfiles::new(budgets, &policy)
}

pub fn prompts() -> Arc<PromptLibrary> {
    Arc::new(
        PromptLibrary::load_dir(&repo_root().join("prompts")).expect("the repository prompts load"),
    )
}

/// Prompts like the repository's, with `system/base.md` replaced by `body`.
pub fn prompts_with_base(body: &str) -> Arc<PromptLibrary> {
    let real = prompts();
    let files = real
        .paths()
        .map(|path| {
            let prompt = real.get(path).expect("listed");
            let text = if path == "system/base.md" {
                body.to_owned()
            } else {
                prompt.body().to_owned()
            };
            (path.to_owned(), with_header(prompt.header(), &text))
        })
        .collect();
    Arc::new(PromptLibrary::from_files(files).expect("prompts build"))
}

pub fn with_header(header: &contracts::types::PromptHeader, body: &str) -> String {
    let inputs: Vec<String> = header.inputs.iter().map(|i| format!("  - {i}")).collect();
    format!(
        "---\nschema_version: \"0.1\"\nid: {}\nversion: {}\npurpose: {}\ninputs:\n{}\nowner: {}\norigin: {}\n---\n\n{body}\n",
        header.id.as_str(),
        header.version.as_str(),
        header.purpose.as_str(),
        inputs.join("\n"),
        header.owner.as_str(),
        header.origin,
    )
}

// -------------------------------------------------------------------- job ---

pub fn job_json(goal: &str) -> Value {
    json!({
        "schema_version": "0.2",
        "id": "job-1",
        "run_id": RUN,
        "project_id": PROJECT,
        "status": { "state": "running" },
        "task": {
            "schema_version": "0.1",
            "id": "task-1",
            "project_id": PROJECT,
            "agent_id": "orchestrator",
            "name": "Size the plate",
            "description": "Choose the plate thickness.",
            "goal": goal,
            "acceptance_criteria": ["The plate is at most 1 kg.", "Every must requirement has evidence."],
            "requirement_ids": ["R-001", "R-002"],
            "skills": [],
            "tools": ["sdr_read", "sdr_propose", "measure"],
            "sdr": {
                "base_version": 0,
                "read_slice": { "kinds": ["requirement", "parameter", "decision", "assumption"], "ids": [] },
                "write_scope": { "kinds": ["parameter", "decision", "assumption"], "ids": [] }
            },
            "budget": {
                "max_tokens": 200000,
                "max_tool_calls": 40,
                "max_steps": 30,
                "max_wall_time_seconds": 900,
                "max_repair_attempts": 3
            },
            "result_schema": "results/result.schema.json"
        },
        "idempotency_key": "job-1",
        "attempt": 0,
        "failed_attempts": 0,
        "attempt_history": [],
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "sdr_base_version": 0,
        "progress": { "steps_completed": 3, "last_event_seq": 0 },
        "pending_work": ["Set the plate thickness", "Check the mass"],
        "artifact_ids": [],
        "budget_used": {
            "input_tokens": 1200,
            "output_tokens": 300,
            "tool_calls": 2,
            "steps": 3,
            "wall_time_seconds": 41.5
        }
    })
}

pub fn job() -> Job {
    parse(job_json(
        "Size a mounting plate that meets its mass and envelope requirements.",
    ))
}

// -------------------------------------------------------------------- sdr ---

pub fn harness() -> HarnessVersions {
    parse(json!({
        "harness_build": "test-build",
        "model_ids": ["test-model"],
        "prompts": [],
        "skills": [],
        "agent_specs": []
    }))
}

pub fn op(value: Value) -> Operation {
    parse(value)
}

pub fn add_requirement(element_id: &str, text: &str, limit_kg: f64) -> Operation {
    op(json!({
        "op": "add_requirement",
        "requirement": {
            "id": element_id,
            "text": text,
            "priority": "must",
            "numeric": {
                "parameter_id": "part.mass",
                "criterion": { "comparator": "le", "limit": { "value": limit_kg, "unit": "kg" } }
            },
            "evidence_ids": []
        }
    }))
}

pub fn add_text_requirement(element_id: &str, text: &str) -> Operation {
    op(json!({
        "op": "add_requirement",
        "requirement": { "id": element_id, "text": text, "priority": "should", "evidence_ids": [] }
    }))
}

pub fn set_parameter(element_id: &str, value: f64, unit: &str) -> Operation {
    op(json!({
        "op": "set_parameter",
        "parameter": {
            "id": element_id,
            "value": { "value": value, "unit": unit },
            "source": { "kind": "user" },
            "evidence_ids": []
        }
    }))
}

pub fn add_decision(
    element_id: &str,
    choice: &str,
    rationale: &str,
    related: &[&str],
) -> Operation {
    op(json!({
        "op": "add_decision",
        "decision": { "id": element_id, "choice": choice, "rationale": rationale, "related_ids": related }
    }))
}

pub fn add_assumption(element_id: &str, statement: &str) -> Operation {
    op(json!({
        "op": "add_assumption",
        "assumption": { "id": element_id, "statement": statement, "related_ids": [] }
    }))
}

fn transition(transition_id: &str, base_version: u64, operations: Vec<Operation>) -> Transition {
    let mut transition: Transition = parse(json!({
        "schema_version": "0.1",
        "id": transition_id,
        "base_version": base_version,
        "actor": { "kind": "agent", "agent_id": "orchestrator" },
        "reason": "Change the design.",
        "final": false,
        "operations": [{ "op": "link_evidence", "element_id": "x", "evidence_id": "x" }]
    }));
    transition.operations = operations;
    transition
}

// ------------------------------------------------------------------ world ---

/// A context builder over in-memory stores, with a project that has version 0.
pub struct World {
    pub sdr: Arc<SdrManager>,
    pub events: Arc<InMemoryEventLog>,
    pub clock: Arc<ManualClock>,
    pub project: ProjectId,
    pub ctx: ExecutionContext,
    pub builder: ContextBuilder,
}

/// A context with no write authority, for builders over parts the test
/// controls: their stores refuse every write.
pub fn execution_context() -> ExecutionContext {
    ExecutionContext::new(id("corr-context"))
        .with_project(id(PROJECT))
        .with_run(id(RUN))
}

impl World {
    pub async fn new() -> Self {
        Self::with(profiles(), prompts()).await
    }

    pub async fn with(profiles: ContextProfiles, prompts: Arc<PromptLibrary>) -> Self {
        let clock = Arc::new(ManualClock::at_epoch());
        let leased = LeasedJob::new(clock.clone(), RUN);
        let sdr = Arc::new(
            SdrManager::new(fenced_sdr_storage(leased.leases()), clock.clone())
                .expect("the SDR schemas compile"),
        );
        let ctx = leased
            .ctx("corr-context")
            .with_project(id(PROJECT))
            .with_run(id(RUN));
        let project: ProjectId = id(PROJECT);
        sdr.create_project(
            &project,
            InitialVersion {
                transition_id: id("tr-init"),
                actor: parse(json!({ "kind": "system", "component": "test" })),
                reason: parse(json!("Start the design.")),
                harness: harness(),
            },
            &ctx,
        )
        .await
        .expect("create the project");
        let events = Arc::new(InMemoryEventLog::fenced_by(leased.leases()));
        let builder = ContextBuilder::new(
            sdr.clone(),
            events.clone(),
            prompts,
            profiles,
            clock.clone(),
        )
        .expect("the builder is configured");
        Self {
            sdr,
            events,
            clock,
            project,
            ctx,
            builder,
        }
    }

    /// A builder over this world's SDR and the given event log, for tests
    /// that wrap the log to make it fail or hang.
    pub fn builder_over(&self, events: Arc<dyn EventLog>) -> ContextBuilder {
        ContextBuilder::new(
            self.sdr.clone(),
            events,
            prompts(),
            profiles(),
            self.clock.clone(),
        )
        .expect("the builder is configured")
    }

    /// A builder over the given parts, for tests that need a broken SDR or
    /// event log. No project is created.
    pub fn from_parts(
        sdr: Arc<SdrManager>,
        events: Arc<dyn EventLog>,
        profiles: ContextProfiles,
        clock: Arc<ManualClock>,
    ) -> (ContextBuilder, ExecutionContext) {
        let builder = ContextBuilder::new(sdr, events, prompts(), profiles, clock)
            .expect("the builder is configured");
        (builder, execution_context())
    }

    /// Commits `operations` as the next version and returns its number.
    pub async fn commit(&self, operations: Vec<Operation>) -> u64 {
        let head = self
            .sdr
            .head(&self.project, &self.ctx)
            .await
            .expect("head")
            .expect("the project exists")
            .0;
        let proposed = self.sdr.propose(
            self.project.clone(),
            transition(&format!("tr-{}", head + 1), head, operations),
        );
        match self
            .sdr
            .validate(proposed, &self.ctx)
            .await
            .expect("validation runs")
        {
            Validation::Valid(validated) => {
                self.sdr
                    .commit(
                        validated,
                        CommitInfo {
                            evidence_ids: Vec::new(),
                            harness: harness(),
                        },
                        &self.ctx,
                    )
                    .await
                    .expect("commit")
                    .version
                    .0
            }
            Validation::Invalid(invalid) => panic!("the transition is invalid: {invalid:?}"),
        }
    }

    /// Appends an event to the run and returns its sequence number.
    pub async fn event(&self, payload: Value) -> u64 {
        self.events
            .append(
                new_event(RUN, self.clock.now(), payload),
                self.ctx.write_authority(),
            )
            .await
            .expect("append")
            .seq
            .get()
    }

    pub async fn build(
        &self,
        job: &Job,
        open_turn: &[Message],
    ) -> Result<AssembledContext, ContextError> {
        self.build_with(request(job, open_turn)).await
    }

    pub async fn build_with(
        &self,
        req: BuildRequest<'_>,
    ) -> Result<AssembledContext, ContextError> {
        self.builder.build(req, &self.ctx).await
    }

    /// Stored events of one type, oldest first.
    pub async fn events_of(&self, kind: &str) -> Vec<Event> {
        self.events
            .read(&id::<RunId>(RUN), 0, 100_000)
            .await
            .expect("read")
            .into_iter()
            .filter(|e| serde_json::to_value(&e.payload).expect("payload")["type"] == kind)
            .collect()
    }

    pub async fn all_events(&self) -> Vec<Event> {
        self.events
            .read(&id::<RunId>(RUN), 0, 100_000)
            .await
            .expect("read")
    }
}

pub fn request<'a>(job: &'a Job, open_turn: &'a [Message]) -> BuildRequest<'a> {
    BuildRequest {
        job,
        prompt: "orchestrator/main.md",
        profile: None,
        open_turn,
        reserved_tokens: 0,
        at: None,
    }
}

pub fn pinned(sdr_version: u64, last_event_seq: u64) -> Snapshot {
    Snapshot {
        sdr_version: contracts::ids::SdrVersion(sdr_version),
        last_event_seq,
    }
}

// ------------------------------------------------------------ open turns ---

pub fn assistant(blocks: Vec<ContentBlock>) -> Message {
    Message {
        role: Role::Assistant,
        content: blocks,
    }
}

pub fn user(blocks: Vec<ContentBlock>) -> Message {
    Message {
        role: Role::User,
        content: blocks,
    }
}

pub fn text(text: &str) -> ContentBlock {
    ContentBlock::Text { text: text.into() }
}

pub fn call(call_id: &str, tool: &str, arguments: Value) -> ContentBlock {
    ContentBlock::ToolCall {
        id: call_id.into(),
        name: id(tool),
        arguments: arguments
            .as_object()
            .cloned()
            .expect("arguments are an object"),
    }
}

pub fn result(call_id: &str, content: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        call_id: call_id.into(),
        content: content.into(),
        is_error: false,
    }
}

pub fn thinking(body: &str) -> ContentBlock {
    ContentBlock::Opaque {
        provider: id("anthropic"),
        data: json!({ "type": "thinking", "thinking": body, "signature": "sig-1" })
            .as_object()
            .cloned()
            .expect("object"),
    }
}

/// One tool round: the assistant calls `measure`, the tool answers.
pub fn tool_round(result_text: &str) -> Vec<Message> {
    vec![
        assistant(vec![
            thinking("The mass needs measuring."),
            text("Measuring the plate."),
            call(
                "call-1",
                "measure",
                json!({ "part": "plate", "unit": "kg" }),
            ),
        ]),
        user(vec![result("call-1", result_text)]),
    ]
}

// ------------------------------------------------------------ event logs ---

/// What one operation of [`FlakyEvents`] does.
#[derive(Clone, Copy, Debug)]
pub enum Behaviour {
    Pass,
    Fail(ErrorCode),
    Hang,
}

/// An event log over the in-memory one whose operations can fail or hang.
pub struct FlakyEvents {
    pub inner: Arc<InMemoryEventLog>,
    pub append: Behaviour,
    pub read: Behaviour,
    pub last_seq: Behaviour,
}

impl FlakyEvents {
    pub fn new(inner: Arc<InMemoryEventLog>) -> Self {
        Self {
            inner,
            append: Behaviour::Pass,
            read: Behaviour::Pass,
            last_seq: Behaviour::Pass,
        }
    }
}

async fn behave(behaviour: Behaviour) -> Result<(), Error> {
    match behaviour {
        Behaviour::Pass => Ok(()),
        Behaviour::Fail(code) => Err(Error::new(code, "injected event log fault")),
        Behaviour::Hang => std::future::pending().await,
    }
}

#[async_trait]
impl EventLog for FlakyEvents {
    async fn append(
        &self,
        event: NewEvent,
        authority: Option<&WriteAuthority>,
    ) -> Result<Event, Error> {
        behave(self.append).await?;
        self.inner.append(event, authority).await
    }

    async fn read(&self, run_id: &RunId, after: u64, limit: usize) -> Result<Vec<Event>, Error> {
        behave(self.read).await?;
        self.inner.read(run_id, after, limit).await
    }

    async fn read_kinds(
        &self,
        run_id: &RunId,
        after: u64,
        kinds: &[EventKind],
        limit: usize,
    ) -> Result<Vec<Event>, Error> {
        behave(self.read).await?;
        self.inner.read_kinds(run_id, after, kinds, limit).await
    }

    async fn last_seq(&self, run_id: &RunId) -> Result<u64, Error> {
        behave(self.last_seq).await?;
        self.inner.last_seq(run_id).await
    }

    async fn wait_for(
        &self,
        run_id: &RunId,
        after: u64,
        ctx: &ExecutionContext,
    ) -> Result<(), Error> {
        self.inner.wait_for(run_id, after, ctx).await
    }
}

// ----------------------------------------------------------------- golden ---

/// Control characters other than newline are written as escapes, so a golden
/// file never holds a raw carriage return that git could rewrite.
fn visible(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\n' || !c.is_control() {
            out.push(c);
        } else {
            write!(out, "\\u{:04x}", c as u32).expect("write to a string");
        }
    }
    out
}

/// A readable, deterministic rendering of what the model would see.
pub fn transcript(context: &AssembledContext) -> String {
    let mut out = String::new();
    writeln!(out, "=== system ===").unwrap();
    writeln!(out, "{}", visible(context.system())).unwrap();
    for (index, message) in context.messages().iter().enumerate() {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        writeln!(out, "\n=== message {index}: {role} ===").unwrap();
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    writeln!(out, "--- text ---\n{}", visible(text)).unwrap();
                }
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    writeln!(
                        out,
                        "--- tool_call id={id} name={} ---\n{}",
                        name.as_str(),
                        visible(&serde_json::to_string(arguments).unwrap())
                    )
                    .unwrap();
                }
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => {
                    writeln!(
                        out,
                        "--- tool_result call_id={call_id} is_error={is_error} ---\n{}",
                        visible(content)
                    )
                    .unwrap();
                }
                ContentBlock::Opaque { provider, data } => {
                    writeln!(
                        out,
                        "--- opaque provider={} ---\n{}",
                        provider.as_str(),
                        visible(&serde_json::to_string(data).unwrap())
                    )
                    .unwrap();
                }
            }
        }
    }
    out
}

/// The manifest with the hash and size, as pretty JSON. A struct, not
/// `json!`, so the key order is the declared one whatever serde_json's features.
pub fn manifest_json(context: &AssembledContext) -> String {
    #[derive(serde::Serialize)]
    struct Golden<'a> {
        context_hash: &'a str,
        total_tokens: u64,
        manifest: &'a context::Manifest,
    }
    let mut out = serde_json::to_string_pretty(&Golden {
        context_hash: context.hash().as_str(),
        total_tokens: context.total_tokens(),
        manifest: context.manifest(),
    })
    .expect("manifest serializes");
    out.push('\n');
    out
}

fn updating() -> bool {
    std::env::var_os("UPDATE_GOLDEN").is_some_and(|v| !v.is_empty())
}

fn compare(path: &Path, actual: &str) {
    if updating() {
        fs::create_dir_all(path.parent().expect("golden files have a parent")).expect("mkdir");
        fs::write(path, actual).expect("write the golden file");
        return;
    }
    let expected = fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "{}: cannot read the golden file ({e}); create it with UPDATE_GOLDEN=1 and review it",
            path.display()
        )
    });
    // The files never hold a raw carriage return; tolerate a checkout that added them.
    let expected = expected.replace("\r\n", "\n");
    if expected == actual {
        return;
    }
    let (line, want, got) = expected
        .lines()
        .zip(actual.lines())
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map(|(i, (a, b))| (i + 1, a.to_owned(), b.to_owned()))
        .unwrap_or_else(|| {
            (
                expected.lines().count().min(actual.lines().count()) + 1,
                "(end of one side)".to_owned(),
                "(end of one side)".to_owned(),
            )
        });
    panic!(
        "{} differs from the assembled context at line {line}:\n  golden: {want}\n  actual: {got}\n\
         If the change is intended, run UPDATE_GOLDEN=1 cargo test -p synth-context --test golden \
         and review the diff (blueprint section 16.3).",
        path.display()
    );
}

/// Compares the context with `tests/golden/context/<scenario>.*`. With
/// `UPDATE_GOLDEN=1` it writes the files instead; read each one before keeping it.
pub fn assert_golden(scenario: &str, context: &AssembledContext) {
    let dir = repo_root().join("tests/golden/context");
    compare(
        &dir.join(format!("{scenario}.context.txt")),
        &transcript(context),
    );
    compare(
        &dir.join(format!("{scenario}.manifest.json")),
        &manifest_json(context),
    );
}
