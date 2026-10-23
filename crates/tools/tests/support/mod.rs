//! Shared set-up for the gateway tests: overlays, gates, in-memory stores.

#![allow(dead_code, reason = "each test binary uses part of this module")]

pub mod relay;

use std::sync::Arc;

use contracts::ids::{AgentId, RunId};
use contracts::types::{AutonomyPolicy, Event, EventPayload, ToolOverlay};
use contracts::{EventLog, ExecutionContext, GateSubjectKind, ToolRequest};
use governance::{ApprovalGate, GateRunner, ToolPermissionGate};
use serde_json::{Value, json};
use testkit::builders::{id, parse};
use testkit::clock::ManualClock;
use testkit::stores::{InMemoryEventLog, InMemoryEvidenceStore, InMemoryStepStore};
use tools::{ServerTools, ToolCall, ToolGateway, ToolRegistry};

/// How a test overlay lists one build123d tool.
#[derive(Clone)]
pub struct Listed {
    pub name: &'static str,
    pub idempotent: bool,
    pub concurrency_safe: bool,
    pub timeout_seconds: u64,
}

impl Listed {
    /// As `config/tools/build123d.yaml` lists it, but with a 1 s timeout so
    /// timeout tests stay short.
    pub fn tool(name: &'static str) -> Self {
        Self {
            name,
            idempotent: true,
            concurrency_safe: true,
            timeout_seconds: 1,
        }
    }

    pub fn not_idempotent(mut self) -> Self {
        self.idempotent = false;
        self
    }

    pub fn exclusive(mut self) -> Self {
        self.concurrency_safe = false;
        self
    }

    pub fn timeout(mut self, seconds: u64) -> Self {
        self.timeout_seconds = seconds;
        self
    }
}

pub fn overlay(listed: &[Listed]) -> ToolOverlay {
    let tools: Vec<Value> = listed
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "access": "read_only",
                "external_side_effects": false,
                "idempotent": t.idempotent,
                "concurrency_safe": t.concurrency_safe,
                "timeout_seconds": t.timeout_seconds,
                "requires_approval": false,
                "request_schema": format!("tools/build123d/{}_request.schema.json", t.name),
                "result_schema": format!("tools/build123d/{}_result.schema.json", t.name)
            })
        })
        .collect();
    parse(json!({
        "schema_version": "0.1",
        "server_id": "build123d",
        "description": "Test overlay for the build123d server.",
        "tools": tools
    }))
}

/// No approval rules, or one rule asking before every call to `tool`.
pub fn autonomy(ask_before: Option<&str>) -> AutonomyPolicy {
    let approvals: Vec<Value> = ask_before
        .map(|tool| {
            json!({
                "id": "approve_tool",
                "description": format!("Ask before calling {tool}."),
                "trigger": { "kind": "tool_call", "tool": tool }
            })
        })
        .into_iter()
        .collect();
    parse(json!({ "schema_version": "0.1", "approvals": approvals }))
}

pub struct Harness {
    pub gateway: ToolGateway,
    pub steps: Arc<InMemoryStepStore>,
    pub evidence: Arc<InMemoryEvidenceStore>,
    pub events: Arc<InMemoryEventLog>,
    pub clock: Arc<ManualClock>,
}

impl Harness {
    /// A gateway over `servers`, with the tool permission gate from
    /// `overlays` and the tool-call approval gate from `policy`.
    pub fn new(
        overlays: &[ToolOverlay],
        policy: &AutonomyPolicy,
        servers: Vec<ServerTools>,
    ) -> Self {
        let clock = Arc::new(ManualClock::at_epoch());
        let steps = Arc::new(InMemoryStepStore::new(clock.clone()));
        let evidence = Arc::new(InMemoryEvidenceStore::new());
        let events = Arc::new(InMemoryEventLog::new());
        let gates = GateRunner::new(
            vec![
                Arc::new(ToolPermissionGate::new(overlays).expect("permission gate")),
                Arc::new(
                    ApprovalGate::new(policy, GateSubjectKind::ToolCall).expect("approval gate"),
                ),
            ],
            events.clone(),
            clock.clone(),
        )
        .expect("gate runner");
        let registry = ToolRegistry::new(overlays, servers).expect("registry");
        let gateway = ToolGateway::new(
            registry,
            gates,
            steps.clone(),
            evidence.clone(),
            events.clone(),
            clock.clone(),
        );
        Self {
            gateway,
            steps,
            evidence,
            events,
            clock,
        }
    }

    pub async fn events(&self) -> Vec<Event> {
        self.events
            .read(&run_id(), 0, 1000)
            .await
            .expect("read events")
    }

    /// The event types in order, for example `["gate_decided", ...]`.
    pub async fn event_types(&self) -> Vec<&'static str> {
        self.events()
            .await
            .iter()
            .map(|e| kind(&e.payload))
            .collect()
    }
}

pub fn kind(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::GateDecided { .. } => "gate_decided",
        EventPayload::ApprovalRequested { .. } => "approval_requested",
        EventPayload::ToolCallStarted { .. } => "tool_call_started",
        EventPayload::ToolCallCompleted { .. } => "tool_call_completed",
        _ => "other",
    }
}

pub fn run_id() -> RunId {
    id("run-1")
}

pub fn agent() -> AgentId {
    id("main_orchestrator")
}

/// The context of one step of run `run-1`.
pub fn step_ctx(step: &str) -> ExecutionContext {
    ExecutionContext::new(id("corr-1"))
        .with_run(run_id())
        .for_step(id(step))
}

pub fn call<'a>(request: &'a ToolRequest, agent: &'a AgentId) -> ToolCall<'a> {
    ToolCall {
        request,
        agent_id: agent,
        sdr_version: 3.into(),
        approval: None,
    }
}

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

/// `measure` arguments for a part handle, with aluminium's density.
pub fn measure(part: &Value) -> Value {
    json!({ "part": part, "density": { "value": 2.70, "unit": "g/cm3" } })
}

/// The schema example result of `measure` for [`plate`].
pub fn measure_result() -> Value {
    json!({
        "part_id": "sha256:c184e3b64442c589f4fee88c0f62ec6e182cf4b5f0eed59e90cb693259dbf706",
        "volume": { "value": 45403.60626231498, "unit": "mm3" },
        "mass": { "value": 122.58973690825044, "unit": "g" },
        "bounding_box": {
            "length": { "value": 110, "unit": "mm" },
            "width": { "value": 70, "unit": "mm" },
            "height": { "value": 6, "unit": "mm" }
        }
    })
}
